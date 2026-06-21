//! Metrics: run RouterV0 over the loaded intents, apply the single
//! [`FillPolicy`], and aggregate — including a first cut at would_fill
//! **calibration** against realized outcomes.
//!
//! Honesty notes baked in here:
//!   - `would_fill` is a **modeled belief** (router + static prices), not a
//!     realized profit. We keep the two clearly separate.
//!   - `profitable_quote_rate` is NOT a win rate (CLAUDE.md rule #10).
//!   - calibration compares our modeled fills to the winning filler's
//!     **price-only realized gross margin** — the right "was there margin
//!     here at all?" reference (B1 deepens this).

use std::collections::BTreeMap;

use solver_pricing::StaticGasModel;
use solver_router::{PriceFeed, QuoteOutcome, RouterV0, StaticPriceFeed};
use solver_types::{FillPolicy, NormalizedIntent};

use crate::route_summary::RouteSummary;

/// Aggregate counts over a backtest run. All USD figures are MODELED on static
/// prices — the report labels them so.
#[derive(Debug, Clone, Default)]
pub struct Metrics {
    pub total: usize,
    pub quoted: usize,
    /// SkipReason string -> count (e.g. "price_unknown").
    pub skipped: BTreeMap<String, usize>,
    /// FillReason string -> count (e.g. "fill", "below_margin").
    pub fill_reasons: BTreeMap<String, usize>,
    pub would_fill: usize,
    /// Quotes with `expected_profit_usd > 0` (before the margin band).
    pub profitable_quotes: usize,
    /// Sum of modeled `expected_profit_usd` over would_fill intents.
    pub modeled_profit_usd_sum: f64,
    // --- calibration vs realized ---
    /// Intents (of any decision) that carry realized data we can price.
    pub with_realized: usize,
    /// would_fill intents that also carry priceable realized data.
    pub would_fill_with_realized: usize,
    /// ...of those, how many had a positive realized **gross** margin (market
    /// confirms there was *some* margin to capture, before our gas).
    pub would_fill_confirmed: usize,
    /// ...of those, how many had a realized **net** margin (after our gas) at
    /// or above `FillPolicy.min_margin_usd` — i.e. the market actually
    /// supported a fill *by our own threshold*, not merely "any margin".
    /// This is the honest upper bound against the bar we'd really bid at.
    pub would_fill_market_supported: usize,
    /// Sum of realized **gross** margin (price-only, no gas) over the
    /// would_fill-with-realized set. Pairs with `modeled_profit_usd_sum`.
    pub realized_margin_usd_sum: f64,
    /// Sum of realized **net** margin (gross − our gas) over the same set.
    /// This is the like-for-like counterpart to `modeled_profit_usd_sum`,
    /// which is also net of our gas — net↔net, so the gap is honest.
    pub realized_net_usd_sum: f64,
    /// Per-fill `drift = modeled_net − realized_net` over the
    /// would_fill-with-realized set. Positive = we over-modeled the profit
    /// (the over-optimism we are trying to make inspectable). Kept as raw
    /// samples so the report can show the *shape* (percentiles), not just a
    /// mean that hides a fat tail.
    pub drift_samples_usd: Vec<f64>,
}

impl Metrics {
    /// Of the quotes produced, the share with positive modeled profit.
    /// **Not** a win rate. `None` when nothing was quoted.
    pub fn profitable_quote_rate(&self) -> Option<f64> {
        (self.quoted > 0).then(|| self.profitable_quotes as f64 / self.quoted as f64)
    }

    /// Of the would_fill intents we can check against realized data, the share
    /// the market confirms had **any** gross margin (realized gross > 0).
    /// `None` when there's nothing to check — the honest answer until real
    /// data lands (don't fabricate a number). This is a generous bar: "was
    /// there margin here at all?", not "could *we* have profited?".
    pub fn would_fill_precision(&self) -> Option<f64> {
        (self.would_fill_with_realized > 0)
            .then(|| self.would_fill_confirmed as f64 / self.would_fill_with_realized as f64)
    }

