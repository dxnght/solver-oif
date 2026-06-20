//! Intent ingestion — historical and live sources normalized into one
//! [`solver_types::NormalizedIntent`] stream. See `solver-engine-spec.md` §3.
//!
//! Each source module is responsible for:
//!   1. Talking to its upstream API.
//!   2. Mapping the raw payload into [`solver_types::NormalizedIntent`].
//!   3. Streaming results out as JSONL (via [`store::JsonlSink`]) for the
//!      downstream backtest to consume.
//!
//! Verified-core scope (bucket B0): historical UniswapX only — the no-key
//! golden path. Across/live and other sources arrive in later buckets, one at
//! a time, behind their own verification — never as a horizontal sweep.

pub mod store;
pub mod uniswapx;

pub use store::JsonlSink;
pub use uniswapx::{UniswapXIngest, UniswapXIngestConfig, UNISWAPX_CHAINS};
