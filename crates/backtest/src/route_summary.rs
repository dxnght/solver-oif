//! Per-route would_fill calibration (WFC-6).
//!
//! The aggregate calibration in [`crate::metrics`] answers "across everything,
//! how over-optimistic were we?". This answers the sharper spec question
//! (§3): *which* routes, if any, are stably in the green — shown per route,
//! not hidden in a blended mean. On honest cross-chain data the most likely
//! answer is "none", and that is a **valid, reportable** result, not a gap.
//!
//! Same footing as the aggregate: every USD figure here is net of our modeled
//! gas, and drift is `modeled_net − realized_net` (positive = we over-modeled).
//! Accumulated in the single [`crate::metrics::compute`] pass — no second walk.

/// Calibration roll-up for one `route_key`. Counts are over the intents seen on
/// that route during the run; the USD sums and drift are over the would_fill
/// subset (and, for realized, the subset that also carried priceable realized
/// data — the only rows where a model-vs-reality gap is computable).
#[derive(Debug, Clone)]
pub struct RouteSummary {
    pub route_key: String,
    /// Total intents observed on this route (any decision, incl. skipped).
    pub n: usize,
    /// ...of which we would_fill (model belief).
    pub would_fill: usize,
    /// ...of those, the ones that also carry priceable realized data — the
    /// denominator for [`Self::precision`] and the drift mean.
    pub would_fill_with_realized: usize,
    /// would_fill-with-realized rows whose realized **gross** margin was > 0.
    pub confirmed: usize,
    /// would_fill-with-realized rows whose realized **net** margin cleared our
    /// own `min_margin_usd` bar (the stricter, honest "we'd have profited" cut).
    pub market_supported: usize,
    /// Sum of modeled **net** profit over the would_fill rows on this route.
    pub modeled_sum: f64,
    /// Sum of realized **net** margin over the would_fill-with-realized rows.
    pub realized_net_sum: f64,
    /// Sum of per-row drift over the same rows; `mean_drift()` divides it out.
    pub drift_sum: f64,
}

impl RouteSummary {
    pub fn empty(route_key: String) -> Self {
        Self {
            route_key,
            n: 0,
            would_fill: 0,
            would_fill_with_realized: 0,
            confirmed: 0,
            market_supported: 0,
            modeled_sum: 0.0,
            realized_net_sum: 0.0,
            drift_sum: 0.0,
        }
    }

    /// Share of the route's checkable would_fill rows the market confirms had
    /// **any** gross margin (`confirmed / would_fill_with_realized`). `None`
    /// when nothing on this route is checkable — don't fabricate a rate.
    pub fn precision(&self) -> Option<f64> {
        (self.would_fill_with_realized > 0)
            .then(|| self.confirmed as f64 / self.would_fill_with_realized as f64)
    }

    /// Stricter per-route precision: share whose realized net cleared *our* bar
    /// (`market_supported / would_fill_with_realized`). `None` when unchecked.
    pub fn market_supported_precision(&self) -> Option<f64> {
        (self.would_fill_with_realized > 0)
            .then(|| self.market_supported as f64 / self.would_fill_with_realized as f64)
    }

    /// Mean per-row drift (modeled_net − realized_net) on this route. `None`
    /// when there are no checkable rows.
    pub fn mean_drift(&self) -> Option<f64> {
        (self.would_fill_with_realized > 0)
            .then(|| self.drift_sum / self.would_fill_with_realized as f64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Hand-built roll-up: 4 would_fill, 3 checkable, 2 with any gross margin,
    /// 1 clearing our bar; drift_sum 9.0 over 3 rows. Rates/mean computed by
    /// hand from those, not read back from the struct.
    #[test]
    fn precision_and_mean_drift_are_ratios_over_checkable_rows() {
        let r = RouteSummary {
            route_key: "base->arbitrum/usdc->usdc".into(),
            n: 6,
            would_fill: 4,
            would_fill_with_realized: 3,
            confirmed: 2,
            market_supported: 1,
            modeled_sum: 40.0,
            realized_net_sum: 12.0,
            drift_sum: 9.0,
        };
        assert!((r.precision().unwrap() - 2.0 / 3.0).abs() < 1e-9);
        assert!((r.market_supported_precision().unwrap() - 1.0 / 3.0).abs() < 1e-9);
        assert!((r.mean_drift().unwrap() - 3.0).abs() < 1e-9);
    }

    /// No checkable rows → every rate is `None`, never a fabricated 0% or NaN.
    #[test]
    fn rates_are_none_without_checkable_rows() {
        let r = RouteSummary::empty("x->y/a->b".into());
        assert!(r.precision().is_none());
        assert!(r.market_supported_precision().is_none());
        assert!(r.mean_drift().is_none());
    }
}
