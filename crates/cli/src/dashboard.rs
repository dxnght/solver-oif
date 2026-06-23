//! Zero-dep live observability dashboard — a hand-rolled tokio HTTP/SSE server.
//!
//! The live shadow loop (`live.rs`) produces one [`DecisionRecord`] per intent.
//! This module exposes that **same** stream to a browser without
//! duplicating any decision logic (CLAUDE.md rule #5): the loop calls
//! [`DashboardHandle::publish`] alongside its JSONL write, never instead.
//!
//! No web framework, no build step (rule: B5 "zero-dep, vanilla"). We bind a
//! [`TcpListener`], speak just enough HTTP/1.1 by hand to answer two routes —
//! `GET /` (the embedded [`dashboard.html`](include_str!) page) and
//! `GET /events` (a `text/event-stream` of decisions) — and fan decisions out
//! over a [`broadcast`] channel. A small replay buffer of the last
//! [`REPLAY_CAP`] decisions is sent to each new client so a browser that
//! connects mid-run sees recent flow instead of a blank screen.
//!
//! Narrative is **flow / exclusivity**, not `would_fill` (edge ≈ 0). This
//! module only moves the records; the framing lives in the HTML.

use std::collections::VecDeque;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex};

use anyhow::Result;
use serde::Serialize;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::broadcast;
use tracing::{debug, warn};

/// The browser UI, embedded at compile time (no runtime file dependency).
const DASHBOARD_HTML: &str = include_str!("dashboard.html");

/// How many recent decisions a freshly-connected client is replayed, so it
/// never starts on an empty screen. Small: this is "recent flow", not history
/// (the durable record is the JSONL the loop also writes).
const REPLAY_CAP: usize = 256;

/// Broadcast backlog. A client slower than this many decisions behind is
/// `Lagged` and silently skips the gap rather than stalling the publisher —
/// acceptable because the live rate is low and JSONL is the source of truth.
const CHANNEL_CAP: usize = 1024;

/// Handle held by the live loop: publishes decisions to all connected clients.
///
/// Cloneable-cheap pieces; the accept loop runs in its own task spawned by
/// [`serve`]. Dropping the last handle closes the broadcast channel, which
/// ends each `/events` stream cleanly at shutdown.
#[derive(Clone)]
pub struct DashboardHandle {
    tx: broadcast::Sender<String>,
    replay: Arc<Mutex<VecDeque<String>>>,
    /// The actually-bound address (useful when `port = 0` picks an ephemeral
    /// one, e.g. in tests, and for the startup log line).
    pub addr: SocketAddr,
}

impl DashboardHandle {
    /// Push one decision to every connected client and the replay buffer.
    ///
    /// Serialization failure is dropped silently (the JSONL write in the loop
    /// is the durable path; we never panic the live loop over a UI hiccup).
    /// The replay push and the broadcast `send` happen under the **same** lock
    /// so a client that snapshots-then-subscribes (see [`serve_events`]) can
    /// neither miss a decision nor see it twice.
    pub fn publish<T: Serialize>(&self, record: &T) {
        let json = match serde_json::to_string(record) {
            Ok(j) => j,
            Err(e) => {
                debug!(error = %e, "dashboard: skipping unserializable record");
                return;
            }
        };
        let mut buf = self.replay.lock().expect("replay buffer poisoned");
        if buf.len() == REPLAY_CAP {
            buf.pop_front();
        }
        buf.push_back(json.clone());
        // Err only means "no subscribers right now" — fine, the record is in
        // the replay buffer for whoever connects next.
        let _ = self.tx.send(json);
    }
}

/// Bind the dashboard on `127.0.0.1:port` (loopback only — this is an
/// operator-local view, not a public listener), spawn the accept loop, and
/// return a handle. `port = 0` binds an ephemeral port (read it back via
/// [`DashboardHandle::addr`]).
pub async fn serve(port: u16) -> Result<DashboardHandle> {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, port)).await?;
    let addr = listener.local_addr()?;
    let (tx, _rx) = broadcast::channel::<String>(CHANNEL_CAP);
    let replay = Arc::new(Mutex::new(VecDeque::with_capacity(REPLAY_CAP)));

    let accept_tx = tx.clone();
    let accept_replay = Arc::clone(&replay);
    tokio::spawn(async move {
        loop {
            match listener.accept().await {
                Ok((stream, _peer)) => {
                    let tx = accept_tx.clone();
                    let replay = Arc::clone(&accept_replay);
                    tokio::spawn(async move {
                        if let Err(e) = handle_conn(stream, tx, replay).await {
                            debug!(error = %e, "dashboard: connection ended");
                        }
                    });
                }
                Err(e) => {
                    warn!(error = %e, "dashboard: accept failed — stopping listener");
                    break;
                }
            }
        }
    });

    Ok(DashboardHandle { tx, replay, addr })
}

