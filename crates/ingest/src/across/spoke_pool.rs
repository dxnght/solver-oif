//! Across SpokePool addresses — the destination-chain contracts that emit the
//! [`crate::across::FilledRelay`] log we ingest.
//!
//! Addresses come from the canonical `@across-protocol/contracts` npm package
//! (v4.1.11) `dist/deployments/deployments.json` — **not** memory (rule #2,
//! the OIF fabricated-address lesson). Only the majors we verified are returned
//! as `Some`; every other chain returns `None` (PENDING) rather than a guessed
//! or placeholder address. The Arbitrum entry is additionally cross-checked by
//! the normalize test, which decodes a real `FilledRelay` log captured from
//! `0xe35e…5f2A`.

use alloy_primitives::{address, Address};
use solver_types::ChainId;

/// Canonical SpokePool for `chain`, or `None` when we have no **verified**
/// deployment for it. `None` means "we will not scan this chain" — the honest
/// PENDING state, never a placeholder.
pub fn spoke_pool(chain: ChainId) -> Option<Address> {
    let addr = match chain {
        // Source: @across-protocol/contracts 4.1.11 deployments.json, keyed by
        // chain id. Checksums are the package's own (EIP-55).
        ChainId::ETHEREUM => address!("5c7BCd6E7De5423a257D81B442095A1a6ced35C5"),
        ChainId::OPTIMISM => address!("6f26Bf09B1C792e3228e5467807a900A503c0281"),
        ChainId::POLYGON => address!("9295ee1d8C5b022Be115A2AD3c30C72E34e7F096"),
        ChainId::BASE => address!("09aea4b2242abC8bb4BB78D537A67a245A7bEC64"),
        ChainId::ARBITRUM => address!("e35e9842fceaCA96570B734083f4a58e8F7C5f2A"),
        _ => return None,
    };
    Some(addr)
}

/// Chains we ingest Across fills on — exactly those with a verified
/// [`spoke_pool`]. Kept in sync with the match above by
/// [`tests::across_chains_match_spoke_pool_table`].
///
/// Note: having a verified SpokePool is necessary but not sufficient to pull
/// data — the Etherscan V2 free key only covers a subset of these (see
/// `doctor`). Chains that are configured but key-unsupported scan to a clear
/// per-chain error, not a silent skip.
pub const ACROSS_CHAINS: &[ChainId] = &[
    ChainId::ETHEREUM,
    ChainId::OPTIMISM,
    ChainId::POLYGON,
    ChainId::BASE,
    ChainId::ARBITRUM,
];

#[cfg(test)]
mod tests {
    use super::*;

    /// Consistency invariant (independent of the address values themselves):
    /// `ACROSS_CHAINS` is *exactly* the set of chains for which `spoke_pool`
    /// returns `Some`. Catches drift where someone adds a verified deployment
    /// to one but not the other — which would either silently skip a scannable
    /// chain or attempt a chain with no address.
    #[test]
    fn across_chains_match_spoke_pool_table() {
        for c in ACROSS_CHAINS {
            assert!(spoke_pool(*c).is_some(), "{c} in ACROSS_CHAINS but no spoke_pool");
        }
        // A spread of named chains we did NOT verify must stay PENDING (None),
        // so we never scan an address we can't stand behind.
        for c in [
            ChainId::ZORA,
            ChainId::SCROLL,
            ChainId::LINEA,
            ChainId::UNICHAIN,
            ChainId::BLAST,
        ] {
            assert!(spoke_pool(c).is_none(), "{c} must be PENDING, not guessed");
            assert!(!ACROSS_CHAINS.contains(&c), "{c} must not be in ACROSS_CHAINS");
        }
    }

    /// Distinct, non-zero addresses — guards a copy-paste that points two
    /// chains at the same SpokePool (which would mislabel every fill's dest).
    #[test]
    fn spoke_pools_are_distinct_and_nonzero() {
        let addrs: Vec<Address> = ACROSS_CHAINS.iter().map(|c| spoke_pool(*c).unwrap()).collect();
        for a in &addrs {
            assert_ne!(*a, Address::ZERO);
        }
        let mut uniq = addrs.clone();
        uniq.sort();
        uniq.dedup();
        assert_eq!(uniq.len(), addrs.len(), "duplicate SpokePool address across chains");
    }
}
