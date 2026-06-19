use alloy_primitives::{address, Address, U256};
use serde::{Deserialize, Serialize};
use std::fmt;

use crate::chain::ChainId;

/// Decimals lookup for the high-volume tokens we recognize.
/// Returns `18` for unknown tokens (the EVM ERC-20 default).
///
/// This is intentionally not exhaustive — adding more entries here helps
/// the Phase 0 backtest qualify more intents into the Router. Anything
/// the [`crate::PriceFeed`] doesn't know will still be skipped, so the
/// fallback of 18 is safe (we never *use* decimals for those).
pub fn known_decimals(token: TokenRef) -> u8 {
    if token.is_native() {
        return 18;
    }
    // 6-decimal stables.
    let usdc_or_usdt = [
        // USDC variants
        address!("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48"), // Ethereum
        address!("af88d065e77c8cC2239327C5EDb3A432268e5831"), // Arbitrum native
        address!("0b2C639c533813f4Aa9D7837CAf62653d097Ff85"), // Optimism native
        address!("833589fCD6eDb6E08f4c7C32D4f71b54bdA02913"), // Base native
        address!("3c499c542cEF5E3811e1192ce70d8cC03d5c3359"), // Polygon native
        address!("176211869cA2b568f2A7D4EE941E073a821EE1ff"), // Linea
        address!("06eFdBFf2a14a7c8E15944D1F4A48F9F95F663A4"), // Scroll
        address!("d988097fb8612cc24eeC14542bC03424c656005f"), // Mode
        address!("06D7a7334B9329cE22452979Fe2160aaA050C7BB"), // Lisk
        // USDT variants
        address!("dAC17F958D2ee523a2206206994597C13D831ec7"), // Ethereum
        address!("Fd086bC7CD5C481DCC9C85ebE478A1C0b69FCbb9"), // Arbitrum
        address!("94b008aA00579c1307B0EF2c499aD98a8ce58e58"), // Optimism
        address!("c2132D05D31c914a87C6611C10748AEb04B58e8F"), // Polygon
    ];
    if usdc_or_usdt.contains(&token.address) {
        return 6;
    }
    // 8-decimal WBTC variants.
    let wbtc = [
        address!("2260FAC5E5542a773Aa44fBCfeDf7C193bc2C599"), // Ethereum
        address!("2f2a2543B76A4166549F7aaB2e75Bef0aefC5B0f"), // Arbitrum
        address!("68f180fcCe6836688e9084f035309E29Bf0A2095"), // Optimism
        address!("0555E30da8f98308EdB960aa94C0Db47230d2B9c"), // Base
        address!("1BFD67037B42Cf73acF2047067bd4F2C47D9BfD6"), // Polygon
    ];
    if wbtc.contains(&token.address) {
        return 8;
    }
    18
}

/// Reference to an ERC-20 token (or native asset).
///
/// `address = 0x0` is treated as native ETH/gas-token on the given chain
/// — matches the convention used by 1inch, 0x, Across, and most aggregators.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TokenRef {
    pub chain: ChainId,
    pub address: Address,
}

impl TokenRef {
    pub fn new(chain: ChainId, address: Address) -> Self {
        Self { chain, address }
    }

    pub fn is_native(self) -> bool {
        self.address == Address::ZERO
    }
}

impl fmt::Display for TokenRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.is_native() {
            write!(f, "{}:native", self.chain)
        } else {
            write!(f, "{}:{}", self.chain, self.address)
        }
    }
}

/// An amount of a specific token. EVM amounts are big — we carry the full
/// [`U256`] and remember decimals so we can render a human-readable view
/// without losing precision in storage.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct AssetAmount {
    pub token: TokenRef,
    pub amount: U256,
    pub decimals: u8,
}

impl AssetAmount {
    pub fn new(token: TokenRef, amount: U256, decimals: u8) -> Self {
        Self { token, amount, decimals }
    }

    /// Lossy conversion to `f64` for statistics. Acceptable for Phase 0 —
    /// 18-decimal amounts up to ~10^21 still keep 16 significant digits.
    pub fn to_f64(self) -> f64 {
        // Split the U256 into 64-bit limbs and reconstruct a float.
        let limbs = self.amount.as_limbs();
        let mut val: f64 = 0.0;
        for (i, limb) in limbs.iter().enumerate() {
            val += (*limb as f64) * 2f64.powi(64 * i as i32);
        }
        val / 10f64.powi(self.decimals as i32)
    }
}

impl fmt::Display for AssetAmount {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:.6} {}", self.to_f64(), self.token)
    }
}
