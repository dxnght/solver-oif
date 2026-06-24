//! Live gas via Etherscan V2 `gastracker/gasoracle` — the top rung of the gas
//! ladder, above `eth_feeHistory`.
//!
//! Etherscan V2 is one host with `chainid` as a query param (same base the
//! Across ingest client already uses). The `gasoracle` action returns the
//! current suggested gas *price* in Gwei; we price the same typical fill
//! (`gas_price × TYPICAL_FILL_GAS`) at it.
//!
//! Honesty (CLAUDE.md #2 / #7): no `ETHERSCAN_API_KEY`, a chain the key's plan
//! does not cover (`status != "1"`), or any parse/HTTP error all yield `None`
//! — the ladder falls through to feeHistory/static. Never a fabricated price.
//! Verified live on 2026-06-24: with the project key, `status:"1"` for
//! ethereum/polygon/arbitrum; `status:"0"` ("Free API access is not supported
//! for this chain") for optimism/base. The parser is tested against those real
//! captured bodies (rule #3).

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use alloy_primitives::U256;
use serde_json::Value;
use solver_types::ChainId;
use tracing::warn;

use crate::gas::{GasEstimate, GasModel, StaticGasModel, TYPICAL_FILL_GAS};
use async_trait::async_trait;
use solver_types::GasOrigin;

/// Etherscan V2 multichain base (one host; `chainid` selects the chain),
/// matching the Across ingest client.
const ETHERSCAN_V2_BASE: &str = "https://api.etherscan.io/v2";

/// Per-chain cache TTL. Gas prices move per block and the free tier is 5 req/s
/// shared, so 12s keeps a busy stream off the API while staying fresh. Both
/// `Some` (a price) and `None` (uncovered/error) outcomes are cached, so an
/// unsupported chain is not re-polled on every intent.
const CACHE_TTL: Duration = Duration::from_secs(12);

/// Live gas model over Etherscan's `gasoracle`. A static fallback only for the
/// standalone [`GasModel`] impl; the ladder calls [`Self::try_fill_gas`] and
/// decides fallthrough itself.
pub struct EtherscanGasModel {
    /// `None` → the model never touches the network and always returns `None`.
    api_key: Option<String>,
    http: reqwest::Client,
    cache: Mutex<HashMap<ChainId, (Instant, Option<U256>)>>,
}

impl EtherscanGasModel {
    /// Build with an explicit key — the test seam (`None` exercises the
    /// no-key path without touching the process env).
    pub fn new(api_key: Option<String>) -> Self {
        Self {
            api_key,
            // 8s per-request timeout (a slow/dead API errors → ladder
            // fallthrough, not a silent stall — rule #7); a User-Agent because
            // some Etherscan edges 403 a UA-less request.
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(8))
                .user_agent("solver-gas/0.0.1")
                .build()
                .unwrap_or_default(),
            cache: Mutex::new(HashMap::new()),
        }
    }

    /// Production model: read `ETHERSCAN_API_KEY` (empty → treated as absent).
    pub fn from_env() -> Self {
        let key = std::env::var("ETHERSCAN_API_KEY")
            .ok()
            .filter(|v| !v.trim().is_empty());
        Self::new(key)
    }

    fn cached(&self, chain: ChainId) -> Option<Option<U256>> {
        let cache = self.cache.lock().expect("etherscan gas cache poisoned");
        cache
            .get(&chain)
            .and_then(|(t, v)| (t.elapsed() < CACHE_TTL).then_some(*v))
    }

    fn store(&self, chain: ChainId, value: Option<U256>) {
        self.cache
            .lock()
            .expect("etherscan gas cache poisoned")
            .insert(chain, (Instant::now(), value));
    }

    /// Live-only native-wei estimate via `gasoracle`. `None` when there is no
    /// key, the chain is not covered (`status != "1"`), or the call/parse
    /// fails. Caches the outcome (`Some` or `None`) for [`CACHE_TTL`].
    pub async fn try_fill_gas(&self, chain: ChainId) -> Option<U256> {
        let key = self.api_key.as_deref()?; // no key → None, no network
        if let Some(v) = self.cached(chain) {
            return v;
        }
        let value = match self.fetch(chain, key).await {
            Ok(wei) => Some(wei),
            Err(e) => {
                warn!(%chain, error = %e, "etherscan gas unavailable — falling through");
                None
            }
        };
        self.store(chain, value);
        value
    }

    /// One `gasoracle` GET → native-wei cost of one fill.
    async fn fetch(&self, chain: ChainId, key: &str) -> anyhow::Result<U256> {
        use anyhow::Context;
        let url = format!(
            "{ETHERSCAN_V2_BASE}/api?chainid={cid}&module=gastracker&action=gasoracle&apikey={key}",
            cid = chain.0,
        );
        let body = self
            .http
            .get(&url)
            .send()
            .await
            .context("gasoracle send")?
            .text()
            .await
            .context("gasoracle body")?;
        let gas_price_wei = parse_gas_price_wei(&body)
            .with_context(|| format!("parsing gasoracle for {chain}"))?;
        Ok(U256::from(fill_cost_wei(gas_price_wei)))
    }
}

#[async_trait]
impl GasModel for EtherscanGasModel {
    /// Standalone use: live price when covered, else the loud static fallback
    /// (tagged `Static`), mirroring [`crate::FeeHistoryGasModel`].
    async fn fill_gas(&self, chain: ChainId) -> GasEstimate {
        match self.try_fill_gas(chain).await {
            Some(native_wei) => GasEstimate { native_wei, origin: GasOrigin::Etherscan },
            None => StaticGasModel.fill_gas(chain).await,
        }
    }
}

