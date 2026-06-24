//! Live gas model via `eth_feeHistory`, behind the `--live-gas` flag.
//!
//! [`StaticGasModel`] hard-codes a per-chain median wei estimate; on an L2 that
//! drifts (base fees move minute to minute) and over-states a cheap fill by an
//! order of magnitude. [`FeeHistoryGasModel`] reads the chain's real recent fee
//! via `eth_feeHistory` over an HTTP RPC (`ACROSS_RPC_<CHAIN>`) and prices the
//! same typical fill (`gas_used × gas_price`) at the live price.
//!
//! HTTP, not WS: `eth_feeHistory` is a request/response call, not a
//! subscription, so it uses a separate `ACROSS_RPC_<CHAIN>` endpoint (the
//! `ACROSS_WS_<CHAIN>` ones are for log subscriptions).
//!
//! Honesty (CLAUDE.md #2 / #7): a chain with **no** RPC configured, or any RPC
//! error, falls back to [`StaticGasModel`] with a `warn!` — never a fabricated
//! or zero gas. A 12s per-chain cache keeps a busy backtest/live stream off the
//! RPC. Only the gas *price* is live; the gas *units* reuse the static model's
//! typical-fill assumption, so live vs static differ in exactly one axis.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use alloy_primitives::U256;
use async_trait::async_trait;
use serde_json::{json, Value};
use solver_types::{ChainId, GasOrigin};
use tracing::warn;

use crate::gas::{GasEstimate, GasModel, StaticGasModel, TYPICAL_FILL_GAS};

/// Per-chain cache TTL. Base fees move per block (~2s on an L2); 12s is fresh
/// enough to track them while keeping a thousand-intent backtest off the RPC.
const CACHE_TTL: Duration = Duration::from_secs(12);

/// Chains we resolve an `ACROSS_RPC_<CHAIN>` endpoint for in [`Self::from_env`].
/// The verified-SpokePool set (same as historical Across) — the chains a live
/// run actually touches. Other chains simply fall back to static gas.
pub const GAS_RPC_CHAINS: &[ChainId] = &[
    ChainId::ETHEREUM,
    ChainId::OPTIMISM,
    ChainId::POLYGON,
    ChainId::BASE,
    ChainId::ARBITRUM,
];

/// Live gas model: `eth_feeHistory` per chain, static fallback.
pub struct FeeHistoryGasModel {
    fallback: StaticGasModel,
    http: reqwest::Client,
    /// Resolved `ACROSS_RPC_<CHAIN>` HTTP endpoints. A chain absent here has no
    /// RPC configured → it uses the static fallback.
    rpc_urls: HashMap<ChainId, String>,
    cache: Mutex<HashMap<ChainId, (Instant, U256)>>,
}

