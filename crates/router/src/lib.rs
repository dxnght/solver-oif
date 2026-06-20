//! Router — the edge core. Turns a [`solver_types::NormalizedIntent`] into a
//! [`solver_types::Quote`] (or an explicit skip). See `CLAUDE.md` §Архітектура.
//!
//! Verified-core scope (bucket B0): **RouterV0** — asset-class routing, Dutch
//! decay, a confidence number, and `QuoteOutcome`/`SkipReason`. It produces a
//! quote; the *fill* decision is made downstream by the single
//! [`solver_types::FillPolicy`] so "would we fill?" means exactly one thing in
//! both backtest and live (`would_fill` is a **modeled belief** until validated
//! against realized outcomes — bucket B1).
//!
//! Economics (one fill, filler's view): the filler receives the intent's
//! `input` on the origin chain and must deliver `output_min` on the dest chain,
//! paying gas for the fill. So, in USD:
//!
//! ```text
//! expected_profit_usd = input·price_in − output_min·price_out − gas_usd
//! ```
//!
//! Offline the sourcing of `output_min` is priced at par (the identity pricer),
//! which is accurate for a **same-class** swap (stable→stable) but optimistic
//! for a **cross-class** one (ETH→USDC) where real DEX slippage is unmodeled —
//! hence the lower confidence on cross-class routes.

mod prices;

pub use prices::{PriceFeed, StaticPriceFeed};

use alloy_primitives::{Address, U256};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use solver_pricing::GasModel;
use solver_types::{
    asset_class, AssetAmount, AssetClass, ChainId, DutchDecay, NormalizedIntent, Quote, Route,
    RouteHop, RouteSource, SignedAmount, TokenRef,
};

/// Confidence in a same-class route (stable→stable): both legs are valued at
/// the genuinely-accurate $1 peg and identity sourcing is at par, so the
/// modeled profit is trustworthy. Above the default fill threshold.
pub const CONF_SAME_CLASS: f64 = 0.90;
/// Confidence in a cross-class route (e.g. ETH/BTC→USDC) **on static prices**:
/// deliberately set BELOW the default `FillPolicy.min_confidence` (0.50) so
/// these never count as would_fill. Two errors make static cross-class profit
/// untrustworthy: (1) the static ETH/BTC price rarely matches the order's
/// epoch price — a 2x-off BTC price fabricates a ~100% phantom margin; (2) DEX
/// slippage on the real swap is unmodeled. We quote them (measuring economics
/// is the router's job) but refuse to call them fillable until a live feed
/// lands (bucket B3 revisits this number, feed-aware). This is the explicit
/// removal of false confidence (CORE-5), not a tuned threshold-dodge.
pub const CONF_CROSS_CLASS: f64 = 0.40;

/// Why the Router declined to quote an intent. Serialized into the
/// decisions JSONL `reason` field downstream — **do not rename** the strings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkipReason {
    /// A side's token (or the dest-chain gas token) has no known USD price
    /// offline — we refuse to invent one (CLAUDE.md rule #2).
    PriceUnknown,
    /// The intent's deadline is at/before the evaluation time — unfillable.
    Expired,
    /// Under another relayer's exclusivity window at the evaluation time.
    ExclusiveToOther,
    /// Input or required output is zero — nothing to quote.
    DustAmount,
}

impl SkipReason {
    pub fn as_str(self) -> &'static str {
        match self {
            SkipReason::PriceUnknown => "price_unknown",
            SkipReason::Expired => "expired",
            SkipReason::ExclusiveToOther => "exclusive_to_other",
            SkipReason::DustAmount => "dust_amount",
        }
    }
}

/// Result of quoting one intent: a priced [`Quote`] or an explicit skip.
/// The quote is boxed because it dwarfs `SkipReason`; keeps the enum small.
#[derive(Debug, Clone)]
pub enum QuoteOutcome {
    Quoted(Box<Quote>),
    Skipped(SkipReason),
}

impl QuoteOutcome {
    pub fn quote(&self) -> Option<&Quote> {
        match self {
            QuoteOutcome::Quoted(q) => Some(q),
            QuoteOutcome::Skipped(_) => None,
        }
    }
}

/// RouterV0. Holds a price feed and a gas model; both are static/offline in
/// Phase 0 for a deterministic backtest.
pub struct RouterV0<P, G> {
    prices: P,
    gas: G,
}

impl<P: PriceFeed, G: GasModel> RouterV0<P, G> {
    pub fn new(prices: P, gas: G) -> Self {
        Self { prices, gas }
    }

