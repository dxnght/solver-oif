//! Canonical Across SpokePool events (current bytes32 generation):
//! [`FilledRelay`] (destination-chain fill, historical ingest) and
//! [`FundsDeposited`] (origin-chain deposit, live shadow — bucket B4).
//!
//! Source of truth is the `@across-protocol/contracts` npm package (v4.1.11)
//! SpokePool ABI — **not** memory (CLAUDE.md rule #2). Each event was lifted
//! from the package artifact, its `topic0` recomputed with `cast`, and that
//! `topic0` then cross-checked against a **real** on-chain log on the verified
//! Arbitrum SpokePool (`0xe35e…5f2A`):
//!   - `FilledRelay` — see [`tests::topic0_matches_canonical_signature`] and
//!     the normalize test in `client.rs`, which decodes a captured fill log.
//!   - `FundsDeposited` — see [`tests::funds_deposited_topic0_matches_real_log`],
//!     which locks the `topic0` against both `keccak(signature)` and the
//!     `topics[0]` of a real captured deposit log (testdata fixture).
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

    /// Across SpokePool `FundsDeposited` — the **origin-chain deposit** that
    /// opens an intent (B4 live shadow). Emitted by the SpokePool on the chain
    /// the user deposits on; `destinationChainId` (indexed) names where the fill
    /// must land. Counterpart to `FilledRelay` (the dest-chain fill).
    ///
    /// AUDITED canonical signature — lifted verbatim from the
    /// `@across-protocol/contracts` 4.1.11 `SpokePool.sol` artifact ABI:
    /// `FundsDeposited(bytes32,bytes32,uint256,uint256,uint256,uint256,uint32,uint32,uint32,bytes32,bytes32,bytes32,bytes)`
    /// topic0 = `0x32ed1a409ef04c7b0227189c3a103dc5ac10e775a15b785dcc510201f7c25ad3`
    /// (`cast keccak` of the signature; cross-checked against a real Arbitrum
    /// deposit log, tx `0x7bcd2bf7…`, in
    /// [`tests::funds_deposited_topic0_matches_real_log`]).
    ///
    /// Indexed (→ topics): `destinationChainId`, `depositId`, `depositor`.
    /// bytes32 token/relayer/recipient/depositor fields are left-padded EVM
    /// addresses — recover with [`addr_from_bytes32`]. Unlike `FilledRelay`,
    /// a deposit carries **no** realized output or winning relayer (it is still
    /// open) — the adapter sets those `None` (B4 LIV-3).
    #[derive(Debug)]
    event FundsDeposited(
        bytes32 inputToken,
        bytes32 outputToken,
        uint256 inputAmount,
        uint256 outputAmount,
        uint256 indexed destinationChainId,
        uint256 indexed depositId,
        uint32 quoteTimestamp,
        uint32 fillDeadline,
        uint32 exclusivityDeadline,
        bytes32 indexed depositor,
        bytes32 recipient,
        bytes32 exclusiveRelayer,
        bytes message,
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

    /// Real Arbitrum SpokePool `FundsDeposited` log (tx `0x7bcd2bf7…`),
    /// Arbitrum→Optimism USDT, captured verbatim from `getLogs`. Independent
    /// truth for the deposit topic0 and (in `live`) the normalize path.
    const REAL_DEPOSIT_LOG: &str = include_str!("testdata/fundsdeposited_arb_usdt.json");

    /// LIV-1 deposit topic0 lock — the same independent-truth shape as the
    /// `FilledRelay` lock, but with an added **real-log** cross-check.
    ///
    /// Three independent paths must agree: (1) the `sol!`-derived
    /// `SIGNATURE` string from the *typed* field declarations; (2) the keccak
    /// of the signature string we audited from the package ABI and computed
    /// with `cast`; (3) the `topics[0]` of a deposit log actually emitted
    /// on-chain (the committed fixture). A wrong field type/order in the
    /// binding shifts (1)/(2); a wrong audited signature shifts (2) away from
    /// (3) — either way this fails, which is the silently-empty-subscription
    /// failure mode (no logs would ever match) we are guarding against.
    #[test]
    fn funds_deposited_topic0_matches_real_log() {
        const CANONICAL: &str = "FundsDeposited(bytes32,bytes32,uint256,uint256,uint256,uint256,uint32,uint32,uint32,bytes32,bytes32,bytes32,bytes)";
        assert_eq!(
            FundsDeposited::SIGNATURE,
            CANONICAL,
            "sol! signature drifted from the audited canonical signature"
        );
        assert_eq!(
            FundsDeposited::SIGNATURE_HASH,
            keccak256(CANONICAL.as_bytes()),
            "topic0 must equal keccak of the canonical signature"
        );
        let topic0_hex = format!("{:#x}", FundsDeposited::SIGNATURE_HASH);
        assert_eq!(
            topic0_hex,
            "0x32ed1a409ef04c7b0227189c3a103dc5ac10e775a15b785dcc510201f7c25ad3",
        );
        // Real-log cross-check: topics[0] of an on-chain deposit log.
        let env: serde_json::Value = serde_json::from_str(REAL_DEPOSIT_LOG).unwrap();
        let real_topic0 = env["result"][0]["topics"][0].as_str().unwrap();
        assert_eq!(
            real_topic0.to_ascii_lowercase(),
            topic0_hex,
            "captured deposit log topic0 must equal the canonical topic0"
        );
        // 4 topics = topic0 + 3 indexed (destinationChainId, depositId, depositor).
        assert_eq!(env["result"][0]["topics"].as_array().unwrap().len(), 4);
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
