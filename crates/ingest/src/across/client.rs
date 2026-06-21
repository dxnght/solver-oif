//! Across historical fill ingest via the Etherscan **V2** multichain API.
//!
//! Mirrors the UniswapX client's shape (config + `new`/`run`/`scan_chain`,
//! one shared `reqwest::Client`, lazy JSONL sink) but the upstream is logs, not
//! a REST orders feed: we read the destination-chain `FilledRelay` event
//! (topic0 from [`crate::across::FilledRelay`]) off each chain's verified
//! [`crate::across::spoke_pool`].
//!
//! Etherscan V2: one host, `chainid` as a query param —
//! `https://api.etherscan.io/v2/api?chainid={id}`. We need `ETHERSCAN_API_KEY`;
//! [`AcrossIngest::new`] refuses to run without it (a clear error, never a
//! silent empty pull). The free key only covers a subset of chains — an
//! unsupported chain returns a loud per-chain error and the run continues
//! (legibility, rule #7); `doctor` reports the same readiness up front.
//!
//! Pagination: `getLogs` returns at most 1000 records, ascending by block. We
//! page by advancing a block cursor to the last returned block until a short
//! page signals the window is drained. Cross-run overlap is fine — load-time
//! dedup by `IntentId` is the safety net (rule #9).

use std::path::PathBuf;
use std::time::Duration as StdDuration;

use anyhow::{Context, Result};
use chrono::{Duration, Utc};
use reqwest::Client;
use serde::Deserialize;
use solver_types::ChainId;
use tracing::{info, warn};

use crate::across::spoke_pool::{spoke_pool, ACROSS_CHAINS};
use crate::across::FilledRelay;
use alloy_sol_types::SolEvent;

/// Etherscan V2 multichain base (one host; `chainid` selects the chain).
const ETHERSCAN_V2_BASE: &str = "https://api.etherscan.io/v2";

/// Etherscan caps `getLogs` at 1000 records per call.
const ETHERSCAN_LOGS_CAP: usize = 1000;

#[derive(Debug, Clone)]
pub struct AcrossIngestConfig {
    pub days_back: i64,
    pub out_dir: PathBuf,
    /// Override set of chains; defaults to [`ACROSS_CHAINS`].
    pub chains: Option<Vec<ChainId>>,
    /// Etherscan V2 base URL (escape hatch / test injection).
    pub base_url: String,
    /// `ETHERSCAN_API_KEY`. Empty is rejected by [`AcrossIngest::new`].
    pub api_key: String,
    /// Min spacing between API calls (free tier is ~5 req/s).
    pub rate_limit_ms: u64,
}

impl AcrossIngestConfig {
    /// Pure defaults — `api_key` is intentionally empty so the caller (the CLI)
    /// supplies it from the environment and `new` can give one clear error.
    pub fn defaults(out_dir: PathBuf) -> Self {
        Self {
            days_back: 14,
            out_dir,
            chains: None,
            base_url: ETHERSCAN_V2_BASE.to_string(),
            api_key: String::new(),
            rate_limit_ms: 220,
        }
    }
}

pub struct AcrossIngest {
    cfg: AcrossIngestConfig,
    http: Client,
}

impl AcrossIngest {
    pub fn new(cfg: AcrossIngestConfig) -> Result<Self> {
        if cfg.api_key.trim().is_empty() {
            anyhow::bail!(
                "across ingest needs ETHERSCAN_API_KEY (Etherscan V2 multichain key). \
                 Set it in .env or the environment, then re-run. See `solver doctor`."
            );
        }
        let http = Client::builder()
            .timeout(StdDuration::from_secs(30))
            .user_agent("solver-ingest/0.0.1")
            .build()
            .context("building reqwest client")?;
        Ok(Self { cfg, http })
    }

    pub async fn run(&self) -> Result<usize> {
        let until = Utc::now();
        let since = until - Duration::days(self.cfg.days_back);
        info!(?since, ?until, "starting Across FilledRelay backfill (Etherscan V2)");

        let chains: Vec<ChainId> =
            self.cfg.chains.clone().unwrap_or_else(|| ACROSS_CHAINS.to_vec());

        let mut total = 0;
        for chain in chains {
            match self.scan_chain(chain, since.timestamp() as u64, until.timestamp() as u64).await {
                Ok(n) => total += n,
                Err(e) => warn!(chain = %chain, error = %e, "across chain scan failed; continuing"),
            }
        }
        Ok(total)
    }