    /// Quote `intent` as of `now` (backtest passes `intent.observed_at`; live
    /// passes wall-clock). Pure given the feed + gas model — no I/O beyond the
    /// async gas lookup, which is offline in Phase 0.
    pub async fn quote(&self, intent: &NormalizedIntent, now: DateTime<Utc>) -> QuoteOutcome {
        // 1. Liveness gates (these are the only places a generic filler is
        //    genuinely locked out; for historical data `now == observed_at`,
        //    so a past deadline/exclusivity does not falsely trip).
        if intent.deadline <= now {
            return QuoteOutcome::Skipped(SkipReason::Expired);
        }
        if let Some(excl) = &intent.exclusivity {
            if now < excl.deadline {
                return QuoteOutcome::Skipped(SkipReason::ExclusiveToOther);
            }
        }
        if intent.input.amount.is_zero() || intent.output_min.amount.is_zero() {
            return QuoteOutcome::Skipped(SkipReason::DustAmount);
        }

        // 2. Value both sides + the dest-chain gas in USD; any unknown → skip.
        let in_tok = intent.input.token;
        let out_tok = intent.output_min.token;
        let (price_in, price_out) =
            match (self.prices.usd_price(in_tok), self.prices.usd_price(out_tok)) {
                (Some(a), Some(b)) => (a, b),
                _ => return QuoteOutcome::Skipped(SkipReason::PriceUnknown),
            };
        let native = TokenRef::new(intent.dest_chain, Address::ZERO);
        let native_price = match self.prices.usd_price(native) {
            Some(p) => p,
            None => return QuoteOutcome::Skipped(SkipReason::PriceUnknown),
        };

        let revenue_usd = intent.input.to_f64() * price_in;
        let sourcing_usd = intent.output_min.to_f64() * price_out;

        let gas_native = self.gas.fill_gas_native(intent.dest_chain).await;
        let gas_usd = AssetAmount::new(native, gas_native, 18).to_f64() * native_price;

        let expected_profit_usd = revenue_usd - sourcing_usd - gas_usd;

        // 3. Confidence: same value-class sources at par; cross-class carries
        //    unmodeled slippage.
        let same_class = value_class(in_tok) == value_class(out_tok)
            && value_class(in_tok) != AssetClass::Unknown;
        let confidence = if same_class { CONF_SAME_CLASS } else { CONF_CROSS_CLASS };

        // 4. Assemble the Quote. Output-token unit fields are derived from the
        //    USD figures via `price_out`; `expected_profit_usd` stays the
        //    authoritative number for aggregation (types invariant 5 / P0-B).
        let out_dec = intent.output_min.decimals;
        let route = Route {
            hops: vec![RouteHop {
                chain: intent.dest_chain,
                source: if same_class { RouteSource::Inventory } else { RouteSource::Aggregator },
                input: intent.output_min,  // what we spend to source the delivery
                output: intent.output_min, // what we hand the user
                gas_native,
            }],
        };
        let expected_profit = signed_amount_from_usd(out_tok, out_dec, expected_profit_usd, price_out);

        QuoteOutcome::Quoted(Box::new(Quote {
            intent_id: intent.id.clone(),
            route,
            expected_output: intent.output_min,
            cost_gas: vec![(intent.dest_chain, gas_native)],
            cost_sourcing: intent.output_min,
            expected_profit,
            expected_profit_usd,
            gas_usd,
            confidence,
            valid_until: intent.deadline,
        }))
    }
}

/// Value-class for confidence: native ETH on an ETH-native chain counts as
/// `Eth` (a native↔WETH route is same-class); Polygon's MATIC and unknown
/// natives stay `Unknown` (conservatively cross-class). ERC-20s defer to
/// [`asset_class`].
fn value_class(token: TokenRef) -> AssetClass {
    if token.is_native() {
        return match token.chain {
            ChainId::POLYGON => AssetClass::Unknown,
            _ => AssetClass::Eth,
        };
    }
    asset_class(token)
}

/// Convert a signed USD profit into output-token base units via `price_out`.
/// `price_out` is assumed > 0 (the caller only reaches here with a known,
/// positive price). NaN/∞ collapse to zero.
fn signed_amount_from_usd(token: TokenRef, decimals: u8, usd: f64, price_out: f64) -> SignedAmount {
    let whole = if price_out > 0.0 { usd.abs() / price_out } else { 0.0 };
    let base = whole * 10f64.powi(decimals as i32);
    let magnitude = if base.is_finite() && base >= 0.0 {
        U256::from(base as u128)
    } else {
        U256::ZERO
    };
    SignedAmount {
        token,
        magnitude,
        decimals,
        negative: usd < 0.0,
    }
}

