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
    // --- calibration vs realized (price-only) ---
    /// Intents (of any decision) that carry realized data we can price.
    pub with_realized: usize,
    /// would_fill intents that also carry priceable realized data.
    pub would_fill_with_realized: usize,
    /// ...of those, how many had a positive realized gross margin (market
    /// confirms there was margin to capture).
    pub would_fill_confirmed: usize,
}

impl Metrics {
    /// Of the quotes produced, the share with positive modeled profit.
    /// **Not** a win rate. `None` when nothing was quoted.
    pub fn profitable_quote_rate(&self) -> Option<f64> {
        (self.quoted > 0).then(|| self.profitable_quotes as f64 / self.quoted as f64)
    }

    /// Of the would_fill intents we can check against realized data, the share
    /// the market confirms had margin. `None` when there's nothing to check —
    /// the honest answer until real data lands (don't fabricate a number).
    pub fn would_fill_precision(&self) -> Option<f64> {
        (self.would_fill_with_realized > 0)
            .then(|| self.would_fill_confirmed as f64 / self.would_fill_with_realized as f64)
    }
}

/// One row for the report's "verifiable fills" section. Carries the
/// click-through link so a reader can check the numbers on a block explorer.
#[derive(Debug, Clone)]
pub struct FillRow {
    pub id: String,
    pub route_key: String,
    pub modeled_profit_usd: f64,
    pub confidence: f64,
    /// Price-only realized gross margin, when known — lets the reader see the
    /// model vs reality gap per row.
    pub realized_margin_usd: Option<f64>,
    pub verify_url: Option<String>,
}

#[derive(Debug, Clone)]
pub struct BacktestOutput {
    pub metrics: Metrics,
    /// would_fill intents, highest modeled profit first.
    pub fills: Vec<FillRow>,
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

    for intent in intents {
        let realized_margin = intent.realized_gross_margin_usd(|t| prices.usd_price(t));
        if realized_margin.is_some() {
            m.with_realized += 1;
        }

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

        match router.quote(intent, eval_at).await {
            QuoteOutcome::Skipped(r) => {
                *m.skipped.entry(r.as_str().to_string()).or_default() += 1;
            }
            QuoteOutcome::Quoted(q) => {
                m.quoted += 1;
                if q.expected_profit_usd > 0.0 {
                    m.profitable_quotes += 1;
                }
                let (fill, reason) = policy.evaluate(&q);
                *m.fill_reasons.entry(reason.as_str().to_string()).or_default() += 1;

                if fill {
                    m.would_fill += 1;
                    m.modeled_profit_usd_sum += q.expected_profit_usd;
                    fills.push(FillRow {
                        id: intent.id.0.clone(),
                        route_key: intent.route_key(),
                        modeled_profit_usd: q.expected_profit_usd,
                        confidence: q.confidence,
                        realized_margin_usd: realized_margin,
                        verify_url: intent.verify_url(),
                    });
                    if let Some(rm) = realized_margin {
                        m.would_fill_with_realized += 1;
                        if rm > 0.0 {
                            m.would_fill_confirmed += 1;
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
    BacktestOutput { metrics: m, fills }
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
        // Fills sorted by modeled profit desc, both carry a verify URL.
        assert_eq!(out.fills.len(), 2);
        assert!(out.fills[0].modeled_profit_usd >= out.fills[1].modeled_profit_usd);
        assert!(out.fills.iter().all(|r| r.verify_url.is_some()));
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
