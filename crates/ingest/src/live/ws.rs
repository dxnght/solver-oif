//! `eth_subscribe("logs")` WebSocket transport for live deposit logs.
//!
//! A thin JSON-RPC-over-WS client: connect to a node's `wss://` endpoint,
//! subscribe to one contract's logs filtered by `topic0`, and forward every
//! matching log as a [`WsLog`]. This is pure transport — decoding a log into a
//! `NormalizedIntent` is the Across adapter's job (next sprint, LIV-3).
//!
//! Legibility (CLAUDE.md rule #7): a confirmed subscription is logged loudly
//! (`subscription confirmed`), and a node that **rejects** `eth_subscribe`
//! (wrong method/params — retrying can't help) is a loud, fatal bail, never
//! silence. A dropped connection is transient: reconnect with capped
//! exponential backoff. *(Lesson baked in: "subscription confirmed but then
//! silence / feels broken" must be impossible — every state is announced.)*
//!
//! The networking loop needs a live node, so it is exercised by the bucket's
//! smoke run, not unit tests. The unit-testable core is the JSON shaping and
//! parsing, kept in free functions: [`subscribe_request`], [`parse_ack`],
//! [`parse_notification`].

use std::time::Duration;

use anyhow::{bail, Context, Result};
use futures::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{info, warn};

/// JSON-RPC id of our single `eth_subscribe` call on each connection.
const SUBSCRIBE_ID: u64 = 1;

/// Reconnect backoff bounds. We start fast (a node restart clears quickly) and
/// cap so a long outage doesn't busy-loop. A connection that successfully
/// subscribed resets the backoff (see [`stream_logs`]).
const BACKOFF_START: Duration = Duration::from_secs(1);
const BACKOFF_MAX: Duration = Duration::from_secs(30);

/// What to subscribe to on one chain's node.
#[derive(Debug, Clone)]
pub struct WsConfig {
    /// `wss://…` node endpoint (e.g. the value of `ACROSS_WS_ARBITRUM`).
    pub ws_url: String,
    /// Contract whose logs we want (the chain's SpokePool), `0x…`.
    pub address: String,
    /// Event `topic0` filter, `0x…` (the `FundsDeposited` selector).
    pub topic0: String,
    /// Human label for logs (the chain name) — legibility only.
    pub label: String,
}

/// A raw EVM log as delivered inside an `eth_subscription` notification.
///
/// These are the standard JSON-RPC log fields (camelCase). There is **no**
/// `timeStamp` (that is an Etherscan-only field, unlike
/// [`crate::across::client::RawLog`]) — live `observed_at` comes from the wall
/// clock, not the log. `topics`/`data` are decoded by the adapter via
/// [`crate::across::FundsDeposited`]; `transaction_hash` is the origin deposit
/// tx for the intent's `TxRef`. `Serialize` so the adapter can stash the raw
/// log on the normalized intent (mirrors the historical path).
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct WsLog {
    pub address: String,
    pub topics: Vec<String>,
    pub data: String,
    #[serde(rename = "blockNumber")]
    pub block_number: String,
    #[serde(rename = "transactionHash")]
    pub transaction_hash: String,
}

/// Build the exact `eth_subscribe(["logs", {address, topics:[topic0]}])`
/// request payload. A free function so the wire shape is testable without a
/// socket — a drifted method/params here would silently match nothing.
pub fn subscribe_request(address: &str, topic0: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": SUBSCRIBE_ID,
        "method": "eth_subscribe",
        "params": ["logs", {"address": address, "topics": [topic0]}],
    })
}

/// Parse the `eth_subscribe` response. Returns the subscription id on success;
/// a loud `Err` if the node returned a JSON-RPC error (subscribe rejected — the
/// method/params are unsupported, retrying won't help) so the caller bails
/// instead of sitting in silence (rule #7).
pub fn parse_ack(text: &str) -> Result<String> {
    let v: Value = serde_json::from_str(text).context("eth_subscribe ack was not JSON")?;
    if let Some(err) = v.get("error") {
        bail!("eth_subscribe rejected by node: {err}");
    }
    v.get("result")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| anyhow::anyhow!("eth_subscribe ack had no result subscription id: {text}"))
}

