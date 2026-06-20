use alloy_primitives::{Address, U256};
use async_trait::async_trait;
use solver_types::ChainId;

use crate::{PriceError, Pricer, SwapQuote};

/// Identity pricer: returns `buy_amount == sell_amount` with zero gas.
///
/// Used for:
/// - `--offline-prices` CLI flag: no network calls, runs fast in CI.
/// - `--reproducible` mode: deterministic output across runs.
///
/// Accuracy: exact for same-token routes; reasonable approximation for
/// stable↔stable (both 1 USD). Intentionally wrong for volatile cross-token
/// swaps — use only where you know the tokens are same-class.
pub struct OfflineIdentityPricer;

#[async_trait]
impl Pricer for OfflineIdentityPricer {
    async fn quote_sell_exact_in(
        &self,
        chain: ChainId,
        sell_token: Address,
        buy_token: Address,
        sell_amount: U256,
    ) -> Result<SwapQuote, PriceError> {
        Ok(SwapQuote {
            chain,
            sell_token,
            buy_token,
            sell_amount,
            buy_amount: sell_amount,
            gas_native: U256::ZERO,
            provider: "offline-identity",
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The defining invariant of the identity pricer: buy == sell, zero gas,
    /// regardless of the amount. This is what makes the backtest deterministic
    /// — a behavioral property, not a constant compared with itself.
    #[tokio::test]
    async fn identity_returns_input_amount_and_zero_gas() {
        let p = OfflineIdentityPricer;
        let amt = U256::from(123_456_789u64);
        let q = p
            .quote_sell_exact_in(ChainId::BASE, Address::ZERO, Address::repeat_byte(0x11), amt)
            .await
            .unwrap();
        assert_eq!(q.buy_amount, amt);
        assert_eq!(q.sell_amount, amt);
        assert_eq!(q.gas_native, U256::ZERO);
        assert_eq!(q.provider, "offline-identity");
    }
}
