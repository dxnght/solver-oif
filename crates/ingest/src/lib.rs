//! Intent ingestion — historical and live sources normalized into one
//! [`solver_types::NormalizedIntent`] stream. See `solver-engine-spec.md` §3.
//!
//! Each source module is responsible for:
//!   1. Talking to its upstream API.
//!   2. Mapping the raw payload into [`solver_types::NormalizedIntent`].
//!   3. Streaming results out as JSONL (via [`store::JsonlSink`]) for the
//!      downstream backtest to consume.
//!
//! Verified sources (one at a time, each behind its own on-chain verification,
//! never a horizontal sweep): historical **UniswapX** (no-key REST orders) and
//! historical **Across** (`FilledRelay` logs via the Etherscan V2 multichain
//! API, key-gated). Live and further sources arrive in later buckets.

pub mod across;
pub mod live;
pub mod oif;
pub mod store;
pub mod uniswapx;

pub use across::{AcrossIngest, AcrossIngestConfig, ACROSS_CHAINS};
pub use live::{across_ws_var, AcrossLive, LiveSource, SourceKind};
pub use oif::{input_settler_escrow, INPUT_SETTLER_ESCROW, OIF_CHAINS};
pub use store::JsonlSink;
pub use uniswapx::{UniswapXIngest, UniswapXIngestConfig, UNISWAPX_CHAINS};
