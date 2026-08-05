//! USD price feed for the Router.
//!
//! Phase 0 uses a **static** feed so the backtest is deterministic and CI
//! never reaches the network. The numbers are explicit MODELED assumptions,
//! not live quotes — every USD figure the backtest reports is labeled static.
//! A live feed (CoinGecko / on-chain TWAP) arrives behind a flag in bucket B3.

use solver_types::{asset_class, AssetClass, ChainId, TokenRef};

/// USD price of one *whole* token (decimal-adjusted), or `None` when we have
/// no honest way to value it offline. `None` makes the Router skip the intent
/// rather than invent a number (CLAUDE.md rule #2).
pub trait PriceFeed: Send + Sync {
    fn usd_price(&self, token: TokenRef) -> Option<f64>;
}

/// Forward `PriceFeed` through a shared reference. Lets one feed value serve
/// both as the Router's owned feed and the backtest's realized-margin price
/// closure without cloning a live cache: the backtest passes `&feed` to
/// `compute`, which builds `RouterV0::new(&feed, …)` and still reads `feed`.
impl<T: PriceFeed + ?Sized> PriceFeed for &T {
    fn usd_price(&self, token: TokenRef) -> Option<f64> {
        (**self).usd_price(token)
    }
}

/// Static price feed. Stables resolve to $1 (genuinely near-peg); ETH/BTC use
/// the configured reference values; the native gas token resolves per chain.
/// Anything else (an unrecognised ERC-20) returns `None`.
#[derive(Debug, Clone, Copy)]
pub struct StaticPriceFeed {
    pub eth_usd: f64,
    pub btc_usd: f64,
    pub matic_usd: f64,
}

impl StaticPriceFeed {
    /// Reference snapshot — **modeled, not a live quote**. Deliberately round
    /// numbers for a deterministic Phase-0 backtest; the report labels every
    /// USD figure as static. Override with [`Self::new`] to stress a regime.
    pub fn reference() -> Self {
        Self {
            eth_usd: 3000.0,
            btc_usd: 100_000.0,
            matic_usd: 0.50,
        }
    }

    pub fn new(eth_usd: f64, btc_usd: f64, matic_usd: f64) -> Self {
        Self { eth_usd, btc_usd, matic_usd }
    }

    /// USD value of the chain's native gas token. `None` for chains whose
    /// native token we have not pinned — we refuse to guess (a wrong gas
    /// valuation silently distorts `expected_profit_usd`).
    fn native_usd(&self, chain: ChainId) -> Option<f64> {
        match chain {
            ChainId::POLYGON => Some(self.matic_usd),
            ChainId::ETHEREUM
            | ChainId::OPTIMISM
            | ChainId::ARBITRUM
            | ChainId::BASE
            | ChainId::UNICHAIN => Some(self.eth_usd),
            _ => None,
        }
    }
}

impl PriceFeed for StaticPriceFeed {
    fn usd_price(&self, token: TokenRef) -> Option<f64> {
        if token.is_native() {
            return self.native_usd(token.chain);
        }
        match asset_class(token) {
            AssetClass::Stable => Some(1.0),
            AssetClass::Eth => Some(self.eth_usd),
            AssetClass::Btc => Some(self.btc_usd),
            AssetClass::Unknown => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{address, Address};

    fn tok(chain: ChainId, a: Address) -> TokenRef {
        TokenRef::new(chain, a)
    }

    /// Each class resolves to its expected value and unknowns to `None` — the
    /// behavior the Router's skip-vs-quote branch depends on. Independent
    /// truth (stable == $1, WETH == configured eth) rather than readback.
    #[test]
    fn prices_by_class_and_native() {
        let f = StaticPriceFeed::new(3000.0, 100_000.0, 0.5);
        // Base USDC → $1.
        assert_eq!(f.usd_price(tok(ChainId::BASE, address!("833589fCD6eDb6E08f4c7C32D4f71b54bdA02913"))), Some(1.0));
        // Optimism WETH → eth_usd.
        assert_eq!(f.usd_price(tok(ChainId::OPTIMISM, address!("4200000000000000000000000000000000000006"))), Some(3000.0));
        // Native on Base → ETH; native on Polygon → MATIC.
        assert_eq!(f.usd_price(tok(ChainId::BASE, Address::ZERO)), Some(3000.0));
        assert_eq!(f.usd_price(tok(ChainId::POLYGON, Address::ZERO)), Some(0.5));
        // Native on a chain we have not pinned → None (no guessing).
        assert_eq!(f.usd_price(tok(ChainId::SCROLL, Address::ZERO)), None);
        // Unrecognised ERC-20 → None.
        assert_eq!(f.usd_price(tok(ChainId::BASE, Address::repeat_byte(0x42))), None);
    }
}
