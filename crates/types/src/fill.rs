//! Single source of truth for the bid/fill decision. Applied identically by
//! the live shadow loop (`cli::live`) and the backtest aggregator
//! (`backtest::metrics`) so "would we fill?" has exactly one meaning across
//! the codebase (FP-1). Was split: live used `> $0.50`, backtest `> $0.0`.

use crate::quote::Quote;

#[derive(Debug, Clone, Copy)]
pub struct FillPolicy {
    /// Minimum modeled NET margin (USD) to bid. Chosen >= the gas model's
    /// error bar (~$0.05-0.3 on L2s): a margin inside that band is noise.
    pub min_margin_usd: f64,
    /// Minimum confidence; below this a quote is "suspect" and never fills.
    pub min_confidence: f64,
}

impl Default for FillPolicy {
    fn default() -> Self {
        Self {
            min_margin_usd: 0.50,
            min_confidence: 0.50,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FillReason {
    Fill,
    Suspect,
    Unprofitable,
    BelowMargin,
}

impl FillReason {
    /// Stable strings for the decisions-*.jsonl `reason` field. DO NOT rename.
    pub fn as_str(self) -> &'static str {
        match self {
            FillReason::Fill => "fill",
            FillReason::Suspect => "suspect",
            FillReason::Unprofitable => "unprofitable",
            FillReason::BelowMargin => "below_margin",
        }
    }
}

impl FillPolicy {
    /// Decide on an already-priced quote. Order matches the legacy live mapping:
    /// confidence first, then sign, then margin band.
    pub fn evaluate(&self, q: &Quote) -> (bool, FillReason) {
        if q.confidence < self.min_confidence {
            (false, FillReason::Suspect)
        } else if q.expected_profit_usd <= 0.0 {
            (false, FillReason::Unprofitable)
        } else if q.expected_profit_usd <= self.min_margin_usd {
            (false, FillReason::BelowMargin)
        } else {
            (true, FillReason::Fill)
        }
    }

    pub fn is_fill(&self, q: &Quote) -> bool {
        self.evaluate(q).0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::amount::{AssetAmount, TokenRef};
    use crate::chain::ChainId;
    use crate::intent::IntentId;
    use crate::quote::{GasOrigin, Route, SignedAmount};
    use alloy_primitives::{Address, U256};
    use chrono::Utc;

    fn tok() -> TokenRef {
        TokenRef::new(ChainId::ARBITRUM, Address::repeat_byte(0x11))
    }

    fn q(profit_usd: f64, confidence: f64) -> Quote {
        Quote {
            intent_id: IntentId("test".into()),
            route: Route::empty(),
            expected_output: AssetAmount::new(tok(), U256::ZERO, 6),
            cost_gas: vec![],
            cost_sourcing: AssetAmount::new(tok(), U256::ZERO, 6),
            expected_profit: SignedAmount::zero(tok(), 6),
            expected_profit_usd: profit_usd,
            gas_usd: 0.0,
            gas_source: GasOrigin::Static,
            confidence,
            valid_until: Utc::now(),
        }
    }

    #[test]
    fn below_margin_band_does_not_fill() {
        let p = FillPolicy::default();
        let (fill, r) = p.evaluate(&q(0.30, 0.9));
        assert!(!fill);
        assert_eq!(r, FillReason::BelowMargin);
    }

    #[test]
    fn comfortable_margin_fills() {
        let p = FillPolicy::default();
        let (fill, r) = p.evaluate(&q(0.60, 0.9));
        assert!(fill);
        assert_eq!(r, FillReason::Fill);
    }

    #[test]
    fn low_confidence_is_suspect_even_with_huge_profit() {
        let p = FillPolicy::default();
        let (fill, r) = p.evaluate(&q(100.0, 0.3));
        assert!(!fill);
        assert_eq!(r, FillReason::Suspect);
    }

    #[test]
    fn zero_profit_is_unprofitable() {
        let p = FillPolicy::default();
        let (fill, r) = p.evaluate(&q(0.0, 0.9));
        assert!(!fill);
        assert_eq!(r, FillReason::Unprofitable);
    }

    #[test]
    fn margin_at_threshold_is_below_not_fill() {
        // Strict-`>` semantics: equal to the threshold is below the band.
        let p = FillPolicy::default();
        let (fill, r) = p.evaluate(&q(0.50, 0.9));
        assert!(!fill);
        assert_eq!(r, FillReason::BelowMargin);
    }

    #[test]
    fn confidence_at_threshold_is_not_suspect() {
        // `>= min_confidence` admits the boundary; below the boundary is suspect.
        let p = FillPolicy::default();
        let (fill, r) = p.evaluate(&q(1.0, 0.5));
        assert!(fill);
        assert_eq!(r, FillReason::Fill);
    }
}
