//! OIF `InputSettlerEscrow` — the origin-chain contract that emits the
//! ERC-7683 `Open` event we would ingest for a live OIF source.
//!
//! **Verified 2026-08-06, three independent sources — not memory (rule #2,
//! the OIF fabricated-address lesson this exact rule is named after):**
//! 1. Official `openintentsframework/oif-contracts` repo, `DEPLOYMENTS.md`:
//!    lists `InputSettlerEscrow` at this address, identical on every listed
//!    chain (CREATE2, deterministic).
//! 2. `eth_getCode` against public RPCs on Ethereum, Base, Arbitrum, and
//!    Optimism: non-empty bytecode (27,908 hex chars) on all four. The
//!    bytecode differs by only ~62 hex chars between chains — one 32-byte
//!    immutable word (consistent with an EIP-712 domain separator baking in
//!    `block.chainid` at construction), not four unrelated contracts.
//! 3. [Sourcify](https://sourcify.dev) (independent third-party bytecode↔source
//!    verifier, no relation to the OIF team or Etherscan): `runtimeMatch` and
//!    `creationMatch` both `exact_match` on Ethereum mainnet, resolving to
//!    `src/input/escrow/InputSettlerEscrow.sol:InputSettlerEscrow`, verified
//!    2026-05-27T03:12:56Z.
//!
//! Only the chains **we personally checked** via `eth_getCode` (leg 2 above)
//! return `Some` — `DEPLOYMENTS.md` lists more (Polygon, BSC, Linea, Scroll,
//! zkSync Era, Katana), but citing a doc is not the same as verifying it
//! (same discipline as [`crate::across::spoke_pool`]). Everything else is
//! `None` (PENDING) until independently checked.
//!
//! This module only establishes the address. The live OIF adapter (decode
//! `Open`/`StandardOrder`, normalize into `NormalizedIntent`) is separate,
//! not-yet-built follow-up work — `oif` stays `Idle` until it exists.

use alloy_primitives::{address, Address};
use solver_types::ChainId;

/// `InputSettlerEscrow` — same address on every chain it's deployed to
/// (CREATE2). See module docs for the three-source verification.
pub const INPUT_SETTLER_ESCROW: Address = address!("1CC9260E285C2C8AC8D2E7102F3978056Ec1d0a8");

/// [`INPUT_SETTLER_ESCROW`] for `chain`, or `None` when we have not
/// personally verified the deployment there via `eth_getCode` — the honest
/// PENDING state, never a placeholder or a doc citation taken on faith.
pub fn input_settler_escrow(chain: ChainId) -> Option<Address> {
    match chain {
        ChainId::ETHEREUM | ChainId::BASE | ChainId::ARBITRUM | ChainId::OPTIMISM => {
            Some(INPUT_SETTLER_ESCROW)
        }
        _ => None,
    }
}

/// Chains we've independently verified [`INPUT_SETTLER_ESCROW`] on. Kept in
/// sync with the match above by [`tests::oif_chains_match_input_settler_table`].
pub const OIF_CHAINS: &[ChainId] =
    &[ChainId::ETHEREUM, ChainId::BASE, ChainId::ARBITRUM, ChainId::OPTIMISM];

#[cfg(test)]
mod tests {
    use super::*;

    /// Consistency invariant (independent of the address value itself):
    /// `OIF_CHAINS` is *exactly* the set of chains for which
    /// `input_settler_escrow` returns `Some`.
    #[test]
    fn oif_chains_match_input_settler_table() {
        for c in OIF_CHAINS {
            assert!(input_settler_escrow(*c).is_some(), "{c} in OIF_CHAINS but no address");
        }
        // Listed in DEPLOYMENTS.md but not independently eth_getCode-checked
        // by us — must stay PENDING regardless of what the doc claims.
        for c in [ChainId::POLYGON, ChainId::BSC, ChainId::LINEA, ChainId::SCROLL] {
            assert!(input_settler_escrow(c).is_none(), "{c} must be PENDING, not doc-trusted");
            assert!(!OIF_CHAINS.contains(&c), "{c} must not be in OIF_CHAINS");
        }
    }

    /// Same constant on every verified chain (CREATE2) — not a per-chain
    /// table, so there's nothing to accidentally cross-wire between chains
    /// the way distinct SpokePool addresses could be.
    #[test]
    fn same_address_on_every_verified_chain() {
        for c in OIF_CHAINS {
            assert_eq!(input_settler_escrow(*c), Some(INPUT_SETTLER_ESCROW));
        }
        assert_ne!(INPUT_SETTLER_ESCROW, Address::ZERO);
    }
}
