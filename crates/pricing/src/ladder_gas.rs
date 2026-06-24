//! The gas ladder: Etherscan `gasoracle` → `eth_feeHistory` → static, in that
//! order of trust. A rung is consulted only if the one above has no live number
//! for the chain; the static floor always answers. The resulting
//! [`GasEstimate::origin`] records which rung actually produced the number, so
//! a reader can tell a live reading from a fallback (CLAUDE.md #7).
//!
//! Each live rung self-resolves its credentials (Etherscan key / `ACROSS_RPC_*`)
//! and returns `None` for a chain it cannot price — so a chain neither covers
//! simply reaches the static floor, with no fabricated price (CLAUDE.md #2).

use alloy_primitives::U256;
use async_trait::async_trait;
use solver_types::{ChainId, GasOrigin};

use crate::gas::{GasEstimate, GasModel, StaticGasModel};
use crate::{EtherscanGasModel, FeeHistoryGasModel};

/// Composite gas model: try each live rung top-down, fall to static.
pub struct LadderGasModel {
    etherscan: EtherscanGasModel,
    /// `None` disables the feeHistory rung entirely (the ladder is then just
    /// etherscan → static). `Some` still prices only the chains with an RPC.
    fee_history: Option<FeeHistoryGasModel>,
    fallback: StaticGasModel,
}

impl LadderGasModel {
    /// Build from explicit rungs — the test seam.
    pub fn new(etherscan: EtherscanGasModel, fee_history: Option<FeeHistoryGasModel>) -> Self {
        Self { etherscan, fee_history, fallback: StaticGasModel }
    }

    /// Production ladder: both live rungs from env. Each self-resolves its
    /// credentials; a chain neither covers reaches the static floor.
    pub fn from_env() -> Self {
        Self::new(EtherscanGasModel::from_env(), Some(FeeHistoryGasModel::from_env()))
    }
}

/// Pure rung selection: the first live `Some` wins (etherscan over feeHistory),
/// else the static floor. Split out so the order/origin is tested without the
/// network.
fn pick(etherscan: Option<U256>, fee_history: Option<U256>, static_wei: U256) -> GasEstimate {
    if let Some(native_wei) = etherscan {
        return GasEstimate { native_wei, origin: GasOrigin::Etherscan };
    }
    if let Some(native_wei) = fee_history {
        return GasEstimate { native_wei, origin: GasOrigin::FeeHistory };
    }
    GasEstimate { native_wei: static_wei, origin: GasOrigin::Static }
}

#[async_trait]
impl GasModel for LadderGasModel {
    async fn fill_gas(&self, chain: ChainId) -> GasEstimate {
        let etherscan = self.etherscan.try_fill_gas(chain).await;
        let fee_history = match &self.fee_history {
            Some(fh) => fh.try_fill_gas(chain).await,
            None => None,
        };
        // The static floor is cheap (no network) and always defined.
        let static_wei = self.fallback.fill_gas(chain).await.native_wei;
        pick(etherscan, fee_history, static_wei)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// Rung order and the origin tag: etherscan wins over feeHistory wins over
    /// static, and each branch stamps the right source. Pure — the real
    /// fallthrough invariant, no network.
    #[test]
    fn pick_prefers_higher_rungs_and_tags_origin() {
        let a = U256::from(100u64);
        let b = U256::from(200u64);
        let s = U256::from(300u64);

        let top = pick(Some(a), Some(b), s);
        assert_eq!((top.native_wei, top.origin), (a, GasOrigin::Etherscan));

        let mid = pick(None, Some(b), s);
        assert_eq!((mid.native_wei, mid.origin), (b, GasOrigin::FeeHistory));

        let floor = pick(None, None, s);
        assert_eq!((floor.native_wei, floor.origin), (s, GasOrigin::Static));
    }

    /// End to end with no credentials: no Etherscan key and no RPC for any
    /// chain → both live rungs return `None` → the ladder returns the static
    /// estimate, tagged `Static` (asserted against an independent model).
    #[tokio::test]
    async fn no_credentials_reaches_static_floor() {
        let ladder = LadderGasModel::new(
            EtherscanGasModel::new(None),
            Some(FeeHistoryGasModel::new(HashMap::new())),
        );
        let stat = StaticGasModel;
        for chain in [ChainId::ETHEREUM, ChainId::ARBITRUM, ChainId::OPTIMISM] {
            let est = ladder.fill_gas(chain).await;
            assert_eq!(est.origin, GasOrigin::Static, "no-creds ladder must hit static for {chain}");
            assert_eq!(est.native_wei, stat.fill_gas(chain).await.native_wei);
        }
    }
}
