//! Pricing — gas and asset sourcing for quotes. See `solver-engine-spec.md` §4.
//!
//! Verified-core scope (bucket B0): offline/static prices only, so backtest is
//! deterministic and CI never reaches the network. Live gas (`eth_feeHistory`)
//! and live ETH/BTC arrive in bucket B3, behind a flag, default off.