/// Cost of one typical fill at `gas_price_wei` (per-gas) — `× TYPICAL_FILL_GAS`.
/// Saturating so a pathological price can't panic (it would over-state cost,
/// which is conservative).
fn fill_cost_wei(gas_price_wei: u128) -> u128 {
    gas_price_wei.saturating_mul(TYPICAL_FILL_GAS)
}

/// Per-gas price in wei from a `gasoracle` body, or `Err` if the chain is not
/// covered. `status` must be the string `"1"`; otherwise `result` is an error
/// *string* (e.g. "Free API access is not supported for this chain"), not the
/// price object, so we must gate on it before reading `ProposeGasPrice`.
/// `ProposeGasPrice` is a **decimal** Gwei string (e.g. `"1.334315419"`,
/// `"0.021168"`), so wei = round(gwei × 1e9).
fn parse_gas_price_wei(body: &str) -> anyhow::Result<u128> {
    let v: Value = serde_json::from_str(body)?;
    let status = v.get("status").and_then(Value::as_str);
    if status != Some("1") {
        let msg = v
            .get("result")
            .and_then(Value::as_str)
            .or_else(|| v.get("message").and_then(Value::as_str))
            .unwrap_or("non-ok status");
        anyhow::bail!("gasoracle status != 1: {msg}");
    }
    let propose = v
        .get("result")
        .and_then(|r| r.get("ProposeGasPrice"))
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("gasoracle: no ProposeGasPrice"))?;
    let gwei: f64 = propose
        .trim()
        .parse()
        .map_err(|_| anyhow::anyhow!("gasoracle: ProposeGasPrice not a number: {propose}"))?;
    if !gwei.is_finite() || gwei < 0.0 {
        anyhow::bail!("gasoracle: bad gwei {gwei}");
    }
    // Gwei carries ≤9 decimals, so × 1e9 is effectively exact for real values.
    Ok((gwei * 1e9).round() as u128)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Real Etherscan V2 gasoracle bodies captured 2026-06-24 (curl against the
    // V2 host) — independent on-chain-ish truth (rule #3). ETH/ARB return a
    // price; OP is uncovered by the key's plan.
    const ETH_OK: &str = r#"{"status":"1","message":"OK","result":{"LastBlock":"25389216","SafeGasPrice":"1.234315419","ProposeGasPrice":"1.334315419","FastGasPrice":"1.35774696","suggestBaseFee":"1.234315419","gasUsedRatio":"0.5894325,0.418971833333333"}}"#;
    const ARB_OK: &str = r#"{"status":"1","message":"OK","result":{"LastBlock":"476931662","SafeGasPrice":"0.021168","ProposeGasPrice":"0.021168","FastGasPrice":"0.021168","suggestBaseFee":"0.021168","gasUsedRatio":"0.8394"}}"#;
    const OP_NOTOK: &str = r#"{"status":"0","message":"NOTOK","result":"Free API access is not supported for this chain. Please upgrade your api plan for full chain coverage. https://etherscan.io/apis"}"#;
    const INK_NOTOK: &str = r#"{"status":"0","message":"NOTOK","result":"Missing or unsupported chainid parameter (required for v2 api), please see https://api.etherscan.io/v2/chainlist for the list of supported chainids"}"#;

    /// ETH: ProposeGasPrice "1.334315419" Gwei → 1,334,315,419 wei per gas
    /// (hand-computed: 1.334315419 × 1e9). Not read back from the constant.
    #[test]
    fn parse_real_eth_gasoracle() {
        assert_eq!(parse_gas_price_wei(ETH_OK).unwrap(), 1_334_315_419);
    }

    /// ARB: a sub-1-Gwei price "0.021168" → 21,168,000 wei per gas — exercises
    /// the decimal-Gwei path the integer-only naive parse would drop.
    #[test]
    fn parse_real_arbitrum_subgwei() {
        assert_eq!(parse_gas_price_wei(ARB_OK).unwrap(), 21_168_000);
    }

    /// A `status:"0"` body (uncovered chain) is an `Err`, never a silent 0 or a
    /// crash on `result` being a string instead of the price object.
    #[test]
    fn parse_rejects_uncovered_chain() {
        assert!(parse_gas_price_wei(OP_NOTOK).is_err());
        assert!(parse_gas_price_wei(INK_NOTOK).is_err());
    }

    /// Native fill cost is the per-gas price × the shared typical-fill units.
    #[test]
    fn fill_cost_multiplies_by_typical_units() {
        assert_eq!(fill_cost_wei(1_334_315_419), 1_334_315_419 * TYPICAL_FILL_GAS);
        // Saturating: an absurd price can't panic.
        assert_eq!(fill_cost_wei(u128::MAX), u128::MAX);
    }

    /// With no key the model never touches the network and returns `None` for
    /// every chain — the ladder then falls through (rule #2: no fabrication).
    #[tokio::test]
    async fn no_key_returns_none() {
        let m = EtherscanGasModel::new(None);
        for chain in [ChainId::ETHEREUM, ChainId::ARBITRUM, ChainId::OPTIMISM] {
            assert_eq!(m.try_fill_gas(chain).await, None, "no-key must be None for {chain}");
        }
    }

    /// Standalone `fill_gas` with no key is the loud static fallback, tagged
    /// `Static` (asserted against an independent `StaticGasModel`).
    #[tokio::test]
    async fn fill_gas_no_key_is_static() {
        let m = EtherscanGasModel::new(None);
        let est = m.fill_gas(ChainId::ETHEREUM).await;
        assert_eq!(est.origin, GasOrigin::Static);
        assert_eq!(est.native_wei, StaticGasModel.fill_gas(ChainId::ETHEREUM).await.native_wei);
    }
}
