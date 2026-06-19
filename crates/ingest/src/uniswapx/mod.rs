//! UniswapX historical orders ingestion.
//!
//! UniswapX exposes a public REST API at
//! `https://api.uniswap.org/v2/orders` that returns paginated orders by
//! status. We pull `filled` orders for the last `days_back` window per
//! supported chain.
//!
//! Caveat: most UniswapX flow is **single-chain** swaps; cross-chain orders
//! exist via the Cross-Chain Reactor but volume is still small. We ingest
//! both; metrics aggregation can filter by `origin_chain == dest_chain`.

pub mod client;

pub use client::{UniswapXIngest, UniswapXIngestConfig};