/// Parse one inbound WS text frame. Returns `Some(log)` when it is an
/// `eth_subscription` notification for our `subscription_id`, `None` for any
/// other frame (the ack, unrelated methods, a different subscription on the same
/// socket). A notification whose `result` does not shape into a [`WsLog`] is an
/// `Err` — silent schema drift is exactly the failure mode we refuse (rule #7).
pub fn parse_notification(text: &str, subscription_id: &str) -> Result<Option<WsLog>> {
    let v: Value = serde_json::from_str(text).context("WS frame was not JSON")?;
    if v.get("method").and_then(Value::as_str) != Some("eth_subscription") {
        return Ok(None);
    }
    let params = match v.get("params") {
        Some(p) => p,
        None => return Ok(None),
    };
    if params.get("subscription").and_then(Value::as_str) != Some(subscription_id) {
        return Ok(None); // a different subscription multiplexed on this socket
    }
    let result = params
        .get("result")
        .ok_or_else(|| anyhow::anyhow!("eth_subscription had no result log: {text}"))?;
    let log: WsLog = serde_json::from_value(result.clone())
        .with_context(|| format!("eth_subscription result was not a log: {result}"))?;
    Ok(Some(log))
}

/// Why a single connection ended (controls reconnect).
enum Ended {
    /// The consumer dropped the receiver (e.g. `--duration` elapsed) — done.
    ConsumerGone,
    /// The socket closed / errored — transient, reconnect. `was_subscribed`
    /// distinguishes a healthy stream that dropped (reset backoff) from a
    /// connect/handshake that never got going (keep growing it).
    Disconnected { was_subscribed: bool },
}

/// Connect, subscribe, and forward every matching log into `tx`, reconnecting
/// with capped exponential backoff across transient drops. Returns `Ok(())`
/// when the receiver is dropped (consumer finished) and `Err` only on a fatal,
/// non-retryable failure (`eth_subscribe` rejected) — the loud bail, not
/// silence.
pub async fn stream_logs(cfg: WsConfig, tx: mpsc::Sender<WsLog>) -> Result<()> {
    let mut backoff = BACKOFF_START;
    loop {
        match connect_and_stream(&cfg, &tx).await {
            Ok(Ended::ConsumerGone) => return Ok(()),
            Ok(Ended::Disconnected { was_subscribed }) => {
                let wait = if was_subscribed { BACKOFF_START } else { backoff };
                warn!(chain = %cfg.label, wait_s = wait.as_secs(),
                    "live WS disconnected — reconnecting after backoff");
                tokio::time::sleep(wait).await;
                backoff = if was_subscribed {
                    BACKOFF_START
                } else {
                    (backoff * 2).min(BACKOFF_MAX)
                };
            }
            Err(e) => return Err(e), // fatal — loud, never silent
        }
    }
}