    /// Scan one destination chain's `FilledRelay` logs over `[since, until]`.
    /// AX-3: fetch + count only — normalization and JSONL writing land in AX-4.
    async fn scan_chain(&self, chain: ChainId, since: u64, until: u64) -> Result<usize> {
        let spoke = spoke_pool(chain)
            .ok_or_else(|| anyhow::anyhow!("no verified SpokePool for {chain}; refusing to scan"))?;

        let from_block = self.get_block_by_time(chain, since, "after").await?;
        let to_block = self.get_block_by_time(chain, until, "before").await?;
        if from_block > to_block {
            warn!(chain = %chain, from_block, to_block, "empty block window; skipping");
            return Ok(0);
        }

        let spoke_hex = spoke.to_string(); // EIP-55 0x… address
        let mut cursor = from_block;
        let mut fetched = 0usize;
        loop {
            let logs = self.get_logs(chain, &spoke_hex, cursor, to_block).await?;
            if logs.is_empty() {
                break;
            }
            let last_block = logs.last().and_then(|l| l.block_number()).unwrap_or(cursor);
            fetched += logs.len();

            if logs.len() < ETHERSCAN_LOGS_CAP {
                break; // window drained
            }
            // Advance past the last block we saw; overlap (== case) is deduped
            // on load. The pathological "≥1000 fills in one block" can't make
            // progress by overlap, so force +1 and warn about possible loss.
            if last_block <= cursor {
                warn!(chain = %chain, block = last_block,
                    "≥1000 FilledRelay in one block — some may be skipped (Etherscan 1000-cap)");
                cursor = last_block + 1;
            } else {
                cursor = last_block;
            }
            if cursor > to_block {
                break;
            }
        }

        info!(chain = %chain, fetched, "across chain scan (normalize pending AX-4)");
        Ok(fetched)
    }

    /// Resolve a unix `timestamp` to a block number on `chain` via
    /// `block/getblocknobytime`. `closest` is `"before"` or `"after"`.
    async fn get_block_by_time(&self, chain: ChainId, timestamp: u64, closest: &str) -> Result<u64> {
        let url = format!(
            "{base}/api?chainid={cid}&module=block&action=getblocknobytime\
             &timestamp={ts}&closest={closest}&apikey={key}",
            base = self.cfg.base_url,
            cid = chain.0,
            ts = timestamp,
            closest = closest,
            key = self.cfg.api_key,
        );
        let body = self.get(&url).await?;
        let env: Envelope = serde_json::from_str(&body)
            .with_context(|| format!("parsing getblocknobytime body: {body}"))?;
        if env.status != "1" {
            anyhow::bail!("getblocknobytime {chain}: {}", env.message_or_result());
        }
        let block: u64 = env
            .result
            .as_str()
            .and_then(|s| s.parse().ok())
            .ok_or_else(|| anyhow::anyhow!("getblocknobytime {chain}: unparsable result {:?}", env.result))?;
        Ok(block)
    }

    /// One `logs/getLogs` page (≤1000 records, ascending by block).
    async fn get_logs(&self, chain: ChainId, address: &str, from_block: u64, to_block: u64) -> Result<Vec<RawLog>> {
        let url = logs_url(
            &self.cfg.base_url, chain, address, &topic0_hex(), from_block, to_block, &self.cfg.api_key,
        );
        let body = self.get(&url).await?;
        parse_logs_envelope(&body).with_context(|| format!("getLogs {chain} [{from_block},{to_block}]"))
    }

    /// HTTP GET with rate-limit spacing and status check.
    async fn get(&self, url: &str) -> Result<String> {
        let resp = self.http.get(url).send().await.with_context(|| format!("GET {url}"))?;
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        tokio::time::sleep(StdDuration::from_millis(self.cfg.rate_limit_ms)).await;
        if !status.is_success() {
            anyhow::bail!("etherscan HTTP {status}: {body}");
        }
        Ok(body)
    }
}

/// Canonical `FilledRelay` topic0 as a `0x…` hex string for the query.
fn topic0_hex() -> String {
    format!("{:#x}", FilledRelay::SIGNATURE_HASH)
}

/// Build the `getLogs` URL (free function so it is unit-testable without I/O).
fn logs_url(
    base: &str,
    chain: ChainId,
    address: &str,
    topic0: &str,
    from_block: u64,
    to_block: u64,
    key: &str,
) -> String {
    format!(
        "{base}/api?chainid={cid}&module=logs&action=getLogs&address={address}\
         &topic0={topic0}&fromBlock={from_block}&toBlock={to_block}\
         &page=1&offset={cap}&apikey={key}",
        cid = chain.0,
        cap = ETHERSCAN_LOGS_CAP,
    )
}

/// Parse a `getLogs` response envelope into raw logs. `status:"0"` with a
/// "No records found" message is an **empty** result, not an error — Etherscan
/// signals "zero logs" that way. Any other `status:"0"` (rate limit, bad key,
/// unsupported chain) is a real error surfaced to the caller.
fn parse_logs_envelope(body: &str) -> Result<Vec<RawLog>> {
    let env: Envelope =
        serde_json::from_str(body).with_context(|| format!("parsing getLogs body: {body}"))?;
    if env.status != "1" {
        if env.message.contains("No records found") {
            return Ok(Vec::new());
        }
        anyhow::bail!("getLogs error: {}", env.message_or_result());
    }
    let logs: Vec<RawLog> = serde_json::from_value(env.result)
        .context("getLogs result was not an array of logs")?;
    Ok(logs)
}

// -------- Etherscan envelope + log DTOs --------

