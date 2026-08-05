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

mod live_prices;
mod prices;

use live_prices::chain_price_supported;
pub use live_prices::{LivePriceFeed, PriceResolver, PriceSource, USER_AGENT};
pub use prices::{PriceFeed, StaticPriceFeed};

use alloy_primitives::{Address, U256};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use solver_pricing::GasModel;
use solver_types::{
    asset_class, checked_decimals, AssetAmount, AssetClass, ChainId, DutchDecay, FillPolicy,
    FillReason, NormalizedIntent, Quote, Route, RouteHop, RouteSource, SignedAmount, TokenRef,
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

/// Confidence stamped on a quote whose modeled margin is implausibly large for
/// the protocol (the sanity backstop in [`RouterV0::quote`]). Below the default
/// fill threshold (0.50) so it resolves to `suspect` and never counts as
/// would_fill — a pricing artifact made **visible** (a low-confidence suspect
/// row), not silently dropped (CLAUDE.md rule #10).
pub const CONF_IMPLAUSIBLE: f64 = 0.20;

/// Upper bound on a *plausible* Across margin as a fraction of input notional.
/// A relayer's take is the deposit's fixed fee — 0.1–0.3% in practice; 5% is
/// orders of magnitude above any of them, so a modeled profit exceeding it is a
/// pricing bug (a mis-scaled leg, a stale cross-class price), not an edge. The
/// backstop demotes such a quote to [`CONF_IMPLAUSIBLE`]. This is a domain
/// constraint, not a tuned threshold-dodge: it only ever fires on garbage.
const MAX_PLAUSIBLE_MARGIN_FRAC: f64 = 0.05;

/// Why the Router declined to quote an intent. Serialized into the
/// decisions JSONL `reason` field downstream — **do not rename** the strings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkipReason {
    /// A token on a **covered** chain (one with a verified price platform) has
    /// no known USD price — we looked and missed; we refuse to invent one
    /// (CLAUDE.md rule #2). Distinct from [`Self::UnsupportedChain`].
    PriceUnknown,
    /// The intent's deadline is at/before the evaluation time — unfillable.
    Expired,
    /// Under another relayer's exclusivity window at the evaluation time.
    ExclusiveToOther,
    /// Input or required output is zero — nothing to quote.
    DustAmount,
    /// A side's token is on a chain **outside our price coverage** (no verified
    /// CoinGecko platform — non-EVM, or an EVM L2 we have not mapped). The price
    /// is unobtainable structurally, not merely missing for this token — the
    /// dashboard shows this as "out of reach" vs `price_unknown`'s "reachable
    /// but unpriced" (B6/COV-2). Additive — appended, never a rename.
    UnsupportedChain,
    /// A side's token has a USD price but **unknown decimals** — we recognize
    /// neither its scale nor a safe default, so converting its raw amount to
    /// whole tokens would fabricate it (a wrong scale underflows the leg to ~0
    /// and inflates profit to the full notional). We skip rather than guess
    /// (CLAUDE.md rule #2). Additive — appended, never a rename.
    UnknownDecimals,
}

