//! OIF (Open Intents Framework / ERC-7683) — currently just the verified
//! contract address (see [`deployments`]). No live or historical ingest
//! adapter exists yet; `oif` stays `Idle` everywhere it's referenced as a
//! source until one is built on top of this.

pub mod deployments;

pub use deployments::{input_settler_escrow, INPUT_SETTLER_ESCROW, OIF_CHAINS};