impl FeeHistoryGasModel {
    /// Build with an explicit RPC map — the test seam (pass an empty map to
    /// exercise the no-RPC fallback without touching the process env).
    pub fn new(rpc_urls: HashMap<ChainId, String>) -> Self {
        Self {
            fallback: StaticGasModel,
            // A per-request timeout so a dead/slow RPC errors (→ static
            // fallback) instead of stalling the whole stream silently (rule #7);
            // reqwest has no default timeout.
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(8))
                .build()
                .unwrap_or_default(),
            rpc_urls,
            cache: Mutex::new(HashMap::new()),
        }
    }

    /// Production model: resolve `ACROSS_RPC_<CHAIN>` for each [`GAS_RPC_CHAINS`]
    /// entry (var name = `chain.name().to_uppercase()`, matching the WS vars).
    pub fn from_env() -> Self {
        let mut rpc_urls = HashMap::new();
        for &chain in GAS_RPC_CHAINS {
            if let Some(url) = std::env::var(across_rpc_var(chain))
                .ok()
                .filter(|v| !v.trim().is_empty())
            {
                rpc_urls.insert(chain, url);
            }
        }
        Self::new(rpc_urls)
    }

    fn cached(&self, chain: ChainId) -> Option<U256> {
        let cache = self.cache.lock().expect("gas cache poisoned");
        cache
            .get(&chain)
            .and_then(|(t, v)| (t.elapsed() < CACHE_TTL).then_some(*v))
    }

    fn store(&self, chain: ChainId, value: U256) {
        self.cache
            .lock()
            .expect("gas cache poisoned")
            .insert(chain, (Instant::now(), value));
    }

    /// One live `eth_feeHistory` call → estimated fill cost in wei.
    async fn fetch_estimate(&self, url: &str, chain: ChainId) -> anyhow::Result<U256> {
        use anyhow::Context;
        let body = self
            .http
            .post(url)
            .json(&fee_history_request())
            .send()
            .await
            .context("eth_feeHistory send")?
            .text()
            .await
            .context("eth_feeHistory body")?;
        let (base_fee, priority) = parse_fee_history(&body)
            .with_context(|| format!("parsing eth_feeHistory for {chain}"))?;
        Ok(U256::from(estimate_fill_wei(base_fee, priority)))
    }

    /// Live-only estimate: `Some(wei)` from `eth_feeHistory`, or `None` if no
    /// RPC is configured for `chain` or the call fails. Successful reads are
    /// cached (12s TTL). The ladder uses this to decide whether to fall through;
    /// standalone use goes through [`GasModel::fill_gas`], which adds the loud
    /// static fallback. No fabricated value on failure (CLAUDE.md #2) — `None`.
    pub async fn try_fill_gas(&self, chain: ChainId) -> Option<U256> {
        if let Some(v) = self.cached(chain) {
            return Some(v);
        }
        let url = self.rpc_urls.get(&chain)?; // no RPC for this chain → None
        match self.fetch_estimate(url, chain).await {
            Ok(wei) => {
                self.store(chain, wei);
                Some(wei)
            }
            Err(e) => {
                warn!(%chain, error = %e, "live gas fetch failed");
                None
            }
        }
    }
}

/// Runtime selection between static and live gas. The `GasModel` counterpart of
/// `PriceSource`: one concrete type for the Router whether or not `--live-gas`
/// is set, no trait objects.
pub enum GasSource {
    Static(StaticGasModel),
    Live(FeeHistoryGasModel),
}

impl GasSource {
    /// Short label for the `gas=…` summary line.
    pub fn label(&self) -> &'static str {
        match self {
            GasSource::Static(_) => "static",
            GasSource::Live(_) => "live-feehistory",
        }
    }
}

#[async_trait]
impl GasModel for GasSource {
    async fn fill_gas(&self, chain: ChainId) -> GasEstimate {
        match self {
            GasSource::Static(g) => g.fill_gas(chain).await,
            GasSource::Live(g) => g.fill_gas(chain).await,
        }
    }
}

#[async_trait]
impl GasModel for FeeHistoryGasModel {
    async fn fill_gas(&self, chain: ChainId) -> GasEstimate {
        match self.try_fill_gas(chain).await {
            Some(native_wei) => GasEstimate { native_wei, origin: GasOrigin::FeeHistory },
            None => {
                // No RPC for this chain, or the fetch failed — the loud static
                // fallback (rule #7), tagged Static so the reader sees it.
                warn!(%chain, "no live gas (no RPC or fetch failed) — using static gas");
                self.fallback.fill_gas(chain).await
            }
        }
    }
}

/// `ACROSS_RPC_<CHAIN>` env var name for `chain` (e.g. `ACROSS_RPC_BASE`),
/// matching the `ACROSS_WS_<CHAIN>` convention (`chain.name()` upper-cased).
pub fn across_rpc_var(chain: ChainId) -> String {
    format!("ACROSS_RPC_{}", chain.name().to_uppercase())
}

/// The canonical `eth_feeHistory(5 blocks, latest, [50th pct])` request — 5
/// recent blocks, the 50th-percentile priority reward. A free function so the
/// wire shape is fixed (and could be asserted), like the WS subscribe request.
fn fee_history_request() -> Value {
    json!({"jsonrpc":"2.0","id":1,"method":"eth_feeHistory","params":["0x5","latest",[50]]})
}