impl SkipReason {
    pub fn as_str(self) -> &'static str {
        match self {
            SkipReason::PriceUnknown => "price_unknown",
            SkipReason::Expired => "expired",
            SkipReason::ExclusiveToOther => "exclusive_to_other",
            SkipReason::DustAmount => "dust_amount",
            SkipReason::UnsupportedChain => "unsupported_chain",
            SkipReason::UnknownDecimals => "unknown_decimals",
        }
    }

    /// Classify a missing USD price by whether `chain` is within our by-address
    /// price coverage: a covered chain we simply couldn't price this token on is
    /// [`Self::PriceUnknown`]; an out-of-coverage chain is
    /// [`Self::UnsupportedChain`]. The single place this split is decided.
    fn for_missing_price(chain: ChainId) -> SkipReason {
        if chain_price_supported(chain) {
            SkipReason::PriceUnknown
        } else {
            SkipReason::UnsupportedChain
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
        //    A missing price is classified by the *failing* token's chain:
        //    out-of-coverage → unsupported_chain, covered → price_unknown.
        let in_tok = intent.input.token;
        let out_tok = intent.output_min.token;
        let (price_in, price_out) =
            match (self.prices.usd_price(in_tok), self.prices.usd_price(out_tok)) {
                (Some(a), Some(b)) => (a, b),
                (None, _) => return QuoteOutcome::Skipped(SkipReason::for_missing_price(in_tok.chain)),
                (_, None) => return QuoteOutcome::Skipped(SkipReason::for_missing_price(out_tok.chain)),
            };
        let native = TokenRef::new(intent.dest_chain, Address::ZERO);
        let native_price = match self.prices.usd_price(native) {
            Some(p) => p,
            None => return QuoteOutcome::Skipped(SkipReason::for_missing_price(intent.dest_chain)),
        };

        // Decimals must be KNOWN to value a leg honestly. The stored
        // `AssetAmount.decimals` is an ingest convenience that falls back to the
        // ERC-20 default of 18; trusting it silently fabricates the amount when
        // wrong — a 6-decimal token (e.g. bridged USDC.e at 0x2791…4174, absent
        // from the stable table) read as 18 underflows to ~0, zeroing its USD
        // leg and inflating profit to the full input notional (the bug behind
        // absurd would_fill rows). Re-derive from recognized tokens and skip
        // rather than guess (CLAUDE.md rule #2).
        let in_dec = match checked_decimals(in_tok) {
            Some(d) => d,
            None => return QuoteOutcome::Skipped(SkipReason::UnknownDecimals),
        };
        let out_dec = match checked_decimals(out_tok) {
            Some(d) => d,
            None => return QuoteOutcome::Skipped(SkipReason::UnknownDecimals),
        };
        let revenue_usd = AssetAmount::new(in_tok, intent.input.amount, in_dec).to_f64() * price_in;
        let sourcing_usd =
            AssetAmount::new(out_tok, intent.output_min.amount, out_dec).to_f64() * price_out;

        let gas_est = self.gas.fill_gas(intent.dest_chain).await;
        let gas_native = gas_est.native_wei;
        let gas_usd = AssetAmount::new(native, gas_native, 18).to_f64() * native_price;

        let expected_profit_usd = revenue_usd - sourcing_usd - gas_usd;

        // 3. Confidence: same value-class sources at par; cross-class carries
        //    unmodeled slippage. Plus a domain sanity backstop — an Across
        //    relayer's margin is the deposit's fixed fee, a fraction of a
        //    percent, so a modeled profit that is a large slice of the input
        //    notional is a pricing artifact, not an edge. Demote it to
        //    `suspect` (visible via low confidence, never silently dropped) so
        //    a future mis-pricing can never masquerade as would_fill.
        let same_class = value_class(in_tok) == value_class(out_tok)
            && value_class(in_tok) != AssetClass::Unknown;
        let implausible_margin =
            revenue_usd > 0.0 && expected_profit_usd > MAX_PLAUSIBLE_MARGIN_FRAC * revenue_usd;
        // The backstop only needs to demote routes that would otherwise CLEAR
        // the fill gate — i.e. same-class ones (cross-class already sits at
        // CONF_CROSS_CLASS, below threshold, so it can never be a false fill).
        // A same-class route with an impossible margin is the artifact we must
        // catch (e.g. WETH→WETH printing a six-figure profit from a flaky
        // output leg); a normal same-class route keeps its high confidence.
        let confidence = if same_class {
            if implausible_margin {
                CONF_IMPLAUSIBLE
            } else {
                CONF_SAME_CLASS
            }
        } else {
            CONF_CROSS_CLASS
        };

        // 4. Assemble the Quote. Output-token unit fields are derived from the
        //    USD figures via `price_out`; `expected_profit_usd` stays the
        //    authoritative number for aggregation (types invariant 5 / P0-B).
        //    `out_dec` is the checked-decimals value derived above.
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
            gas_source: gas_est.origin,
            confidence,
            valid_until: intent.deadline,
        }))
    }

    /// Quote `intent` as of `eval_at` **and** apply `policy` — the single place
    /// `router.quote` and `policy.evaluate` are combined. Both the backtest
    /// aggregator and the live shadow loop call this, so `would_fill` and the
    /// `reason` string mean exactly one thing everywhere (the FP-1 invariant;
    /// CLAUDE.md §would_fill — "live і backtest зобов'язані давати однакову
    /// відповідь"). Do not re-derive the decision anywhere else.
    pub async fn decide(
        &self,
        intent: &NormalizedIntent,
        eval_at: DateTime<Utc>,
        policy: &FillPolicy,
    ) -> Decision {
        let outcome = self.quote(intent, eval_at).await;
        // A skip has no policy verdict (its reason IS the SkipReason); a quote
        // always yields one.
        let verdict = outcome.quote().map(|q| policy.evaluate(q));
        Decision { outcome, verdict }
    }
}