/// Output the Dutch auction offers at time `t`, linearly interpolated and
/// clamped to the `[start_time, end_time]` band. Integer math throughout (no
/// float drift): before `start` → `start_output`, after `end` → `end_output`.
/// Handles both decaying (start ≥ end, the UniswapX norm) and the rare
/// ascending shape symmetrically.
pub fn effective_output_at(decay: &DutchDecay, t: DateTime<Utc>) -> U256 {
    if t <= decay.start_time {
        return decay.start_output;
    }
    if t >= decay.end_time {
        return decay.end_output;
    }
    let total = (decay.end_time - decay.start_time).num_seconds();
    if total <= 0 {
        return decay.end_output;
    }
    let elapsed = (t - decay.start_time).num_seconds().max(0) as u64;
    let frac_num = U256::from(elapsed);
    let frac_den = U256::from(total as u64);
    let (start, end) = (decay.start_output, decay.end_output);
    if start >= end {
        start - (start - end) * frac_num / frac_den
    } else {
        start + (end - start) * frac_num / frac_den
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::address;
    use chrono::Duration;
    use solver_pricing::StaticGasModel;
    use solver_types::{
        FillPolicy, IntentId, SettlementType, Source,
    };

    const BASE_USDC: Address = address!("833589fCD6eDb6E08f4c7C32D4f71b54bdA02913");
    const ARB_USDC: Address = address!("af88d065e77c8cC2239327C5EDb3A432268e5831");
    const BASE_WETH: Address = address!("4200000000000000000000000000000000000006");

    fn router() -> RouterV0<StaticPriceFeed, StaticGasModel> {
        RouterV0::new(StaticPriceFeed::reference(), StaticGasModel)
    }

    /// Build a single-hop intent. Amounts are in base units; decimals are
    /// looked up by the caller-provided values.
    #[allow(clippy::too_many_arguments)]
    fn intent(
        origin: ChainId,
        dest: ChainId,
        in_addr: Address,
        in_units: u128,
        in_dec: u8,
        out_addr: Address,
        out_units: u128,
        out_dec: u8,
    ) -> NormalizedIntent {
        let now = Utc::now();
        let in_tok = TokenRef::new(origin, in_addr);
        let out_tok = TokenRef::new(dest, out_addr);
        NormalizedIntent {
            id: IntentId("test:r".into()),
            source: Source::UniswapX,
            origin_chain: origin,
            dest_chain: dest,
            input: AssetAmount::new(in_tok, U256::from(in_units), in_dec),
            output_min: AssetAmount::new(out_tok, U256::from(out_units), out_dec),
            output_decay: None,
            deadline: now + Duration::minutes(5),
            settlement: SettlementType::OffchainAttestation,
            observed_at: now,
            winning_filler: None,
            realized_output: None,
            exclusivity: None,
            tx_ref: None,
            raw: serde_json::Value::Null,
        }
    }

    /// 1000 USDC in → 998 USDC out (cross-chain, same class): on cheap L2 gas
    /// the route prints, confidence is the same-class value, and the default
    /// FillPolicy would fill. Profit is checked against the hand-computed
    /// 1000 − 998 − gas, not against itself.
    #[tokio::test]
    async fn profitable_stable_route_quotes_and_fills() {
        let i = intent(ChainId::BASE, ChainId::ARBITRUM, BASE_USDC, 1_000_000_000, 6, ARB_USDC, 998_000_000, 6);
        let out = router().quote(&i, i.observed_at).await;
        let q = out.quote().expect("should quote");
        // gas on Arbitrum is ~0.00003 ETH * $3000 ≈ $0.09, so profit ≈ $1.91.
        assert!((q.expected_profit_usd - 2.0).abs() < 0.2, "profit {}", q.expected_profit_usd);
        assert!(q.expected_profit_usd > 1.5);
        assert_eq!(q.confidence, CONF_SAME_CLASS);
        assert!(FillPolicy::default().is_fill(q));
    }

    /// Output worth more than input → negative profit → the default policy
    /// declines (Unprofitable). The Router still *quotes* (measuring the
    /// route's economics is its job); the FillPolicy owns the fill decision.
    #[tokio::test]
    async fn underwater_route_quotes_but_policy_declines() {
        let i = intent(ChainId::BASE, ChainId::ARBITRUM, BASE_USDC, 1_000_000_000, 6, ARB_USDC, 1_001_000_000, 6);
        let q = router().quote(&i, i.observed_at).await.quote().cloned().expect("quotes");
        assert!(q.expected_profit_usd < 0.0, "profit {}", q.expected_profit_usd);
        assert!(!FillPolicy::default().is_fill(&q));
    }

    /// Cross-class (WETH→USDC) is priced but, on static prices, must sit below
    /// the default fill threshold and therefore NEVER count as would_fill —
    /// even when the modeled profit looks large. This is the guard against the
    /// static-price phantom-margin artifact (CORE-5: no false confidence).
    #[tokio::test]
    async fn cross_class_quotes_but_never_fills_on_static_prices() {
        // 1 WETH in (~$3000) vs only 1500 USDC out → a fat *phantom* $1500
        // modeled profit. The policy must still decline it (Suspect), because
        // confidence is below threshold.
        let i = intent(ChainId::BASE, ChainId::BASE, BASE_WETH, 1_000_000_000_000_000_000, 18, BASE_USDC, 1_500_000_000, 6);
        let q = router().quote(&i, i.observed_at).await.quote().cloned().expect("quotes");
        assert_eq!(q.confidence, CONF_CROSS_CLASS);
        assert!(q.confidence < CONF_SAME_CLASS);
        assert!(q.expected_profit_usd > 1000.0, "phantom profit is large: {}", q.expected_profit_usd);
        assert!(!FillPolicy::default().is_fill(&q), "static cross-class must not fill");
        assert!(q.confidence < FillPolicy::default().min_confidence);
    }

    /// An unrecognised output token has no offline USD price → skip rather
    /// than fabricate a valuation.
    #[tokio::test]
    async fn unknown_token_is_skipped_not_priced() {
        let i = intent(ChainId::BASE, ChainId::BASE, BASE_USDC, 1_000_000_000, 6, Address::repeat_byte(0x42), 1_000_000_000_000_000_000, 18);
        match router().quote(&i, i.observed_at).await {
            QuoteOutcome::Skipped(SkipReason::PriceUnknown) => {}
            other => panic!("expected PriceUnknown skip, got {other:?}"),
        }
    }

    /// Evaluating after the deadline → Expired (the live liveness gate).
    #[tokio::test]
    async fn expired_intent_is_skipped() {
        let i = intent(ChainId::BASE, ChainId::ARBITRUM, BASE_USDC, 1_000_000_000, 6, ARB_USDC, 998_000_000, 6);
        let after = i.deadline + Duration::seconds(1);
        match router().quote(&i, after).await {
            QuoteOutcome::Skipped(SkipReason::Expired) => {}
            other => panic!("expected Expired, got {other:?}"),
        }
    }

    /// Same route, expensive vs cheap gas chain: Ethereum's ~$27 fill gas eats
    /// far more than Base's ~$0.15, so the modeled profit must be strictly
    /// lower on Ethereum. Guards the gas→USD wiring behaviorally.
    #[tokio::test]
    async fn higher_gas_chain_yields_lower_profit() {
        let on_base = intent(ChainId::ARBITRUM, ChainId::BASE, ARB_USDC, 1_000_000_000, 6, BASE_USDC, 990_000_000, 6);
        let on_eth = {
            let mut i = on_base.clone();
            i.dest_chain = ChainId::ETHEREUM;
            i.output_min = AssetAmount::new(TokenRef::new(ChainId::ETHEREUM, address!("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48")), U256::from(990_000_000u64), 6);
            i
        };
        let pb = router().quote(&on_base, on_base.observed_at).await.quote().cloned().unwrap();
        let pe = router().quote(&on_eth, on_eth.observed_at).await.quote().cloned().unwrap();
        assert!(pb.expected_profit_usd > pe.expected_profit_usd, "base {} eth {}", pb.expected_profit_usd, pe.expected_profit_usd);
    }

    /// Dutch decay: midpoint of a 60s window from 3110→3009 base units is
    /// 3110 − (101·30/60) = 3060; endpoints and out-of-band clamp exactly.
    /// Mirrors the V3 fixture in the ingest tests.
    #[test]
    fn decay_interpolation_is_linear_and_clamped() {
        let t0 = Utc::now();
        let decay = DutchDecay {
            start_time: t0,
            end_time: t0 + Duration::seconds(60),
            start_output: U256::from(3_110u64),
            end_output: U256::from(3_009u64),
        };
        assert_eq!(effective_output_at(&decay, t0 - Duration::seconds(5)), U256::from(3_110u64));
        assert_eq!(effective_output_at(&decay, t0), U256::from(3_110u64));
        assert_eq!(effective_output_at(&decay, t0 + Duration::seconds(30)), U256::from(3_060u64));
        assert_eq!(effective_output_at(&decay, t0 + Duration::seconds(60)), U256::from(3_009u64));
        assert_eq!(effective_output_at(&decay, t0 + Duration::seconds(90)), U256::from(3_009u64));
    }
}
