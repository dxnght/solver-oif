//! Checkpointed `eth_getLogs` HTTP polling — the live-ingest transport.
//!
//! Replaces the earlier `eth_subscribe("logs")` WS push. A push subscription's
//! request volume is driven by real on-chain event throughput, which we don't
//! control and can't cap — on a busy chain (Across deposits across 3 L2s,
//! 24/7 on the deployed shadow service) that alone was enough to eat a
//! meaningful share of a free RPC-provider daily quota every night, for a
//! system that only ever scores `would_fill` **after** the fact (CLAUDE.md's
//! `would_fill` definition — a post-hoc model belief, never acted on live).
//! Polling gives a fixed, predictable request ceiling (2 JSON-RPC calls ×
//! chains × ticks/day) independent of chain activity, at the cost of
//! up-to-one-interval latency — exactly the trade this project's shadow-only
//! philosophy is happy to make.
//!
//! One tick: `eth_blockNumber` (the tip) then `eth_getLogs` over
//! `[checkpoint+1, min(tip, checkpoint+MAX_BLOCK_RANGE)]`. The checkpoint
//! always advances to the queried `toBlock` on success — even with zero
//! matching logs — so a quiet chain doesn't grow an ever-widening range next
//! tick. The checkpoint persists to a small JSON file so a restart resumes
//! near where it left off instead of re-scanning history or silently
//! skipping the gap (rule #7/#9). No history replay on first start (no
//! checkpoint file): the baseline is the chain's current tip, matching the
//! old WS subscription's "only what happens next" behavior.
//!
//! Every RPC failure here is transient (bad URL, rate limit, network) — there
//! is no analog to WS's "eth_subscribe rejected, retrying can't help" fatal
//! case, so this module never bails: a failed tick is a loud `WARN` and a
//! retry next tick, never silence.
//!
//! The network loop needs a live RPC endpoint, so it is exercised by the
//! bucket's smoke run, not unit tests. The unit-testable core is the JSON
//! shaping/parsing and the range-decision logic, kept in free functions:
//! [`get_logs_request`], [`block_number_request`], [`parse_get_logs`],
//! [`parse_block_number`], [`next_range`].

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tracing::{info, warn};

/// Max blocks requested in one `eth_getLogs` call — bounds both the request
/// cost and the response size after a long gap (e.g. downtime); a longer gap
/// then catches up over several ticks instead of one giant range a provider
/// might reject.
const MAX_BLOCK_RANGE: u64 = 2_000;

/// Default poll cadence — overridable via `solver live --poll-interval-secs`.
pub const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(60);

/// A raw EVM log as returned by `eth_getLogs` — the same fields the
/// Across/OIF decode functions need, regardless of which transport produced
/// them (this used to be delivered by the WS `eth_subscription` envelope;
/// the fields are identical, only the delivery mechanism changed).
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RawLog {
    pub address: String,
    pub topics: Vec<String>,
    pub data: String,
    #[serde(rename = "blockNumber")]
    pub block_number: String,
    #[serde(rename = "transactionHash")]
    pub transaction_hash: String,
}

/// What to poll on one chain's HTTP RPC endpoint.
#[derive(Debug, Clone)]
pub struct PollConfig {
    /// `https://…` JSON-RPC HTTP endpoint (e.g. `ACROSS_RPC_<CHAIN>`).
    pub rpc_url: String,
    /// Contract whose logs we want, `0x…`.
    pub address: String,
    /// Event `topic0` filter, `0x…`.
    pub topic0: String,
    /// Human label for logs (the chain name) — legibility only.
    pub label: String,
    /// Where this chain+source's checkpoint (last processed block) persists.
    pub checkpoint_path: PathBuf,
    pub interval: Duration,
}

/// Build the canonical `eth_getLogs([{address, topics:[topic0], fromBlock,
/// toBlock}])` request. A free function so the wire shape is testable
/// without a socket — a drifted param here would silently match nothing.
pub fn get_logs_request(address: &str, topic0: &str, from_block: u64, to_block: u64) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "eth_getLogs",
        "params": [{
            "address": address,
            "topics": [topic0],
            "fromBlock": format!("{from_block:#x}"),
            "toBlock": format!("{to_block:#x}"),
        }],
    })
}