/// One intent's full decision: the router [`QuoteOutcome`] plus, when quoted,
/// the [`FillPolicy`] verdict. Produced only by [`RouterV0::decide`] so backtest
/// and live share identical logic.
#[derive(Debug, Clone)]
pub struct Decision {
    pub outcome: QuoteOutcome,
    /// `(would_fill, reason)` from the policy — `Some` iff `outcome` is
    /// `Quoted`. A skip carries its reason in the `SkipReason` instead.
    pub verdict: Option<(bool, FillReason)>,
}

impl Decision {
    /// The priced quote, when the intent was quoted (else `None` — a skip).
    pub fn quote(&self) -> Option<&Quote> {
        self.outcome.quote()
    }

    /// Whether we would bid. `false` for any skip.
    pub fn would_fill(&self) -> bool {
        matches!(self.verdict, Some((true, _)))
    }

    /// The single stable reason string for the decisions log: the `SkipReason`
    /// when skipped, otherwise the `FillReason`. Both come from `as_str()` — no
    /// new strings invented (CLAUDE.md discipline).
    pub fn reason(&self) -> &'static str {
        match (&self.outcome, &self.verdict) {
            (QuoteOutcome::Skipped(r), _) => r.as_str(),
            (QuoteOutcome::Quoted(_), Some((_, fr))) => fr.as_str(),
            // A quote always carries a verdict; stay total without panicking.
            (QuoteOutcome::Quoted(_), None) => FillReason::Suspect.as_str(),
        }
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
        FillPolicy, GasOrigin, IntentId, SettlementType, Source,
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

    /// GAS-5: a Polygon-destination fill prices gas in the chain's NATIVE token
    /// (POL/MATIC, $0.50), not ETH ($3000). The router asks the feed for the
    /// dest chain's native token, so a wrong-token bug would value Polygon gas
    /// 6000× too high. Expected value derived independently from the static gas
    /// units × MATIC price, then asserted far from the ETH-priced figure. Also
    /// checks the GAS-1 source tag rides through to the Quote.
    #[tokio::test]
    async fn polygon_gas_priced_in_pol_not_eth() {
        // USDC (stable, $1) in on Base → USDC out on Polygon, so both sides
        // price and the quote turns on the dest-chain native gas valuation.
        let i = intent(ChainId::BASE, ChainId::POLYGON, BASE_USDC, 1_000_000_000, 6, ARB_USDC, 998_000_000, 6);
        let q = router().quote(&i, i.observed_at).await.quote().cloned().expect("should quote");

        let native = TokenRef::new(ChainId::POLYGON, Address::ZERO);
        let gas_native = StaticGasModel.fill_gas(ChainId::POLYGON).await.native_wei;
        let gas_matic = AssetAmount::new(native, gas_native, 18).to_f64();
        let expected_pol = gas_matic * 0.50; // priced in POL
        let wrong_eth = gas_matic * 3000.0; // the bug we guard against

        assert!((q.gas_usd - expected_pol).abs() < 1e-9, "gas {} ≠ POL-priced {expected_pol}", q.gas_usd);
        assert!(
            (q.gas_usd - wrong_eth).abs() > 1.0,
            "gas {} is ETH-priced — Polygon native must be POL",
            q.gas_usd
        );
        // Static feed ⇒ static gas model ⇒ the source tag is Static.
        assert_eq!(q.gas_source, GasOrigin::Static);
    }

