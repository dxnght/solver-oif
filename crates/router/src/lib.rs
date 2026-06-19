//! Router — the edge core. Turns a [`solver_types::NormalizedIntent`] into a
//! [`solver_types::Quote`] and a fill decision. See `solver-engine-spec.md` §5.
//!
//! Verified-core scope (bucket B0): RouterV0 — asset-class routing, Dutch
//! decay, confidence, `QuoteOutcome`/`SkipReason` — with the single
//! [`solver_types::FillPolicy`] making the `would_fill` decision identically in
//! backtest and live.
