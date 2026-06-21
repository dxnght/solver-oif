//! Live (real-time) intent sources for the Phase-1 shadow run.
//!
//! Where the historical Across path scans **destination**-chain `FilledRelay`
//! logs after the fact (see [`crate::across`]), the live path subscribes to
//! **origin**-chain `FundsDeposited` logs as they happen and runs the same
//! shadow router/policy decision — zero transactions (`solver-engine-spec.md`
//! §0, Phase 1).
//!
//! [`ws`] is the transport: a thin `eth_subscribe("logs")` WebSocket client.
//! Later in this bucket a `LiveSource` trait and the Across adapter decode each
//! streamed log into a [`solver_types::NormalizedIntent`] on top of it.

pub mod ws;