/// `(base_fee_wei, priority_fee_wei)` from an `eth_feeHistory` response. Pure
/// (no network) so it is unit-tested against the **real** Base/Arbitrum bodies
/// captured on 2026-06-23. `base_fee` is the most recent (last) `baseFeePerGas`
/// entry — `feeHistory` returns `blockCount+1` of them, the last being the next
/// block's base fee; `priority` is the mean of the per-block 50th-pct rewards.
fn parse_fee_history(body: &str) -> anyhow::Result<(u128, u128)> {
    let v: Value = serde_json::from_str(body)?;
    if let Some(err) = v.get("error") {
        anyhow::bail!("eth_feeHistory rejected: {err}");
    }
    let result = v
        .get("result")
        .ok_or_else(|| anyhow::anyhow!("eth_feeHistory: no result"))?;
    let base_list = result
        .get("baseFeePerGas")
        .and_then(Value::as_array)
        .filter(|a| !a.is_empty())
        .ok_or_else(|| anyhow::anyhow!("eth_feeHistory: no baseFeePerGas"))?;
    let base_fee = base_list
        .last()
        .and_then(Value::as_str)
        .and_then(parse_hex_u128)
        .ok_or_else(|| anyhow::anyhow!("eth_feeHistory: unparsable baseFeePerGas"))?;

    // reward is an array (one per block) of arrays (one per requested
    // percentile). We asked for a single percentile, so take `[0]` of each and
    // average — smoother than a single block's spike. Absent on some chains
    // (e.g. an L2 with no priority fee) → priority 0.
    let priority = match result.get("reward").and_then(Value::as_array) {
        Some(blocks) if !blocks.is_empty() => {
            let mut sum = 0u128;
            let mut n = 0u128;
            for b in blocks {
                if let Some(p) = b.get(0).and_then(Value::as_str).and_then(parse_hex_u128) {
                    sum = sum.saturating_add(p);
                    n += 1;
                }
            }
            if n > 0 { sum / n } else { 0 }
        }
        _ => 0,
    };
    Ok((base_fee, priority))
}

/// Estimated fill cost in wei: `(base_fee + priority) × typical_fill_gas`.
/// Saturating so a pathological RPC value can't panic (it would just over-state
/// gas, which is conservative).
fn estimate_fill_wei(base_fee: u128, priority: u128) -> u128 {
    base_fee
        .saturating_add(priority)
        .saturating_mul(TYPICAL_FILL_GAS)
}

