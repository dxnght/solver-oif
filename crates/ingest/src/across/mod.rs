//! Across historical fill ingestion (cross-chain observability shader).
//!
//! Scope framing (bucket B2): the edge≈0 thesis is already closed (B1). Across
//! is added **not** to hunt for edge but as a cross-chain observability layer —
//! and to run the B1 would_fill calibration on cross-chain data for the first
//! time. For Across the margin is fixed at deposit time (`inputAmount −
//! outputAmount`, no Dutch auction), so would_fill drift here reflects mostly
//! the error in our static USD inputs, not auction dynamics — expected.
//!
//! We scan the **destination**-chain `FilledRelay` log (the bytes32 SpokePool
//! generation) per supported chain and normalize each into one
//! [`solver_types::NormalizedIntent`]. This is a *correct ingest*, not
//! analytics: the deposits side / unfilled-intent JOIN is a separate bucket.

pub mod client;
pub mod event;
pub mod spoke_pool;

pub use client::{AcrossIngest, AcrossIngestConfig, RawLog};
pub use event::{addr_from_bytes32, FilledRelay, FundsDeposited, V3RelayExecutionEventInfo};
pub use spoke_pool::{spoke_pool, ACROSS_CHAINS};
