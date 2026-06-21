//! Canonical Across `FilledRelay` event (current bytes32 SpokePool generation).
//!
//! Source of truth is the `@across-protocol/contracts` npm package (v4.1.11)
//! SpokePool ABI — **not** memory (CLAUDE.md rule #2). The event was lifted
//! from the package artifact, its `topic0` recomputed with `cast`, and that
//! `topic0` then cross-checked against a **real** on-chain `FilledRelay` log on
//! the verified Arbitrum SpokePool (`0xe35e…5f2A`) — see the
//! [`tests::topic0_matches_canonical_signature`] lock and the normalize test in
//! `client.rs`, which decodes that captured log.
//!
//! Why `bytes32` (not `address`) token/relayer/recipient fields: the current
//! SpokePool generation widened EVM `address` fields to `bytes32` so one event
//! can carry non-EVM (e.g. Solana) participants. On EVM chains Across
//! left-pads the 20-byte address into the low 20 bytes of the word — recover it
//! with [`addr_from_bytes32`].

use alloy_primitives::{Address, B256};
use alloy_sol_types::sol;

sol! {
    /// Execution-time outcome carried inline in `FilledRelay`.
    /// `updatedOutputAmount` is what the relayer actually delivered — it can be
    /// reduced from `outputAmount` by a signed depositor "speed-up", so it is
    /// our realized-output ground truth, not `outputAmount`.
    #[derive(Debug)]
    struct V3RelayExecutionEventInfo {
        bytes32 updatedRecipient;
        bytes32 updatedMessageHash;
        uint256 updatedOutputAmount;
        uint8 fillType;
    }

    /// Across SpokePool `FilledRelay` (current bytes32 generation).
    ///
    /// AUDITED canonical signature — the `struct` above expands to its tuple
    /// form `(bytes32,bytes32,uint256,uint8)` in the event selector, giving:
    /// `FilledRelay(bytes32,bytes32,uint256,uint256,uint256,uint256,uint256,uint32,uint32,bytes32,bytes32,bytes32,bytes32,bytes32,(bytes32,bytes32,uint256,uint8))`
    /// topic0 = `0x44b559f101f8fbcc8a0ea43fa91a05a729a5ea6e14a7c75aa750374690137208`
    /// (locked by [`tests::topic0_matches_canonical_signature`]).
    #[derive(Debug)]
    event FilledRelay(
        bytes32 inputToken,
        bytes32 outputToken,
        uint256 inputAmount,
        uint256 outputAmount,
        uint256 repaymentChainId,
        uint256 indexed originChainId,
        uint256 indexed depositId,
        uint32 fillDeadline,
        uint32 exclusivityDeadline,
        bytes32 exclusiveRelayer,
        bytes32 indexed relayer,
        bytes32 depositor,
        bytes32 recipient,
        bytes32 messageHash,
        V3RelayExecutionEventInfo relayExecutionInfo,
    );
}

/// Across left-pads 20-byte EVM addresses into the low 20 bytes of a `bytes32`
/// word. Recover the `Address` from the trailing 20 bytes. A non-EVM (Solana)
/// participant would carry nonzero high bytes; for the EVM SpokePools we ingest
/// the high 12 bytes are zero, so this is lossless on our data.
pub fn addr_from_bytes32(b: B256) -> Address {
    Address::from_slice(&b[12..32])
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::keccak256;
    use alloy_sol_types::SolEvent;

    /// AX-1 topic0 lock — **independent truth**, not a constant vs itself.
    ///
    /// `FilledRelay::SIGNATURE_HASH` is assembled by the `sol!` macro from the
    /// *typed* field declarations above (it canonicalises each type and expands
    /// the nested struct to a tuple). We compare it against the keccak of the
    /// signature string we audited with `cast` and saw on-chain. The two paths
    /// are independent: a wrong field type or order in the binding would shift
    /// `SIGNATURE_HASH` and break this test — which is exactly the AE-gap
    /// failure mode (a silently-empty `getLogs`) we are guarding against.
    #[test]
    fn topic0_matches_canonical_signature() {
        const CANONICAL: &str = "FilledRelay(bytes32,bytes32,uint256,uint256,uint256,uint256,uint256,uint32,uint32,bytes32,bytes32,bytes32,bytes32,bytes32,(bytes32,bytes32,uint256,uint8))";
        assert_eq!(
            FilledRelay::SIGNATURE,
            CANONICAL,
            "sol! signature drifted from the audited canonical signature"
        );
        assert_eq!(
            FilledRelay::SIGNATURE_HASH,
            keccak256(CANONICAL.as_bytes()),
            "topic0 must equal keccak of the canonical signature"
        );
        // The audited literal (what we matched against real Arbitrum logs).
        assert_eq!(
            format!("{:#x}", FilledRelay::SIGNATURE_HASH),
            "0x44b559f101f8fbcc8a0ea43fa91a05a729a5ea6e14a7c75aa750374690137208",
        );
    }

    /// `addr_from_bytes32` recovers the low-20-byte EVM address from a
    /// left-padded word, and zeroes-out for the canonical zero word (the
    /// "no exclusive relayer" sentinel). Uses the real relayer word from the
    /// captured log as independent truth.
    #[test]
    fn addr_from_bytes32_recovers_low_20_bytes() {
        // topic3 (relayer) of the captured Arbitrum fill 0x234ddc0d…
        let relayer: B256 =
            "0x000000000000000000000000d0588c940bdbfcfb5f3e8e3c843ed1462d9f828e"
                .parse()
                .unwrap();
        assert_eq!(
            addr_from_bytes32(relayer),
            "0xd0588c940bdbfcfb5f3e8e3c843ed1462d9f828e"
                .parse::<Address>()
                .unwrap()
        );
        assert_eq!(addr_from_bytes32(B256::ZERO), Address::ZERO);
    }
}
