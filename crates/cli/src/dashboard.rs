//! Zero-dep live observability dashboard — a hand-rolled tokio HTTP/SSE server.
//!
//! The live shadow loop (`live.rs`) produces one [`DecisionRecord`] per intent.
//! This module exposes that **same** stream to a browser without
//! duplicating any decision logic (CLAUDE.md rule #5): the loop calls
//! [`DashboardHandle::publish`] alongside its JSONL write, never instead.
//!
//! No web framework, no build step (rule: B5 "zero-dep, vanilla"). We bind a
//! [`TcpListener`], speak just enough HTTP/1.1 by hand to answer a handful of
//! routes — `GET /` (the embedded [`dashboard.html`](include_str!) page),
//! `GET /about` (BFRAME-1 "what this is / is not" page), `GET /events` (a
//! `text/event-stream` of decisions), `GET /healthz` (BNET-1 liveness probe),
//! and `GET /aggregates` (BAGG-1 persisted 24h/7d rollups) — and fan decisions
//! out over a [`broadcast`] channel. A small replay buffer of the last
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

/// BFRAME-1: the "what this is / is not" page — embedded the same way, no
/// runtime dependency, so a reviewer can understand the dashboard's limits
/// from the deployed binary alone.
const ABOUT_HTML: &str = include_str!("about.html");

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
    /// BAGG-1: the last recomputed 24h/7d aggregates snapshot, pre-serialized
    /// JSON. `None` until the live loop's first [`Self::set_aggregates`] call
    /// (a client connecting in that brief window gets `null`, handled by the
    /// dashboard JS as "not loaded yet" — never a crash).
    aggregates: Arc<Mutex<Option<String>>>,
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

    /// BAGG-1: swap in a freshly recomputed aggregates snapshot, serialized by
    /// the caller (`crate::aggregates::compute` + `serde_json::to_string`).
    /// Replaces the whole snapshot atomically — `/aggregates` never serves a
    /// half-updated window.
    pub fn set_aggregates(&self, json: String) {
        *self.aggregates.lock().expect("aggregates poisoned") = Some(json);
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
    let aggregates = Arc::new(Mutex::new(None));

    let accept_tx = tx.clone();
    let accept_replay = Arc::clone(&replay);
    let accept_aggregates = Arc::clone(&aggregates);
    tokio::spawn(async move {
        loop {
            match listener.accept().await {
                Ok((stream, _peer)) => {
                    let tx = accept_tx.clone();
                    let replay = Arc::clone(&accept_replay);
                    let aggregates = Arc::clone(&accept_aggregates);
                    tokio::spawn(async move {
                        if let Err(e) = handle_conn(stream, tx, replay, aggregates).await {
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

    Ok(DashboardHandle { tx, replay, aggregates, addr })
}

/// Read the request line, route to the right handler. We only need the path,
/// so we read one chunk and parse the first line (`GET /events HTTP/1.1`);
/// any request body is ignored (we serve only GETs).
async fn handle_conn(
    mut stream: TcpStream,
    tx: broadcast::Sender<String>,
    replay: Arc<Mutex<VecDeque<String>>>,
    aggregates: Arc<Mutex<Option<String>>>,
) -> std::io::Result<()> {
    let path = match read_request_path(&mut stream).await? {
        Some(p) => p,
        None => return Ok(()), // empty/closed connection
    };
    // Strip any query string; routes are exact.
    let route = path.split('?').next().unwrap_or("/");
    match route {
        "/" => serve_html(&mut stream).await,
        "/about" => serve_about(&mut stream).await,
        "/events" => serve_events(stream, tx, replay).await,
        "/healthz" => serve_health(&mut stream).await,
        "/aggregates" => serve_aggregates(&mut stream, &aggregates).await,
        _ => serve_404(&mut stream).await,
    }
}

/// Liveness probe: a tiny `200 ok` so a reverse proxy / uptime monitor can
/// check the listener is up without fetching the whole page. "Up" here means
/// the HTTP server is answering; sparse intent flow (legitimately no decisions
/// for a while) is not unhealthy, so this deliberately does not gate on recent
/// activity.
async fn serve_health(stream: &mut TcpStream) -> std::io::Result<()> {
    const BODY: &str = "ok";
    let header = format!(
        "HTTP/1.1 200 OK\r\n\
         Content-Type: text/plain; charset=utf-8\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\r\n",
        BODY.len()
    );
    stream.write_all(header.as_bytes()).await?;
    stream.write_all(BODY.as_bytes()).await?;
    stream.flush().await
}

/// BAGG-1: serve the last computed 24h/7d aggregates snapshot as JSON. Before
/// the live loop's first [`DashboardHandle::set_aggregates`] call this is
/// `null` — the dashboard JS treats that as "not loaded yet" (same as any
/// other slow first paint), never a crash.
async fn serve_aggregates(
    stream: &mut TcpStream,
    aggregates: &Mutex<Option<String>>,
) -> std::io::Result<()> {
    let body = aggregates
        .lock()
        .expect("aggregates poisoned")
        .clone()
        .unwrap_or_else(|| "null".to_string());
    let header = format!(
        "HTTP/1.1 200 OK\r\n\
         Content-Type: application/json\r\n\
         Cache-Control: no-cache\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(header.as_bytes()).await?;
    stream.write_all(body.as_bytes()).await?;
    stream.flush().await
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

/// BFRAME-1: serve the embedded "what this is / is not" page.
async fn serve_about(stream: &mut TcpStream) -> std::io::Result<()> {
    let header = format!(
        "HTTP/1.1 200 OK\r\n\
         Content-Type: text/html; charset=utf-8\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\r\n",
        ABOUT_HTML.len()
    );
    stream.write_all(header.as_bytes()).await?;
    stream.write_all(ABOUT_HTML.as_bytes()).await?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::live::DecisionRecord;
    use chrono::Utc;
    use std::time::Duration;

    /// A distinctive record so the round-trip asserts the real payload, not a
    /// coincidence. Skipped intent (no quote) → the `Option`s are `None`.
    fn sample_record() -> DecisionRecord {
        DecisionRecord {
            intent_id: "across:base:arbitrum:dash-test".into(),
            route: "base->arbitrum/833589->af88d0".into(),
            observed_at: Utc::now(),
            as_of: Utc::now(),
            would_fill: false,
            reason: "exclusive_to_other".into(),
            expected_profit_usd: None,
            confidence: None,
            gas_usd: None,
            gas_source: None,
            verify_url: Some("https://basescan.org/tx/0xabc".into()),
            retrospective: false,
        }
    }

    /// Drive a raw HTTP request and read the whole response (server sends
    /// `Connection: close` for one-shot routes, so read-to-EOF terminates).
    async fn http_get(addr: SocketAddr, path: &str) -> String {
        let mut s = TcpStream::connect(addr).await.expect("connect");
        let req = format!("GET {path} HTTP/1.1\r\nHost: localhost\r\n\r\n");
        s.write_all(req.as_bytes()).await.expect("write request");
        let mut resp = Vec::new();
        tokio::time::timeout(Duration::from_secs(5), s.read_to_end(&mut resp))
            .await
            .expect("response timed out")
            .expect("read response");
        String::from_utf8_lossy(&resp).into_owned()
    }

    /// Read an SSE response until the first complete `data: …\n\n` frame.
    async fn read_first_event(addr: SocketAddr) -> String {
        let mut s = TcpStream::connect(addr).await.expect("connect");
        s.write_all(b"GET /events HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .await
            .expect("write request");
        let read = async {
            let mut buf = Vec::new();
            let mut tmp = [0u8; 1024];
            loop {
                let n = s.read(&mut tmp).await.expect("read events");
                assert!(n > 0, "stream closed before an event arrived");
                buf.extend_from_slice(&tmp[..n]);
                let text = String::from_utf8_lossy(&buf);
                if let Some(start) = text.find("data: ") {
                    if text[start..].contains("\n\n") {
                        return text.into_owned();
                    }
                }
            }
        };
        tokio::time::timeout(Duration::from_secs(5), read)
            .await
            .expect("event timed out")
    }

    /// `GET /` returns the embedded dashboard HTML with the right envelope.
    #[tokio::test]
    async fn serves_embedded_html() {
        let h = serve(0).await.expect("serve");
        let resp = http_get(h.addr, "/").await;
        assert!(resp.contains("200 OK"), "expected 200, got:\n{resp}");
        assert!(
            resp.to_lowercase().contains("content-type: text/html"),
            "expected html content-type"
        );
        assert!(resp.contains("<!doctype html"), "expected the embedded page body");
    }

    /// `GET /about` (BFRAME-1) serves the embedded "what this is / is not"
    /// page — a reviewer must be able to reach it from the running binary
    /// alone, no external doc lookup.
    #[tokio::test]
    async fn serves_embedded_about_page() {
        let h = serve(0).await.expect("serve");
        let resp = http_get(h.addr, "/about").await;
        assert!(resp.contains("200 OK"), "expected 200, got:\n{resp}");
        assert!(
            resp.to_lowercase().contains("content-type: text/html"),
            "expected html content-type"
        );
        assert!(resp.contains("What this is"), "expected the about page body");
    }

    /// `GET /healthz` (BNET-1) answers a plain 200 "ok" — the reverse-proxy
    /// liveness target — independent of whether any decision has ever been
    /// published (an idle-but-up server is healthy, not silent-broken).
    #[tokio::test]
    async fn healthz_answers_ok() {
        let h = serve(0).await.expect("serve");
        let resp = http_get(h.addr, "/healthz").await;
        assert!(resp.contains("200 OK"), "expected 200, got:\n{resp}");
        assert!(
            resp.to_lowercase().contains("content-type: text/plain"),
            "expected text/plain content-type"
        );
        assert!(resp.trim_end().ends_with("ok"), "expected body 'ok', got:\n{resp}");
    }

    /// `GET /aggregates` (BAGG-1) is `null` before the live loop ever calls
    /// `set_aggregates` — never a crash for a client that connects in that
    /// window — and round-trips whatever JSON was last set, verbatim.
    #[tokio::test]
    async fn aggregates_serves_last_snapshot_or_null() {
        let h = serve(0).await.expect("serve");

        let before = http_get(h.addr, "/aggregates").await;
        assert!(before.contains("200 OK"), "expected 200, got:\n{before}");
        assert!(before.contains("application/json"), "expected json content-type");
        assert!(before.trim_end().ends_with("null"), "no snapshot yet ⇒ null, got:\n{before}");

        h.set_aggregates(r#"{"computed_at":"2026-01-01T00:00:00Z","last_24h":{"total":1}}"#.to_string());
        let after = http_get(h.addr, "/aggregates").await;
        assert!(after.contains(r#""total":1"#), "expected the snapshot we set, got:\n{after}");
    }

    /// `GET /events` is an event-stream and forwards a published
    /// `DecisionRecord` — including the replay buffer, so a record published
    /// before the client connects still arrives — round-tripping intact.
    #[tokio::test]
    async fn events_streams_published_decision() {
        let h = serve(0).await.expect("serve");
        let rec = sample_record();
        h.publish(&rec); // lands in the replay buffer before the client connects

        let text = read_first_event(h.addr).await;
        assert!(
            text.to_lowercase().contains("content-type: text/event-stream"),
            "expected event-stream content-type, got:\n{text}"
        );

        // Extract the JSON between `data: ` and the frame terminator `\n\n`.
        let start = text.find("data: ").expect("a data frame") + "data: ".len();
        let rest = &text[start..];
        let end = rest.find("\n\n").expect("frame terminator");
        let json = &rest[..end];

        let back: DecisionRecord = serde_json::from_str(json).expect("valid record JSON");
        assert_eq!(back.intent_id, rec.intent_id);
        assert_eq!(back.route, rec.route);
        assert_eq!(back.reason, rec.reason);
        assert_eq!(back.would_fill, rec.would_fill);
        assert_eq!(back.verify_url, rec.verify_url);
    }

    /// A live decision published while a client is connected is pushed without
    /// the client having to reconnect (the broadcast path, not just replay).
    #[tokio::test]
    async fn events_pushes_live_after_connect() {
        let h = serve(0).await.expect("serve");

        let mut s = TcpStream::connect(h.addr).await.expect("connect");
        s.write_all(b"GET /events HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .await
            .expect("write request");
        // Drain the header block so the next read sees only the live frame.
        let read_headers = async {
            let mut buf = Vec::new();
            let mut tmp = [0u8; 1024];
            while !String::from_utf8_lossy(&buf).contains("\r\n\r\n") {
                let n = s.read(&mut tmp).await.expect("read headers");
                assert!(n > 0, "closed before headers");
                buf.extend_from_slice(&tmp[..n]);
            }
        };
        tokio::time::timeout(Duration::from_secs(5), read_headers)
            .await
            .expect("headers timed out");

        // Publish AFTER the client is subscribed → exercises the broadcast.
        let rec = sample_record();
        h.publish(&rec);

        let read_frame = async {
            let mut buf = Vec::new();
            let mut tmp = [0u8; 1024];
            loop {
                let n = s.read(&mut tmp).await.expect("read frame");
                assert!(n > 0, "closed before live frame");
                buf.extend_from_slice(&tmp[..n]);
                if String::from_utf8_lossy(&buf).contains("\n\n") {
                    return String::from_utf8_lossy(&buf).into_owned();
                }
            }
        };
        let frame = tokio::time::timeout(Duration::from_secs(5), read_frame)
            .await
            .expect("live frame timed out");
        assert!(frame.contains(&rec.intent_id), "live frame missing the record");
    }
}