#[derive(Debug, Deserialize)]
struct Envelope {
    status: String,
    #[serde(default)]
    message: String,
    #[serde(default)]
    result: serde_json::Value,
}

impl Envelope {
    /// Human cause for an error envelope — Etherscan puts the reason in
    /// `result` (a string) for some errors and in `message` for others.
    fn message_or_result(&self) -> String {
        match self.result.as_str() {
            Some(s) if !s.is_empty() => s.to_string(),
            _ => self.message.clone(),
        }
    }
}

/// Raw `getLogs` entry. Hex-encoded fields are parsed lazily by helpers so a
/// single malformed field doesn't reject the whole page (rule #9 hygiene).
#[derive(Debug, Clone, Deserialize)]
pub struct RawLog {
    pub topics: Vec<String>,
    pub data: String,
    #[serde(rename = "blockNumber")]
    pub block_number: String,
    #[serde(rename = "timeStamp")]
    pub time_stamp: String,
    #[serde(rename = "transactionHash")]
    pub transaction_hash: String,
}

impl RawLog {
    /// Block number (hex `0x…`) as `u64`, or `None` if unparsable.
    pub fn block_number(&self) -> Option<u64> {
        parse_hex_u64(&self.block_number)
    }
    /// Log `timeStamp` (hex unix seconds) as `i64`, or `None` if unparsable.
    pub fn time_stamp(&self) -> Option<i64> {
        parse_hex_u64(&self.time_stamp).map(|v| v as i64)
    }
}

fn parse_hex_u64(s: &str) -> Option<u64> {
    let s = s.trim();
    let hex = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")).unwrap_or(s);
    if hex.is_empty() {
        return None;
    }
    u64::from_str_radix(hex, 16).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The query URL carries the audited topic0 and the right chain id — a
    /// drifted topic0 here would silently empty every pull (AE-gap). Checks
    /// against the independently-known canonical topic0, not a re-derivation.
    #[test]
    fn logs_url_has_canonical_topic0_and_chain() {
        let url = logs_url(
            "https://api.etherscan.io/v2",
            ChainId::ARBITRUM,
            "0xe35e9842fceaca96570b734083f4a58e8f7c5f2a",
            &topic0_hex(),
            100,
            200,
            "KEY",
        );
        assert!(url.contains("chainid=42161"));
        assert!(url.contains("module=logs&action=getLogs"));
        assert!(url.contains(
            "topic0=0x44b559f101f8fbcc8a0ea43fa91a05a729a5ea6e14a7c75aa750374690137208"
        ));
        assert!(url.contains("fromBlock=100&toBlock=200"));
        assert!(url.contains("offset=1000"));
    }

    /// "No records found" (status 0) is an empty page, not an error — getting
    /// this wrong would abort a chain that simply had no fills in the window.
    #[test]
    fn no_records_parses_as_empty_not_error() {
        let body = r#"{"status":"0","message":"No records found","result":[]}"#;
        assert!(parse_logs_envelope(body).unwrap().is_empty());
    }

    /// A real error envelope (unsupported chain on the free key) must surface
    /// as an Err carrying the upstream reason — the loud failure rule #7. This
    /// is the exact body the free key returns for Base/Optimism.
    #[test]
    fn unsupported_chain_envelope_is_error() {
        let body = r#"{"status":"0","message":"NOTOK","result":"Free API access is not supported for this chain. Please upgrade your api plan for full chain coverage."}"#;
        let err = parse_logs_envelope(body).unwrap_err().to_string();
        assert!(err.contains("Free API access is not supported"), "got: {err}");
    }

    /// A successful page parses into RawLogs with hex fields decodable. Uses the
    /// real captured Arbitrum response shape (topics + data + block/time/tx).
    #[test]
    fn ok_envelope_parses_logs_and_hex_fields() {
        let body = r#"{"status":"1","message":"OK","result":[
            {"address":"0xe35e9842fceaca96570b734083f4a58e8f7c5f2a",
             "topics":["0x44b559f101f8fbcc8a0ea43fa91a05a729a5ea6e14a7c75aa750374690137208",
                       "0x0000000000000000000000000000000000000000000000000000000000000001",
                       "0x00000000000000000000000000000000000000000000000000000000003d3304",
                       "0x000000000000000000000000d0588c940bdbfcfb5f3e8e3c843ed1462d9f828e"],
             "data":"0x00",
             "blockNumber":"0x1c5a2244",
             "timeStamp":"0x6a374e7d",
             "transactionHash":"0x234ddc0d8ff5742f9ef05eea8b40865dd9efd42c3f0de85fb1fc70985aa887dc"}
        ]}"#;
        let logs = parse_logs_envelope(body).unwrap();
        assert_eq!(logs.len(), 1);
        let l = &logs[0];
        assert_eq!(l.topics.len(), 4);
        assert_eq!(l.block_number(), Some(0x1c5a2244));
        assert_eq!(l.time_stamp(), Some(0x6a374e7d));
        assert_eq!(l.transaction_hash, "0x234ddc0d8ff5742f9ef05eea8b40865dd9efd42c3f0de85fb1fc70985aa887dc");
    }
}
