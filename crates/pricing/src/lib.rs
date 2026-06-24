//! Pricing — gas and asset sourcing for quotes. See `solver-engine-spec.md` §4.
//!
//! The Router asks: *what would it cost to acquire `target_amount` of token Y
//! on chain C right now?* and *what does a fill tx cost in gas?*
//!
//! Verified-core scope (bucket B0): **offline / static only**, so the backtest
//! is deterministic and CI never reaches the network. Live DEX-aggregator
//! sourcing (odos) and live gas (`eth_feeHistory`) return in later buckets,
//! behind a flag, default off — never as a horizontal sweep (CLAUDE.md rule #5).

mod etherscan_gas;
mod fee_history_gas;
mod gas;
mod offline;

pub use etherscan_gas::EtherscanGasModel;
pub use fee_history_gas::{across_rpc_var, FeeHistoryGasModel, GasSource, GAS_RPC_CHAINS};
pub use gas::{GasEstimate, GasModel, StaticGasModel, TYPICAL_FILL_GAS};
pub use offline::OfflineIdentityPricer;

use alloy_primitives::{Address, U256};
use async_trait::async_trait;
use solver_types::ChainId;

/// A token swap quote on a single chain.
#[derive(Debug, Clone)]
pub struct SwapQuote {
    pub chain: ChainId,
    pub sell_token: Address,
    pub buy_token: Address,
    pub sell_amount: U256,
    pub buy_amount: U256,
    /// Gas estimate for executing the swap.
    pub gas_native: U256,
    /// Source attribution ("odos", "lifi", "synthetic"...).
    pub provider: &'static str,
}

#[derive(Debug, thiserror::Error)]
pub enum PriceError {
    #[error("no route found")]
    NoRoute,
    #[error("provider error: {0}")]
    Provider(String),
    #[error("http: {0}")]
    Http(#[from] reqwest::Error),
    #[error("decode: {0}")]
    Decode(String),
}

#[async_trait]
pub trait Pricer: Send + Sync {
    /// Quote selling `sell_amount` of `sell_token` for as much `buy_token`
    /// as the aggregator can produce, on `chain`.
    async fn quote_sell_exact_in(
        &self,
        chain: ChainId,
        sell_token: Address,
        buy_token: Address,
        sell_amount: U256,
    ) -> Result<SwapQuote, PriceError>;
}
