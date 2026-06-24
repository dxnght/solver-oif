//! Gas-cost model.
//!
//! Phase 0 uses **static per-chain estimates** of (gas_used × gas_price)
//! for a typical fill transaction (≈300k gas) at the chain's median price
//! over a recent window. This is a coarse model — sufficient to rank
//! routes, not to size individual trades. Live `eth_feeHistory` gas is a
//! later bucket (B3), behind a flag, default off.

use alloy_primitives::U256;
use async_trait::async_trait;
use solver_types::{ChainId, GasOrigin};

/// Gas units for one typical fill tx (`gas_used`). The gas *price* varies per
/// source; the *units* are this single shared constant (used by every model)
/// so live and static estimates differ in exactly one axis — and so no result
/// is fudged by quietly tuning the units (CLAUDE.md #2).
pub const TYPICAL_FILL_GAS: u128 = 300_000;

/// A wei gas estimate plus the model that produced it. Carrying the source is
/// what makes the gas number legible downstream (CLAUDE.md #7): a reader can
/// tell a live oracle reading from a static fallback.
#[derive(Debug, Clone, Copy)]
pub struct GasEstimate {
    /// Cost of one fill tx in the dest chain's native token, in wei.
    pub native_wei: U256,
    /// Which model produced `native_wei`.
    pub origin: GasOrigin,
}

/// Wei-denominated gas cost for one fill transaction on `chain`, with source.
#[async_trait]
pub trait GasModel: Send + Sync {
    /// Gas cost for one fill on `chain`, tagged with the model that produced it.
    async fn fill_gas(&self, chain: ChainId) -> GasEstimate;

    /// Convenience for callers that only need the wei amount (route hops, the
    /// static fallback path). Defaults to `fill_gas(...).native_wei`.
    async fn fill_gas_native(&self, chain: ChainId) -> U256 {
        self.fill_gas(chain).await.native_wei
    }
}

/// Static lookup table. Values are rough medians at time of writing —
/// expand or back with an RPC `eth_feeHistory` call in bucket B3.
///
/// All numbers in wei. Convert to USD via the native-token price oracle
/// in the Router.
pub struct StaticGasModel;

#[async_trait]
impl GasModel for StaticGasModel {
    async fn fill_gas(&self, chain: ChainId) -> GasEstimate {
        let wei = match chain {
            ChainId::ETHEREUM => 9_000_000_000_000_000_u128,   // 0.009 ETH at ~30 gwei × 300k gas
            ChainId::ARBITRUM => 30_000_000_000_000_u128,      // 0.00003 ETH
            ChainId::OPTIMISM => 60_000_000_000_000_u128,      // 0.00006 ETH
            ChainId::BASE     => 50_000_000_000_000_u128,      // 0.00005 ETH
            ChainId::POLYGON  => 15_000_000_000_000_000_000_u128, // 0.015 MATIC
            ChainId::ZKSYNC   => 50_000_000_000_000_u128,
            ChainId::LINEA    => 60_000_000_000_000_u128,
            ChainId::SCROLL   => 50_000_000_000_000_u128,
            ChainId::MODE     => 30_000_000_000_000_u128,
            ChainId::BLAST    => 30_000_000_000_000_u128,
            ChainId::LISK     => 30_000_000_000_000_u128,
            ChainId::REDSTONE => 30_000_000_000_000_u128,
            ChainId::WORLDCHAIN => 50_000_000_000_000_u128,
            ChainId::ZORA     => 50_000_000_000_000_u128,
            ChainId::INK      => 30_000_000_000_000_u128,
            ChainId::LENS     => 30_000_000_000_000_u128,
            ChainId::SONEIUM  => 30_000_000_000_000_u128,
            ChainId::UNICHAIN => 30_000_000_000_000_u128,
            // Conservative fallback: at least as costly as the priciest known
            // L2 (Optimism, above), so an unknown chain is never optimistically
            // *under*-priced — the invariant the gas test guards. (Was 9e12,
            // cheaper than every named L2, which under-priced unknown chains.)
            _ => 60_000_000_000_000_u128,
        };
        GasEstimate { native_wei: U256::from(wei), origin: GasOrigin::Static }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Properties, not table-readback: an L2 fill is cheaper than an L1 fill,
    /// and an unrecognized chain falls back to the conservative estimate
    /// (which must be no cheaper than a known cheap L2 — so unknown chains are
    /// never optimistically under-priced). Guards the fallback branch and the
    /// L1-vs-L2 ordering the router relies on, without asserting the exact
    /// constants against themselves.
    #[tokio::test]
    async fn l2_is_cheaper_than_l1_and_unknown_falls_back_conservatively() {
        let g = StaticGasModel;
        let eth = g.fill_gas_native(ChainId::ETHEREUM).await;
        let base = g.fill_gas_native(ChainId::BASE).await;
        let arb = g.fill_gas_native(ChainId::ARBITRUM).await;
        assert!(base < eth, "base fill should be cheaper than ethereum");
        assert!(arb < eth, "arbitrum fill should be cheaper than ethereum");

        let unknown = g.fill_gas_native(ChainId(424_242)).await;
        assert!(
            unknown >= arb,
            "unknown chain must not be priced below a known cheap L2"
        );
    }
}