    /// Stricter precision: of the checkable would_fill intents, the share whose
    /// realized **net** margin cleared our own `min_margin_usd` bar — "could we
    /// have been in the green by our threshold?". The honest upper bound vs the
    /// bar we'd actually bid at. `None` when nothing is checkable.
    pub fn would_fill_market_supported_precision(&self) -> Option<f64> {
        (self.would_fill_with_realized > 0).then(|| {
            self.would_fill_market_supported as f64 / self.would_fill_with_realized as f64
        })
    }

    /// Mean per-fill drift (modeled_net − realized_net) over the checkable set.
    /// `None` when there are no drift samples. A mean alone hides the tail —
    /// pair with [`Self::drift_percentiles`].
    pub fn mean_drift_usd(&self) -> Option<f64> {
        (!self.drift_samples_usd.is_empty())
            .then(|| self.drift_samples_usd.iter().sum::<f64>() / self.drift_samples_usd.len() as f64)
    }

    /// Distribution shape of the drift samples as
    /// `[p10, p25, p50, p75, p90, min, max]`. `None` when there are no
    /// samples — we return `Option` rather than a fabricated all-zero array
    /// (no inventing a distribution that isn't there, CLAUDE.md rule #2).
    /// Nearest-rank on the `n-1` scale; for tiny samples the report labels the
    /// percentiles as indicative.
    pub fn drift_percentiles(&self) -> Option<[f64; 7]> {
        if self.drift_samples_usd.is_empty() {
            return None;
        }
        let mut s = self.drift_samples_usd.clone();
        s.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let pick = |p: f64| -> f64 {
            let idx = ((p * (s.len() - 1) as f64).round() as usize).min(s.len() - 1);
            s[idx]
        };
        Some([pick(0.10), pick(0.25), pick(0.50), pick(0.75), pick(0.90), s[0], s[s.len() - 1]])
    }
}

/// One row for the report's "verifiable fills" section. Carries the
/// click-through link so a reader can check the numbers on a block explorer.
#[derive(Debug, Clone)]
pub struct FillRow {
    pub id: String,
    pub route_key: String,
    /// Modeled profit, **net of our gas** (the router's `expected_profit_usd`).
    pub modeled_profit_usd: f64,
    /// Our modeled gas cost for this fill (USD). Carried so the report can put
    /// `realized_margin` (price-only, gross) on the same net footing as the
    /// modeled number: `realized_net = realized_margin − gas_usd`.
    pub gas_usd: f64,
    pub confidence: f64,
    /// Price-only realized **gross** margin, when known — lets the reader see
    /// the model vs reality gap per row.
    pub realized_margin_usd: Option<f64>,
    pub verify_url: Option<String>,
}

impl FillRow {
    /// Realized margin put on the same footing as `modeled_profit_usd`: gross
    /// price-spread minus *our* modeled gas. `None` when no realized data.
    pub fn realized_net_usd(&self) -> Option<f64> {
        self.realized_margin_usd.map(|m| m - self.gas_usd)
    }

    /// `modeled_net − realized_net` for this fill — positive = over-modeled.
    /// `None` when there's no realized data to compare against.
    pub fn drift_usd(&self) -> Option<f64> {
        self.realized_net_usd().map(|rn| self.modeled_profit_usd - rn)
    }
}

#[derive(Debug, Clone)]
pub struct BacktestOutput {
    pub metrics: Metrics,
    /// would_fill intents, highest modeled profit first.
    pub fills: Vec<FillRow>,
    /// Per-route calibration roll-ups, most-observed route first (WFC-6).
    pub routes: Vec<RouteSummary>,
}