/// The canonical `eth_blockNumber` request (no params).
pub fn block_number_request() -> Value {
    json!({"jsonrpc": "2.0", "id": 1, "method": "eth_blockNumber", "params": []})
}

/// Parse an `eth_getLogs` response into its matching logs. A JSON-RPC error
/// envelope is a loud `Err` (never an empty-and-silent `Vec`, which would
/// look identical to "no new deposits" — rule #7).
pub fn parse_get_logs(body: &str) -> Result<Vec<RawLog>> {
    let v: Value = serde_json::from_str(body).context("eth_getLogs response was not JSON")?;
    if let Some(err) = v.get("error") {
        anyhow::bail!("eth_getLogs rejected: {err}");
    }
    let result = v
        .get("result")
        .ok_or_else(|| anyhow::anyhow!("eth_getLogs: no result field: {body}"))?;
    serde_json::from_value(result.clone())
        .with_context(|| format!("eth_getLogs: result did not shape into logs: {result}"))
}

/// Parse an `eth_blockNumber` response into the tip block height.
pub fn parse_block_number(body: &str) -> Result<u64> {
    let v: Value = serde_json::from_str(body).context("eth_blockNumber response was not JSON")?;
    if let Some(err) = v.get("error") {
        anyhow::bail!("eth_blockNumber rejected: {err}");
    }
    let hex = v
        .get("result")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("eth_blockNumber: no result field: {body}"))?;
    parse_hex_u64(hex)
        .ok_or_else(|| anyhow::anyhow!("eth_blockNumber: unparsable result {hex}"))
}

fn parse_hex_u64(s: &str) -> Option<u64> {
    let body = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X"))?;
    u64::from_str_radix(body, 16).ok()
}

/// Decide what block range (if any) to query this tick, given the last
/// persisted checkpoint (`None` = no checkpoint yet — the caller establishes
/// a baseline instead of calling this) and the chain's current tip. `None`
/// means nothing new since the last tick; `Some((from, to))` is capped to
/// [`MAX_BLOCK_RANGE`] blocks so one call can't blow past a provider's range
/// limit after a long gap.
fn next_range(checkpoint: u64, tip: u64) -> Option<(u64, u64)> {
    let from = checkpoint + 1;
    if from > tip {
        return None;
    }
    let to = tip.min(from + MAX_BLOCK_RANGE - 1);
    Some((from, to))
}

#[derive(Serialize, Deserialize)]
struct CheckpointFile {
    last_block: u64,
}

async fn load_checkpoint(path: &Path) -> Option<u64> {
    let bytes = tokio::fs::read(path).await.ok()?;
    serde_json::from_slice::<CheckpointFile>(&bytes).ok().map(|c| c.last_block)
}

async fn store_checkpoint(path: &Path, last_block: u64) -> Result<()> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await.context("creating checkpoint dir")?;
    }
    let bytes = serde_json::to_vec(&CheckpointFile { last_block }).expect("checkpoint serializes");
    tokio::fs::write(path, bytes).await.context("writing checkpoint")
}

async fn fetch_block_number(http: &reqwest::Client, url: &str) -> Result<u64> {
    let body = http
        .post(url)
        .json(&block_number_request())
        .send()
        .await
        .context("eth_blockNumber send")?
        .text()
        .await
        .context("eth_blockNumber body")?;
    parse_block_number(&body)
}

async fn fetch_logs(
    http: &reqwest::Client,
    url: &str,
    address: &str,
    topic0: &str,
    from: u64,
    to: u64,
) -> Result<Vec<RawLog>> {
    let body = http
        .post(url)
        .json(&get_logs_request(address, topic0, from, to))
        .send()
        .await
        .context("eth_getLogs send")?
        .text()
        .await
        .context("eth_getLogs body")?;
    parse_get_logs(&body)
}

