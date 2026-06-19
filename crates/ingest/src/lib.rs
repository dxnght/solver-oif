//! Intent ingestion — historical and live sources normalized into one
//! [`solver_types::NormalizedIntent`] stream. See `solver-engine-spec.md` §3.
//!
//! Verified-core scope (bucket B0): historical UniswapX only (the no-key
//! golden path). Across/live and other sources arrive in later buckets, one
//! at a time, behind their own verification — never as a horizontal sweep.