/// Read the request line, route to the right handler. We only need the path,
/// so we read one chunk and parse the first line (`GET /events HTTP/1.1`);
/// any request body is ignored (we serve only GETs).
async fn handle_conn(
    mut stream: TcpStream,
    tx: broadcast::Sender<String>,
    replay: Arc<Mutex<VecDeque<String>>>,
) -> std::io::Result<()> {
    let path = match read_request_path(&mut stream).await? {
        Some(p) => p,
        None => return Ok(()), // empty/closed connection
    };
    // Strip any query string; routes are exact.
    let route = path.split('?').next().unwrap_or("/");
    match route {
        "/" => serve_html(&mut stream).await,
        "/events" => serve_events(stream, tx, replay).await,
        _ => serve_404(&mut stream).await,
    }
}

/// Parse the request-target from the HTTP request line. Returns `None` on an
/// immediately-closed connection.
async fn read_request_path(stream: &mut TcpStream) -> std::io::Result<Option<String>> {
    let mut buf = [0u8; 1024];
    let n = stream.read(&mut buf).await?;
    if n == 0 {
        return Ok(None);
    }
    let head = String::from_utf8_lossy(&buf[..n]);
    let line = head.lines().next().unwrap_or("");
    // "GET /events HTTP/1.1" → the middle whitespace-delimited token.
    Ok(line.split_whitespace().nth(1).map(str::to_string))
}

/// Serve the embedded dashboard page (`Connection: close` — one-shot).
async fn serve_html(stream: &mut TcpStream) -> std::io::Result<()> {
    let header = format!(
        "HTTP/1.1 200 OK\r\n\
         Content-Type: text/html; charset=utf-8\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\r\n",
        DASHBOARD_HTML.len()
    );
    stream.write_all(header.as_bytes()).await?;
    stream.write_all(DASHBOARD_HTML.as_bytes()).await?;
    stream.flush().await
}

/// Serve the SSE stream: replay the recent buffer, then forward every new
/// decision as it is published. Runs until the client disconnects (a write
/// fails) or the broadcast channel closes (live loop shut down).
async fn serve_events(
    mut stream: TcpStream,
    tx: broadcast::Sender<String>,
    replay: Arc<Mutex<VecDeque<String>>>,
) -> std::io::Result<()> {
    let header = "HTTP/1.1 200 OK\r\n\
         Content-Type: text/event-stream\r\n\
         Cache-Control: no-cache\r\n\
         Connection: keep-alive\r\n\
         Access-Control-Allow-Origin: *\r\n\r\n";
    stream.write_all(header.as_bytes()).await?;

    // Snapshot the replay buffer and subscribe under the SAME lock the
    // publisher holds: every already-published decision is in the snapshot,
    // and every later one arrives on `rx` — no gap, no duplicate.
    let (snapshot, mut rx) = {
        let buf = replay.lock().expect("replay buffer poisoned");
        let snap: Vec<String> = buf.iter().cloned().collect();
        (snap, tx.subscribe())
    };
    for json in snapshot {
        write_event(&mut stream, &json).await?;
    }
    stream.flush().await?;

    loop {
        match rx.recv().await {
            Ok(json) => {
                write_event(&mut stream, &json).await?;
                stream.flush().await?;
            }
            // Slow client: skip the gap rather than stall the publisher.
            Err(broadcast::error::RecvError::Lagged(_)) => continue,
            // Live loop ended → handle dropped → channel closed.
            Err(broadcast::error::RecvError::Closed) => break,
        }
    }
    Ok(())
}

/// Write one SSE `data:` event. `json` is a single line (serde emits no
/// newlines), so one `data:` field carries the whole record.
async fn write_event(stream: &mut TcpStream, json: &str) -> std::io::Result<()> {
    stream.write_all(b"data: ").await?;
    stream.write_all(json.as_bytes()).await?;
    stream.write_all(b"\n\n").await
}

/// Minimal 404 for any route other than `/` and `/events`.
async fn serve_404(stream: &mut TcpStream) -> std::io::Result<()> {
    let resp = "HTTP/1.1 404 Not Found\r\n\
         Content-Length: 0\r\n\
         Connection: close\r\n\r\n";
    stream.write_all(resp.as_bytes()).await?;
    stream.flush().await
}
