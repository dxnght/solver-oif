//! Backtest — load (dedup + hygiene), metrics (incl. would_fill calibration),
//! and report (with verify-links). See `solver-engine-spec.md` §6.
//!
//! Verified-core scope (bucket B0): the no-key cycle `ingest uniswapx →
//! backtest` must yield a non-empty report whose every intent carries a
//! verifiable block-explorer link. `would_fill` is a model belief here; it
//! becomes trust only once validated against realized outcomes (bucket B1).