/// Run the router + policy over `intents` and aggregate. Async because the gas
/// model is async (offline in Phase 0, so still deterministic).
pub async fn compute(
    intents: &[NormalizedIntent],
    prices: StaticPriceFeed,
    policy: FillPolicy,
) -> BacktestOutput {
    let router = RouterV0::new(prices, StaticGasModel);
    let mut m = Metrics { total: intents.len(), ..Default::default() };
    let mut fills: Vec<FillRow> = Vec::new();
    let mut routes: BTreeMap<String, RouteSummary> = BTreeMap::new();

    for intent in intents {
        let realized_margin = intent.realized_gross_margin_usd(|t| prices.usd_price(t));
        if realized_margin.is_some() {
            m.with_realized += 1;
        }

        // Count every observation against its route up front (any decision),
        // so `n` reads as "intents on this route", not "fills". Re-borrowed via
        // get_mut below in the fill branch to avoid holding it across the await.
        let rk = intent.route_key();
        routes
            .entry(rk.clone())
            .or_insert_with(|| RouteSummary::empty(rk.clone()))
            .n += 1;

        // Evaluate as of when the order is open to a *generic* filler — i.e.
        // when its Dutch auction starts (exclusivity, if any, ends there). For
        // a no-decay intent there's no auction, so observation time stands in.
        // This measures profitability of the flow, not winnability: a generic
        // filler is genuinely locked out only during exclusivity, which has
        // ended by decay-start (CLAUDE.md: profitable_quote_rate ≠ win rate).
        let eval_at = intent
            .output_decay
            .as_ref()
            .map(|d| d.start_time)
            .unwrap_or(intent.observed_at);

        // Single shared decision (router.quote + policy.evaluate) — identical
        // to what the live shadow loop runs, so would_fill means one thing.
        let decision = router.decide(intent, eval_at, &policy).await;
        match &decision.outcome {
            QuoteOutcome::Skipped(r) => {
                *m.skipped.entry(r.as_str().to_string()).or_default() += 1;
            }
            QuoteOutcome::Quoted(q) => {
                m.quoted += 1;
                if q.expected_profit_usd > 0.0 {
                    m.profitable_quotes += 1;
                }
                let (fill, reason) = decision.verdict.expect("a quote always has a verdict");
                *m.fill_reasons.entry(reason.as_str().to_string()).or_default() += 1;

                if fill {
                    let route = routes.get_mut(&rk).expect("route inserted above");
                    m.would_fill += 1;
                    m.modeled_profit_usd_sum += q.expected_profit_usd;
                    route.would_fill += 1;
                    route.modeled_sum += q.expected_profit_usd;
                    fills.push(FillRow {
                        id: intent.id.0.clone(),
                        route_key: rk.clone(),
                        modeled_profit_usd: q.expected_profit_usd,
                        gas_usd: q.gas_usd,
                        confidence: q.confidence,
                        realized_margin_usd: realized_margin,
                        verify_url: intent.verify_url(),
                    });
                    if let Some(rm) = realized_margin {
                        // Put realized on the same net footing as the modeled
                        // number before computing drift (net↔net), so the gap
                        // isn't inflated by comparing net-of-gas to gross.
                        let realized_net = rm - q.gas_usd;
                        let drift = q.expected_profit_usd - realized_net;
                        let confirmed = rm > 0.0;
                        let supported = realized_net >= policy.min_margin_usd;
                        m.would_fill_with_realized += 1;
                        m.realized_margin_usd_sum += rm;
                        m.realized_net_usd_sum += realized_net;
                        m.drift_samples_usd.push(drift);
                        route.would_fill_with_realized += 1;
                        route.realized_net_sum += realized_net;
                        route.drift_sum += drift;
                        if confirmed {
                            m.would_fill_confirmed += 1;
                            route.confirmed += 1;
                        }
                        if supported {
                            m.would_fill_market_supported += 1;
                            route.market_supported += 1;
                        }
                    }
                }
            }
        }
    }

    fills.sort_by(|a, b| {
        b.modeled_profit_usd
            .partial_cmp(&a.modeled_profit_usd)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    // Most-observed route first; route_key tiebreak keeps the order stable.
    let mut routes: Vec<RouteSummary> = routes.into_values().collect();
    routes.sort_by(|a, b| b.n.cmp(&a.n).then_with(|| a.route_key.cmp(&b.route_key)));
    BacktestOutput { metrics: m, fills, routes }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{address, Address, U256};
    use chrono::{Duration, Utc};
    use solver_types::{AssetAmount, ChainId, IntentId, SettlementType, Source, TokenRef, TxRef};

    const BASE_USDC: Address = address!("833589fCD6eDb6E08f4c7C32D4f71b54bdA02913");
    const ARB_USDC: Address = address!("af88d065e77c8cC2239327C5EDb3A432268e5831");

    fn stable_intent(id: &str, in_units: u64, out_units: u64, realized_units: Option<u64>, tx: bool) -> NormalizedIntent {
        let now = Utc::now();
        let in_tok = TokenRef::new(ChainId::BASE, BASE_USDC);
        let out_tok = TokenRef::new(ChainId::ARBITRUM, ARB_USDC);
        NormalizedIntent {
            id: IntentId(id.into()),
            source: Source::UniswapX,
            origin_chain: ChainId::BASE,
            dest_chain: ChainId::ARBITRUM,
            input: AssetAmount::new(in_tok, U256::from(in_units), 6),
            output_min: AssetAmount::new(out_tok, U256::from(out_units), 6),
            output_decay: None,
            deadline: now + Duration::minutes(5),
            settlement: SettlementType::OffchainAttestation,
            observed_at: now,
            winning_filler: None,
            realized_output: realized_units.map(|u| AssetAmount::new(out_tok, U256::from(u), 6)),
            exclusivity: None,
            tx_ref: tx.then(|| TxRef {
                chain: ChainId::ARBITRUM,
                tx_hash: "0xabc0000000000000000000000000000000000000000000000000000000000001".into(),
            }),
            raw: serde_json::Value::Null,
        }
    }

    /// A hand-built set: one fat-margin fill that realized positively, one
    /// fat-margin fill that realized negatively, one underwater (no fill).
    /// Assert the decision counts and that precision = 1/2 — computed by hand
    /// from the inputs, not read back from the aggregator.
    #[tokio::test]
    async fn aggregates_decisions_and_calibrates() {
        let prices = StaticPriceFeed::reference();
        let policy = FillPolicy::default();

        let intents = vec![
            // 1000 → 990 floor, realized 995 (filler captured +$5) → would_fill, confirmed.
            stable_intent("uniswapx:a", 1_000_000_000, 990_000_000, Some(995_000_000), true),
            // 1000 → 990 floor, realized 1002 (filler underwater −$2) → would_fill, NOT confirmed.
            stable_intent("uniswapx:b", 1_000_000_000, 990_000_000, Some(1_002_000_000), true),
            // 1000 → 1005 floor (we must deliver more than we get) → underwater, no fill.
            stable_intent("uniswapx:c", 1_000_000_000, 1_005_000_000, Some(1_000_000_000), true),
        ];

        let out = compute(&intents, prices, policy).await;
        let m = &out.metrics;
        assert_eq!(m.total, 3);
        assert_eq!(m.quoted, 3, "all stable-priced, none skipped");
        assert_eq!(m.would_fill, 2);
        assert_eq!(m.profitable_quotes, 2);
        assert_eq!(m.fill_reasons.get("fill").copied(), Some(2));
        assert_eq!(m.fill_reasons.get("unprofitable").copied(), Some(1));
        // Calibration: 2 would_fill with realized, 1 confirmed → precision 0.5.
        assert_eq!(m.would_fill_with_realized, 2);
        assert_eq!(m.would_fill_confirmed, 1);
        assert!((m.would_fill_precision().unwrap() - 0.5).abs() < 1e-9);

        // Gross/net reconciliation (WFC-5). Gas on Arbitrum = 0.00003 ETH ×
        // $3000 = $0.09. Hand-computed per intent (net↔net drift):
        //   a: modeled 9.91, realized_gross +5.0, realized_net 4.91, drift 5.0
        //   b: modeled 9.91, realized_gross −2.0, realized_net −2.09, drift 12.0
        assert!((m.realized_margin_usd_sum - 3.0).abs() < 1e-6, "gross sum {}", m.realized_margin_usd_sum);
        assert!((m.realized_net_usd_sum - 2.82).abs() < 1e-6, "net sum {}", m.realized_net_usd_sum);
        assert!((m.mean_drift_usd().unwrap() - 8.5).abs() < 1e-6, "mean drift {:?}", m.mean_drift_usd());
        let p = m.drift_percentiles().expect("samples present");
        assert!((p[5] - 5.0).abs() < 1e-6, "min drift {}", p[5]); // min
        assert!((p[6] - 12.0).abs() < 1e-6, "max drift {}", p[6]); // max
        assert!((p[2] - 12.0).abs() < 1e-6, "p50 drift {}", p[2]); // p50 (nearest-rank, n=2)

        // Stricter precision: only intent a's realized_net (4.91) clears our
        // $0.50 bar; b's (−2.09) does not → market_supported = 1, precision 0.5.
        assert_eq!(m.would_fill_market_supported, 1);
        assert!((m.would_fill_market_supported_precision().unwrap() - 0.5).abs() < 1e-9);

        // Fills sorted by modeled profit desc, both carry a verify URL and the
        // gas needed to put realized on a net footing.
        assert_eq!(out.fills.len(), 2);
        assert!(out.fills[0].modeled_profit_usd >= out.fills[1].modeled_profit_usd);
        assert!(out.fills.iter().all(|r| r.verify_url.is_some()));
        // Per-row drift helper agrees with the aggregate (a: 5.0, b: 12.0).
        let mut drifts: Vec<f64> = out.fills.iter().filter_map(|r| r.drift_usd()).collect();
        drifts.sort_by(|a, b| a.partial_cmp(b).unwrap());
        assert!((drifts[0] - 5.0).abs() < 1e-6 && (drifts[1] - 12.0).abs() < 1e-6, "row drifts {drifts:?}");

        // Per-route roll-up (WFC-6): all three share one route. n counts every
        // observation (3); would_fill 2; checkable 2; confirmed/supported 1
        // each → precision 0.5; mean drift 8.5; sums match the aggregate.
        assert_eq!(out.routes.len(), 1);
        let r = &out.routes[0];
        assert_eq!((r.n, r.would_fill, r.would_fill_with_realized), (3, 2, 2));
        assert_eq!((r.confirmed, r.market_supported), (1, 1));
        assert!((r.precision().unwrap() - 0.5).abs() < 1e-9);
        assert!((r.market_supported_precision().unwrap() - 0.5).abs() < 1e-9);
        assert!((r.mean_drift().unwrap() - 8.5).abs() < 1e-6, "route mean drift {:?}", r.mean_drift());
        assert!((r.modeled_sum - 19.82).abs() < 1e-6, "route modeled {}", r.modeled_sum);
        assert!((r.realized_net_sum - 2.82).abs() < 1e-6, "route realized_net {}", r.realized_net_sum);
    }

    /// With no realized data anywhere, precision is `None` — we refuse to
    /// report a calibration number we cannot compute.
    #[tokio::test]
    async fn precision_is_none_without_realized() {
        let intents = vec![stable_intent("uniswapx:x", 1_000_000_000, 990_000_000, None, false)];
        let out = compute(&intents, StaticPriceFeed::reference(), FillPolicy::default()).await;
        assert_eq!(out.metrics.would_fill, 1);
        assert!(out.metrics.would_fill_precision().is_none());
    }
}