    /// HOV-3: the dashboard's profit tooltip reconstructs the gross spread as
    /// `net + gas` (net = expected_profit_usd, gas = gas_usd). Guard that
    /// identity against the router's own math — for a 1000→998 USDC route the
    /// gross spread is a hand-computed $2.00 (revenue $1·1000 − sourcing
    /// $1·998), so net + gas must equal it, whatever the gas number is. If the
    /// router ever stopped subtracting gas to get net, the tooltip would lie.
    #[tokio::test]
    async fn profit_gross_equals_net_plus_gas() {
        let i = intent(ChainId::BASE, ChainId::ARBITRUM, BASE_USDC, 1_000_000_000, 6, ARB_USDC, 998_000_000, 6);
        let q = router().quote(&i, i.observed_at).await.quote().cloned().expect("should quote");
        let gross = 1_000.0 - 998.0; // hand-computed, independent of the quote
        assert!(q.gas_usd > 0.0, "a real fill costs gas");
        assert!(
            (q.expected_profit_usd + q.gas_usd - gross).abs() < 1e-9,
            "net {} + gas {} must equal gross {gross}",
            q.expected_profit_usd,
            q.gas_usd
        );
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

    /// The sanity backstop: a SAME-class route whose modeled margin is a large
    /// slice of the input notional is a pricing artifact (e.g. a flaky output
    /// leg pricing WETH→WETH at a six-figure profit), never an edge — relayer
    /// fees are fractions of a percent. It must be demoted to suspect so it can
    /// never count as would_fill, even though same-class routes are normally
    /// high-confidence. Constructed as WETH→WETH whose output amount is dust,
    /// so sourcing ≈ 0 and profit ≈ full input (≈100% margin ≫ 5%). The
    /// hand-reasoned margin, not the quote, is what makes this implausible.
    #[tokio::test]
    async fn implausible_same_class_margin_is_demoted_to_suspect() {
        // 1 WETH in (~$3000); output WETH amount is 1 wei → sourcing ≈ 0 →
        // profit ≈ $3000 on $3000 revenue (≈100% margin). Same-class WETH→WETH.
        let i = intent(ChainId::BASE, ChainId::BASE, BASE_WETH, 1_000_000_000_000_000_000, 18, BASE_WETH, 1, 18);
        let q = router().quote(&i, i.observed_at).await.quote().cloned().expect("quotes");
        assert!(q.expected_profit_usd > 1000.0, "artifact profit is large: {}", q.expected_profit_usd);
        assert_eq!(q.confidence, CONF_IMPLAUSIBLE, "implausible same-class margin must be demoted");
        assert!(!FillPolicy::default().is_fill(&q), "an impossible margin must not count as would_fill");
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

    /// Honest scope of `--live-prices` (corrects the original LP-6 claim): a
    /// live USD price is necessary but **not sufficient** to value an intent —
    /// we also need the token's decimals, and for an unrecognized token we do
    /// not know them. The same 0x42... token the static feed skips as
    /// `price_unknown` is, under live prices, *priced* but still **skipped as
    /// `unknown_decimals`**: we refuse to convert its raw amount with a guessed
    /// scale, which would underflow a 6-dp token to ~0 and fabricate a
    /// full-notional profit (the bug behind absurd would_fill rows). Live prices
    /// add real-time *values* for tokens we recognize; they do not let us value
    /// amounts whose decimals we cannot vouch for. Deterministic: mock resolver,
    /// no network.
    #[tokio::test]
    async fn live_price_without_known_decimals_still_skips() {
        use crate::{LivePriceFeed, PriceResolver, PriceSource};
        let mystery = Address::repeat_byte(0x42);
        let i = intent(ChainId::BASE, ChainId::BASE, BASE_USDC, 1_000_000_000, 6, mystery, 1_000_000_000_000_000_000, 18);

        // Static feed: unknown output token → price_unknown (no price at all).
        assert!(matches!(
            router().quote(&i, i.observed_at).await,
            QuoteOutcome::Skipped(SkipReason::PriceUnknown)
        ));

        // Live feed prices the 0x42... token by contract ($2), but its decimals
        // are unknown → unknown_decimals skip, NOT a fabricated quote. The USDC
        // input and native ETH defer to the static fallback (empty body → Unknown).
        let resolver: PriceResolver = Box::new(|url: &str| {
            if url.contains("4242424242") {
                Ok(r#"{"0x4242424242424242424242424242424242424242":{"usd":2.0}}"#.to_string())
            } else {
                Ok("{}".to_string())
            }
        });
        let live = RouterV0::new(
            PriceSource::Live(LivePriceFeed::with_resolver(StaticPriceFeed::reference(), resolver)),
            StaticGasModel,
        );
        assert!(
            matches!(
                live.quote(&i, i.observed_at).await,
                QuoteOutcome::Skipped(SkipReason::UnknownDecimals)
            ),
            "a live-priced token with unknown decimals must skip as unknown_decimals, not quote"
        );
    }

    /// COV-2/COV-3: a missing USD price is split by chain coverage. On a covered
    /// chain (Base has a verified price platform) an unknown token still skips as
    /// `price_unknown` — the input is priced, the output token simply isn't. On
    /// an out-of-coverage chain (Scroll has no platform) the same unknown token
    /// skips as `unsupported_chain` — structurally out of reach, not merely
    /// missed. Checked on both the output side (dest) and the input side (origin)
    /// so both skip arms are exercised.
    #[tokio::test]
    async fn missing_price_splits_unsupported_vs_unknown() {
        let mystery = Address::repeat_byte(0x42);

        // Covered dest chain, unknown output token → price_unknown.
        let covered = intent(ChainId::BASE, ChainId::BASE, BASE_USDC, 1_000_000_000, 6, mystery, 1_000_000_000_000_000_000, 18);
        assert!(
            matches!(router().quote(&covered, covered.observed_at).await,
                QuoteOutcome::Skipped(SkipReason::PriceUnknown)),
            "unknown token on a covered chain must be price_unknown"
        );

        // Out-of-coverage dest chain (Scroll), unknown output token → unsupported_chain.
        let bad_dest = intent(ChainId::BASE, ChainId::SCROLL, BASE_USDC, 1_000_000_000, 6, mystery, 1_000_000_000_000_000_000, 18);
        assert!(
            matches!(router().quote(&bad_dest, bad_dest.observed_at).await,
                QuoteOutcome::Skipped(SkipReason::UnsupportedChain)),
            "token on an out-of-coverage dest chain must be unsupported_chain"
        );

        // Out-of-coverage origin chain (Scroll), unknown input token → unsupported_chain.
        let bad_origin = intent(ChainId::SCROLL, ChainId::BASE, mystery, 1_000_000_000_000_000_000, 18, BASE_USDC, 1_000_000_000, 6);
        assert!(
            matches!(router().quote(&bad_origin, bad_origin.observed_at).await,
                QuoteOutcome::Skipped(SkipReason::UnsupportedChain)),
            "token on an out-of-coverage origin chain must be unsupported_chain"
        );
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

    /// The shared `decide` helper must equal quote-then-evaluate (the FP-1
    /// invariant: one decision, used by both backtest and live). Checked on the
    /// quoted path (would_fill + reason + profit) AND a skip path (reason is the
    /// SkipReason string, no quote) — so the decisions log can't diverge from
    /// the backtest.
    #[tokio::test]
    async fn decide_matches_quote_then_evaluate() {
        let policy = FillPolicy::default();
        let i = intent(ChainId::BASE, ChainId::ARBITRUM, BASE_USDC, 1_000_000_000, 6, ARB_USDC, 998_000_000, 6);

        // Quoted path: helper agrees with the manual quote+evaluate.
        let q = router().quote(&i, i.observed_at).await.quote().cloned().expect("quotes");
        let (fill, reason) = policy.evaluate(&q);
        let d = router().decide(&i, i.observed_at, &policy).await;
        assert_eq!(d.would_fill(), fill);
        assert_eq!(d.reason(), reason.as_str());
        assert_eq!(d.quote().unwrap().expected_profit_usd, q.expected_profit_usd);

        // Skip path: reason is the SkipReason, would_fill false, no quote.
        let after = i.deadline + Duration::seconds(1);
        let d2 = router().decide(&i, after, &policy).await;
        assert!(!d2.would_fill());
        assert_eq!(d2.reason(), "expired");
        assert!(d2.quote().is_none());
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