/// Poll `cfg` forever, forwarding every matching log into `tx`. Returns when
/// the consumer drops its receiver (e.g. the live loop shuts down). Every RPC
/// failure is a loud `WARN` + retry next tick (see module docs) — this
/// function has no fatal-error path.
pub async fn poll_loop(cfg: PollConfig, tx: mpsc::Sender<RawLog>) {
    let http = match reqwest::Client::builder().timeout(Duration::from_secs(15)).build() {
        Ok(c) => c,
        Err(e) => {
            warn!(chain = %cfg.label, error = %e, "live poll: building HTTP client failed — chain idle");
            return;
        }
    };

    let mut checkpoint = load_checkpoint(&cfg.checkpoint_path).await;
    if let Some(b) = checkpoint {
        info!(chain = %cfg.label, from_block = b + 1, "live poll resuming from checkpoint");
    } else {
        info!(chain = %cfg.label, interval_s = cfg.interval.as_secs(), "live poll starting (no checkpoint — baselining at chain tip, no history replay)");
    }

    let mut ticker = tokio::time::interval(cfg.interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        ticker.tick().await;
        if tx.is_closed() {
            return; // consumer gone
        }

        let tip = match fetch_block_number(&http, &cfg.rpc_url).await {
            Ok(t) => t,
            Err(e) => {
                warn!(chain = %cfg.label, error = %e, "live poll: eth_blockNumber failed — retrying next tick");
                continue;
            }
        };

        let Some(cp) = checkpoint else {
            // First successful tip: baseline here, persist it, and fetch logs
            // starting next tick (nothing to backfill by design).
            if let Err(e) = store_checkpoint(&cfg.checkpoint_path, tip).await {
                warn!(chain = %cfg.label, error = %e, "live poll: failed to persist initial checkpoint");
            }
            info!(chain = %cfg.label, tip, "live poll baselined at chain tip");
            checkpoint = Some(tip);
            continue;
        };

        let Some((from, to)) = next_range(cp, tip) else {
            continue; // no new blocks since the last tick
        };

        match fetch_logs(&http, &cfg.rpc_url, &cfg.address, &cfg.topic0, from, to).await {
            Ok(logs) => {
                let n = logs.len();
                for log in logs {
                    if tx.send(log).await.is_err() {
                        return; // consumer gone
                    }
                }
                if let Err(e) = store_checkpoint(&cfg.checkpoint_path, to).await {
                    warn!(chain = %cfg.label, error = %e, "live poll: failed to persist checkpoint");
                }
                checkpoint = Some(to);
                if n > 0 {
                    info!(chain = %cfg.label, count = n, from_block = from, to_block = to, "live poll: new logs");
                }
            }
            Err(e) => {
                warn!(chain = %cfg.label, error = %e, from_block = from, to_block = to,
                    "live poll: eth_getLogs failed — retrying next tick");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The request must be the canonical `eth_getLogs([{address, topics,
    /// fromBlock, toBlock}])` — a drifted param would silently match nothing
    /// (the same AE-gap failure mode the old WS subscribe test guarded).
    #[test]
    fn get_logs_request_is_canonical() {
        let req = get_logs_request("0xabc", "0xdef", 100, 200);
        assert_eq!(req["jsonrpc"], "2.0");
        assert_eq!(req["method"], "eth_getLogs");
        assert_eq!(req["params"][0]["address"], "0xabc");
        assert_eq!(req["params"][0]["topics"][0], "0xdef");
        assert_eq!(req["params"][0]["fromBlock"], "0x64");
        assert_eq!(req["params"][0]["toBlock"], "0xc8");
        assert_eq!(req["params"][0]["topics"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn block_number_request_is_canonical() {
        let req = block_number_request();
        assert_eq!(req["method"], "eth_blockNumber");
        assert_eq!(req["params"].as_array().unwrap().len(), 0);
    }

    /// `parse_get_logs` on the REAL committed Across fixture (the same
    /// on-chain Arbitrum `FundsDeposited` log the WS-era normalize tests
    /// used) — an Etherscan-shaped `{status,message,result:[...]}` envelope,
    /// which is a superset of the plain JSON-RPC `{result:[...]}` shape this
    /// parser reads. Independent on-chain truth (rule #3), not invented JSON.
    #[test]
    fn parse_get_logs_reads_real_across_fixture() {
        const REAL: &str = include_str!("../across/testdata/fundsdeposited_arb_usdt.json");
        let logs = parse_get_logs(REAL).unwrap();
        assert_eq!(logs.len(), 1);
        assert_eq!(logs[0].address, "0xe35e9842fceaca96570b734083f4a58e8f7c5f2a");
        assert_eq!(logs[0].block_number, "0x1c5ba74e");
        assert_eq!(
            logs[0].transaction_hash,
            "0x7bcd2bf7968003eb3db3b1fa01e5be92760eceb72b34ad117ad3a83105d3fd24"
        );
        assert_eq!(logs[0].topics.len(), 4);
    }

    /// Same independent-truth argument for the real OIF `Open` log fixture —
    /// a plain `{result:[...]}` envelope (this crate's OIF-2 test fixture).
    #[test]
    fn parse_get_logs_reads_real_oif_fixture() {
        const REAL: &str = include_str!("../oif/testdata/open_base_usdc_to_polygon.json");
        let logs = parse_get_logs(REAL).unwrap();
        assert_eq!(logs.len(), 1);
        assert_eq!(logs[0].address, "0x1cc9260e285c2c8ac8d2e7102f3978056ec1d0a8");
        assert_eq!(
            logs[0].transaction_hash,
            "0x504e78d99fcbcd1dd9c26a13906b475a1d5d720bf9b8cbed6ec68358844f1b57"
        );
    }

    /// A JSON-RPC error envelope must be a loud `Err`, never an empty (and
    /// indistinguishable from "no new deposits") `Vec` — rule #7.
    #[test]
    fn parse_get_logs_rejection_is_loud_error() {
        let body = r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32000,"message":"query returned more than 10000 results"}}"#;
        let err = parse_get_logs(body).unwrap_err().to_string();
        assert!(err.contains("eth_getLogs rejected"), "got: {err}");
    }

    /// The canonical `eth_blockNumber` envelope — hex tip → decimal u64.
    #[test]
    fn parse_block_number_reads_hex_result() {
        let body = r#"{"jsonrpc":"2.0","id":1,"result":"0x1c5ba90"}"#;
        assert_eq!(parse_block_number(body).unwrap(), 0x1c5ba90);
    }

    #[test]
    fn parse_block_number_rejection_is_loud_error() {
        let body = r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32601,"message":"method not found"}}"#;
        let err = parse_block_number(body).unwrap_err().to_string();
        assert!(err.contains("eth_blockNumber rejected"), "got: {err}");
    }

    /// `next_range`: no new blocks (tip == checkpoint) → `None`; a small gap
    /// → the exact `(checkpoint+1, tip)` range; a gap wider than
    /// `MAX_BLOCK_RANGE` → capped, not the full span (bounds one call's cost
    /// after a long outage — the rest catches up over subsequent ticks).
    #[test]
    fn next_range_no_new_blocks_is_none() {
        assert_eq!(next_range(100, 100), None);
    }

    #[test]
    fn next_range_small_gap_is_exact() {
        assert_eq!(next_range(100, 105), Some((101, 105)));
    }

    #[test]
    fn next_range_wide_gap_is_capped() {
        let (from, to) = next_range(100, 100 + MAX_BLOCK_RANGE * 3).unwrap();
        assert_eq!(from, 101);
        assert_eq!(to, 101 + MAX_BLOCK_RANGE - 1);
        assert_eq!(to - from + 1, MAX_BLOCK_RANGE);
    }

    /// Checkpoint round-trips through disk — a restart reads back exactly
    /// what the previous run persisted (the restart-safety this module
    /// exists to provide).
    #[tokio::test]
    async fn checkpoint_round_trips_through_disk() {
        let dir = std::env::temp_dir().join(format!("poll-ckpt-test-{}", std::process::id()));
        let path = dir.join("checkpoint-test-base.json");
        assert!(load_checkpoint(&path).await.is_none(), "no file yet");
        store_checkpoint(&path, 12_345).await.unwrap();
        assert_eq!(load_checkpoint(&path).await, Some(12_345));
        store_checkpoint(&path, 12_400).await.unwrap();
        assert_eq!(load_checkpoint(&path).await, Some(12_400));
        let _ = tokio::fs::remove_dir_all(&dir).await;
    }
}