/// One connection lifetime: connect → subscribe → stream until it ends.
async fn connect_and_stream(cfg: &WsConfig, tx: &mpsc::Sender<WsLog>) -> Result<Ended> {
    // Connection failures are transient (node restart, network) — a reconnect,
    // not a fatal error.
    let (mut socket, _resp) = match connect_async(&cfg.ws_url).await {
        Ok(ok) => ok,
        Err(e) => {
            warn!(chain = %cfg.label, error = %e, "live WS connect failed");
            return Ok(Ended::Disconnected { was_subscribed: false });
        }
    };

    let req = subscribe_request(&cfg.address, &cfg.topic0);
    socket
        .send(Message::Text(req.to_string()))
        .await
        .context("sending eth_subscribe")?;

    // The first text frame must be the subscribe ack; a JSON-RPC error in it is
    // fatal (loud bail), a closed/garbled socket is just a reconnect.
    let ack = match socket.next().await {
        Some(Ok(Message::Text(t))) => t,
        Some(Ok(Message::Close(_))) | None => {
            warn!(chain = %cfg.label, "live WS closed before subscribe ack");
            return Ok(Ended::Disconnected { was_subscribed: false });
        }
        Some(Ok(other)) => {
            warn!(chain = %cfg.label, ?other, "unexpected first WS frame; reconnecting");
            return Ok(Ended::Disconnected { was_subscribed: false });
        }
        Some(Err(e)) => {
            warn!(chain = %cfg.label, error = %e, "live WS error before ack");
            return Ok(Ended::Disconnected { was_subscribed: false });
        }
    };
    let sub_id = parse_ack(&ack)?; // fatal Err on "eth_subscribe rejected"
    info!(chain = %cfg.label, address = %cfg.address, subscription = %sub_id,
        "subscription confirmed");

    // Stream notifications until the socket drops or the consumer goes away.
    while let Some(frame) = socket.next().await {
        let text = match frame {
            Ok(Message::Text(t)) => t,
            Ok(Message::Ping(_) | Message::Pong(_)) => continue,
            Ok(Message::Close(_)) => {
                warn!(chain = %cfg.label, "live WS closed by node");
                return Ok(Ended::Disconnected { was_subscribed: true });
            }
            Ok(_) => continue, // binary/frame we don't expect on this feed
            Err(e) => {
                warn!(chain = %cfg.label, error = %e, "live WS stream error");
                return Ok(Ended::Disconnected { was_subscribed: true });
            }
        };
        match parse_notification(&text, &sub_id) {
            Ok(Some(log)) => {
                if tx.send(log).await.is_err() {
                    return Ok(Ended::ConsumerGone);
                }
            }
            Ok(None) => {} // ack echo / unrelated frame — ignore
            Err(e) => warn!(chain = %cfg.label, error = %e, "skipping malformed WS log"),
        }
    }
    Ok(Ended::Disconnected { was_subscribed: true })
}

#[cfg(test)]
mod tests {
    use super::*;

    // Real Arbitrum SpokePool + the canonical FundsDeposited topic0 (LIV-1).
    const SPOKE: &str = "0xe35e9842fceaca96570b734083f4a58e8f7c5f2a";
    const TOPIC0: &str = "0x32ed1a409ef04c7b0227189c3a103dc5ac10e775a15b785dcc510201f7c25ad3";
    // A plausible Geth/Infura subscription id (opaque 16-byte hex).
    const SUB_ID: &str = "0x9cef478923ff08bf67fde6c64013158d";

    /// A real Arbitrum `FundsDeposited` log (tx `0x7bcd2bf7…`, the on-chain
    /// values committed in `across/testdata/fundsdeposited_arb_usdt.json`)
    /// wrapped in the `eth_subscription` envelope a node pushes over WS. Real
    /// topics/data/tx — **no invented hex** — so the parse test runs on
    /// independent truth (rule #3).
    const REAL_DEPOSIT_NOTIFICATION: &str = r#"{"jsonrpc":"2.0","method":"eth_subscription","params":{"subscription":"0x9cef478923ff08bf67fde6c64013158d","result":{"address":"0xe35e9842fceaca96570b734083f4a58e8f7c5f2a","topics":["0x32ed1a409ef04c7b0227189c3a103dc5ac10e775a15b785dcc510201f7c25ad3","0x000000000000000000000000000000000000000000000000000000000000000a","0x0000000000000000000000000000000000000000000000000000000000435819","0x000000000000000000000000734793f216a973be81ef278e5fd873de19ead4b0"],"data":"0x000000000000000000000000fd086bc7cd5c481dcc9c85ebe478a1c0b69fcbb900000000000000000000000094b008aa00579c1307b0ef2c499ad98a8ce58e580000000000000000000000000000000000000000000000000000000005f515e10000000000000000000000000000000000000000000000000000000005f4103c000000000000000000000000000000000000000000000000000000006a37af2f000000000000000000000000000000000000000000000000000000006a37cb4f0000000000000000000000000000000000000000000000000000000000000000000000000000000000000000d7b33802fd6266bbcfe0f43cd3ce551d9c405caa000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000001400000000000000000000000000000000000000000000000000000000000000000","blockNumber":"0x1c5ba74e","transactionHash":"0x7bcd2bf7968003eb3db3b1fa01e5be92760eceb72b34ad117ad3a83105d3fd24","logIndex":"0x1d","removed":false}}}"#;

