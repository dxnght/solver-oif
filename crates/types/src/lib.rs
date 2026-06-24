//! Shared types for the solver engine. See `solver-engine-spec.md` §2.
//!
//! The normalization layer lets us speak one language across heterogenous
//! intent sources (Across, UniswapX, OIF). The cost of adding a new source is
//! mapping its raw order into [`NormalizedIntent`] — nothing else changes.

mod amount;
mod chain;
mod fill;
mod intent;
mod quote;
mod settlement;

pub use amount::{asset_class, known_decimals, AssetAmount, AssetClass, TokenRef};
pub use chain::ChainId;
pub use fill::{FillPolicy, FillReason};
pub use intent::{DutchDecay, ExclusivityWindow, IntentId, NormalizedIntent, Source, TxRef};
pub use quote::{GasOrigin, Quote, Route, RouteHop, RouteSource, SignedAmount, Wei};
pub use settlement::SettlementType;
