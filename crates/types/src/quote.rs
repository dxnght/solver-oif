use alloy_primitives::U256;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::amount::{AssetAmount, TokenRef};
use crate::chain::ChainId;
use crate::intent::IntentId;

/// Wei amount in the native gas token of a chain. Treated as a scalar — gas
/// cost gets converted to the output token's value via [`crate::Quote::cost_sourcing`].
pub type Wei = U256;

/// Where a single leg of the route gets its liquidity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RouteSource {
    /// Direct swap on a DEX aggregator on `chain` (0x/1inch/LI.FI).
    Aggregator,
    /// JIT-sourced from another chain via a bridge.
    Bridge,
    /// Quote from a market maker over RFQ.
    Rfq,
    /// Already in inventory on `chain`.
    Inventory,
}

/// One step in the route the filler will take to deliver `output_min`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RouteHop {
    pub chain: ChainId,
    pub source: RouteSource,
    /// What the hop consumes.
    pub input: AssetAmount,
    /// What the hop produces.
    pub output: AssetAmount,
    /// Gas cost of executing this hop in the chain's native token.
    pub gas_native: Wei,
}

/// Complete execution path for one intent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Route {
    pub hops: Vec<RouteHop>,
}

impl Route {
    pub fn empty() -> Self {
        Self { hops: vec![] }
    }
}

/// Output of the Quoter/Router. The Bid-decision stage filters on this
/// via [`crate::FillPolicy`] — fills happen only when
/// `expected_profit_usd > min_margin_usd` with sufficient `confidence`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Quote {
    pub intent_id: IntentId,
    pub route: Route,
    /// Output we expect to land on dest chain after the route executes.
    pub expected_output: AssetAmount,
    /// Total gas across all hops, accumulated in the chain-native unit of
    /// the chain where each hop runs. Kept per-chain because gas tokens differ.
    pub cost_gas: Vec<(ChainId, Wei)>,
    /// What we pay to source the output asset. Denominated in the output token.
    pub cost_sourcing: AssetAmount,
    /// `expected_output - output_min - sum(costs in output-token terms)`.
    /// Positive means the trade prints; negative means we'd lose.
    /// Denominated in the output token, signed (we serialize as i128 of base units).
    pub expected_profit: SignedAmount,
    /// Same quantity as `expected_profit`, pre-converted to USD by the
    /// Router's price feed. **This is the only field aggregation code may
    /// read for USD math** (invariant 5 / P0-B): `expected_profit` is in
    /// output-token units and summing it as dollars silently understates
    /// WETH routes ~3500x and WBTC routes ~60000x.
    pub expected_profit_usd: f64,
    /// Total gas cost in USD, summed across all hops (origin + dest). The
    /// router already computes this internally; surfacing it lets the live
    /// loop log it (RES-3) and lets the backtest report a net-of-gas
    /// realized margin alongside the price-only one.
    pub gas_usd: f64,
    /// `[0, 1]` — how confident we are in `expected_profit` (price stability,
    /// latency assumptions, slippage model fit).
    pub confidence: f64,
    pub valid_until: DateTime<Utc>,
}

/// Signed token amount. We can't use `i256` directly; carry token + magnitude
/// + sign separately.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignedAmount {
    pub token: TokenRef,
    pub magnitude: U256,
    pub decimals: u8,
    pub negative: bool,
}

impl SignedAmount {
    pub fn zero(token: TokenRef, decimals: u8) -> Self {
        Self { token, magnitude: U256::ZERO, decimals, negative: false }
    }

    pub fn to_f64(self) -> f64 {
        let mag = AssetAmount::new(self.token, self.magnitude, self.decimals).to_f64();
        if self.negative { -mag } else { mag }
    }

    pub fn is_positive(self) -> bool {
        !self.negative && self.magnitude > U256::ZERO
    }
}