/// Parse a `0x`-prefixed hex string into `u128` (the wei fields in
/// `eth_feeHistory`). `None` on bad input.
fn parse_hex_u128(s: &str) -> Option<u128> {
    let body = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X"))?;
    u128::from_str_radix(body, 16).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    // Real eth_feeHistory responses captured 2026-06-23 (curl against the
    // public Base / Arbitrum RPCs) — independent on-chain truth (rule #3).
    const BASE_BODY: &str = r#"{"jsonrpc":"2.0","result":{"baseFeePerBlobGas":["0x1","0x1","0x1","0x1","0x1","0x1"],"baseFeePerGas":["0x4c4b40","0x4c4b40","0x4c4b40","0x4c4b40","0x4c4b40","0x4c4b40"],"blobGasUsedRatio":[0,0,0,0,0],"gasUsedRatio":[0.07861741,0.0900573825,0.0418887625,0.092119905,0.073150265],"oldestBlock":"0x2d7df14","reward":[["0x10c8e0"],["0x2625a0"],["0x1cfde0"],["0xf4240"],["0xf4240"]]},"id":1}"#;
    const ARB_BODY: &str = r#"{"jsonrpc":"2.0","id":1,"result":{"oldestBlock":"0x1c655258","reward":[["0x0"],["0x0"],["0x0"],["0x0"],["0x0"]],"baseFeePerGas":["0x1317b20","0x131f820","0x13255e0","0x1312d00","0x1312d00","0x1314470"],"gasUsedRatio":[1,1,0.0031000714285714286,0.013617428571428572,0.03032707142857143]}}"#;

    /// `parse_fee_history` on the real Base body: base_fee is the LAST
    /// baseFeePerGas (0x4c4b40 = 5,000,000 wei), priority is the mean of the
    /// per-block p50 rewards: (0x10c8e0+0x2625a0+0x1cfde0+0xf4240+0xf4240)/5.
    /// Numbers hand-computed from the hex, not read back.
    #[test]
    fn parse_real_base_fee_history() {
        let (base, priority) = parse_fee_history(BASE_BODY).unwrap();
        assert_eq!(base, 5_000_000); // 0x4c4b40
        // (1,100,000 + 2,500,000 + 1,900,000 + 1,000,000 + 1,000,000) / 5
        assert_eq!(priority, 7_500_000 / 5);
    }

    /// Arbitrum: base_fee = last baseFeePerGas (0x1314470 = 20,006,000 wei);
    /// reward all 0x0 → priority 0 (no priority fee on Arb). Guards the
    /// zero-reward branch behaviorally.
    #[test]
    fn parse_real_arbitrum_fee_history_zero_priority() {
        let (base, priority) = parse_fee_history(ARB_BODY).unwrap();
        assert_eq!(base, 0x1314470);
        assert_eq!(priority, 0);
    }

    /// A JSON-RPC error envelope and a missing `result` both surface as `Err`
    /// (→ the caller falls back to static), never a silent 0.
    #[test]
    fn parse_rejects_error_and_missing_result() {
        let err = r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32601,"message":"method not found"}}"#;
        assert!(parse_fee_history(err).is_err());
        assert!(parse_fee_history(r#"{"jsonrpc":"2.0","id":1}"#).is_err());
    }

    /// The estimate is `(base + priority) × 300k`. On the real Base numbers
    /// that is (5,000,000 + 1,500,000) × 300,000 = 1.95e12 wei ≈ 0.00000195 ETH
    /// — the small, live L2 cost the static $0.28-ish hard-code overshoots.
    #[test]
    fn estimate_uses_typical_gas_units() {
        assert_eq!(estimate_fill_wei(5_000_000, 1_500_000), 6_500_000 * 300_000);
        // Saturating: an absurd base fee can't panic.
        assert_eq!(estimate_fill_wei(u128::MAX, u128::MAX), u128::MAX);
    }

    /// `parse_hex_u128` accepts `0x`/`0X`, rejects garbage.
    #[test]
    fn hex_parse() {
        assert_eq!(parse_hex_u128("0x4c4b40"), Some(5_000_000));
        assert_eq!(parse_hex_u128("0X10"), Some(16));
        assert_eq!(parse_hex_u128("zzz"), None);
        assert_eq!(parse_hex_u128("4c4b40"), None); // no prefix → reject
    }

    /// Env var name matches the `ACROSS_RPC_<CHAIN>` convention.
    #[test]
    fn rpc_var_spelling() {
        assert_eq!(across_rpc_var(ChainId::BASE), "ACROSS_RPC_BASE");
        assert_eq!(across_rpc_var(ChainId::ARBITRUM), "ACROSS_RPC_ARBITRUM");
    }

    /// With NO RPC configured, the live model returns exactly the static gas —
    /// the loud fallback, no network, no panic. Asserts equality against the
    /// independent `StaticGasModel` (not itself).
    #[tokio::test]
    async fn no_rpc_falls_back_to_static() {
        let live = FeeHistoryGasModel::new(HashMap::new());
        let stat = StaticGasModel;
        for chain in [ChainId::BASE, ChainId::ARBITRUM, ChainId::ETHEREUM] {
            assert_eq!(
                live.fill_gas_native(chain).await,
                stat.fill_gas_native(chain).await,
                "no-RPC live gas must equal static for {chain}"
            );
        }
    }

    /// LP-6: `GasSource` is a pure dispatch — Live (no RPC → static fallback)
    /// equals Static, and the labels are the stable summary strings.
    #[tokio::test]
    async fn gas_source_dispatch() {
        let live = GasSource::Live(FeeHistoryGasModel::new(HashMap::new()));
        let stat = GasSource::Static(StaticGasModel);
        for chain in [ChainId::BASE, ChainId::ARBITRUM] {
            assert_eq!(live.fill_gas_native(chain).await, stat.fill_gas_native(chain).await);
        }
        assert_eq!(stat.label(), "static");
        assert_eq!(live.label(), "live-feehistory");
    }
}