    /// The request must be the canonical `eth_subscribe(logs, {address,
    /// topics:[topic0]})` — a drifted method/param would subscribe to nothing
    /// and the feed would be silent (the AE-gap failure mode).
    #[test]
    fn subscribe_request_is_canonical_eth_subscribe() {
        let req = subscribe_request(SPOKE, TOPIC0);
        assert_eq!(req["jsonrpc"], "2.0");
        assert_eq!(req["id"], SUBSCRIBE_ID);
        assert_eq!(req["method"], "eth_subscribe");
        assert_eq!(req["params"][0], "logs");
        assert_eq!(req["params"][1]["address"], SPOKE);
        assert_eq!(req["params"][1]["topics"][0], TOPIC0);
        // Exactly one topic filter (topic0 only — match every FundsDeposited).
        assert_eq!(req["params"][1]["topics"].as_array().unwrap().len(), 1);
    }

    /// A success ack yields the subscription id we then match notifications on.
    #[test]
    fn parse_ack_returns_subscription_id() {
        let body = r#"{"jsonrpc":"2.0","id":1,"result":"0x9cef478923ff08bf67fde6c64013158d"}"#;
        assert_eq!(parse_ack(body).unwrap(), SUB_ID);
    }

    /// A rejected `eth_subscribe` must surface as a loud `Err`, not a silent
    /// never-confirmed subscription (rule #7). This is the JSON-RPC error shape
    /// a node returns when the method/endpoint can't serve subscriptions.
    #[test]
    fn parse_ack_rejection_is_loud_error() {
        let body = r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32601,"message":"the method eth_subscribe does not exist/is not available"}}"#;
        let err = parse_ack(body).unwrap_err().to_string();
        assert!(err.contains("eth_subscribe rejected"), "got: {err}");
    }

    /// A real notification parses into a [`WsLog`] carrying the on-chain topics,
    /// tx hash, and block; and that log actually decodes as the canonical
    /// `FundsDeposited` (topics+data round-trip through the LIV-1 binding) —
    /// proving the transport hands the adapter a consumable log, not just any
    /// JSON. destinationChainId 10 (Optimism) and depositId 4413465 are the
    /// independently-decoded ground truth from the committed fixture.
    #[test]
    fn parse_notification_extracts_real_consumable_log() {
        use alloy_primitives::{B256, U256};
        use alloy_sol_types::SolEvent;

        let log = parse_notification(REAL_DEPOSIT_NOTIFICATION, SUB_ID)
            .unwrap()
            .expect("a matching subscription log");
        assert_eq!(log.address, SPOKE);
        assert_eq!(log.topics.len(), 4); // topic0 + 3 indexed
        assert_eq!(log.topics[0], TOPIC0);
        assert_eq!(log.block_number, "0x1c5ba74e");
        assert_eq!(
            log.transaction_hash,
            "0x7bcd2bf7968003eb3db3b1fa01e5be92760eceb72b34ad117ad3a83105d3fd24"
        );

        let topics: Vec<B256> = log.topics.iter().map(|t| t.parse().unwrap()).collect();
        let data = alloy_primitives::hex::decode(&log.data).unwrap();
        let ev = crate::across::FundsDeposited::decode_raw_log(topics, &data, true).unwrap();
        assert_eq!(ev.destinationChainId, U256::from(10)); // → Optimism
        assert_eq!(ev.depositId, U256::from(4_413_465u64)); // 0x435819
    }

    /// A notification for a different subscription multiplexed on the same
    /// socket is ignored (not mis-attributed to ours).
    #[test]
    fn notification_for_other_subscription_is_ignored() {
        let other = parse_notification(REAL_DEPOSIT_NOTIFICATION, "0xdeadbeef").unwrap();
        assert!(other.is_none());
    }

    /// The subscribe ack itself is not a notification — it must parse to `None`,
    /// otherwise the consumer would try to decode the ack as a log.
    #[test]
    fn ack_frame_is_not_a_notification() {
        let ack = r#"{"jsonrpc":"2.0","id":1,"result":"0x9cef478923ff08bf67fde6c64013158d"}"#;
        assert!(parse_notification(ack, SUB_ID).unwrap().is_none());
    }
}
