//! OIF `InputSettlerEscrow` `Open` event — the origin-chain event that opens
//! an intent (ERC-7683-shaped), analogous to Across's `FundsDeposited`.
//!
//! Source of truth is the official `openintentsframework/oif-contracts` repo
//! (not memory — CLAUDE.md rule #2, named after exactly this class of bug):
//! `src/input/escrow/InputSettlerEscrow.sol` (`event Open`),
//! `src/input/types/StandardOrderType.sol` (`struct StandardOrder`), and
//! `src/input/types/MandateOutputType.sol` (`struct MandateOutput`).
//!
//! `topic0` is cross-checked **four** independent ways, not just against the
//! package source (more legs than the Across `FilledRelay` precedent, because
//! `StandardOrder` nests two dynamic arrays and is worth over-verifying):
//! 1. The `sol!`-derived [`Open::SIGNATURE_HASH`] from the *typed* struct
//!    fields below, against `cast keccak` of the same canonical signature
//!    computed by hand from the Solidity source
//!    ([`tests::topic0_matches_canonical_signature`]).
//! 2. That same hash against `topics[0]` of a **real** `Open` log, captured
//!    via `eth_getTransactionReceipt` on Base (tx `0x504e78d9…`, block
//!    `47245610`) — not Etherscan, straight from a public RPC
//!    ([`tests::open_topic0_matches_real_log`]).
//! 3. The real log's `data` independently decoded with `cast abi-decode`
//!    against the *exact same* tuple shape used below — before any Rust code
//!    ran, `cast` alone recovered sane real values: input token
//!    `0x833589fc…` = Base USDC, output settler `0x52602D7c…` (presumed
//!    `OutputSettlerSimple` by name — **not** independently verified; OIF-1
//!    only checked `InputSettlerEscrow`, see `deployments.rs`), output token
//!    `0x3c499c54…` = Polygon USDC (both already in
//!    [`solver_types::amount::STABLE_ADDRS`]).
//! 4. [Blockscout](https://base.blockscout.com)'s own independent ABI decode
//!    of the same log (from the verified source it holds, unrelated to this
//!    repo or `cast abi-decode`) reports the identical tuple shape:
//!    `Open(bytes32 indexed orderId, (address,uint256,uint256,uint32,uint32,
//!    address,uint256[2][],(bytes32,bytes32,uint256,bytes32,uint256,bytes32,
//!    bytes,bytes)[]) order)`.
//!
//! `orderId` is the only indexed field — the whole `StandardOrder` (inputs
//! and outputs included) is ABI-encoded into `data` as one dynamic tuple.

use alloy_sol_types::sol;

sol! {
    /// One leg of what the filler must deliver on `chainId`. `oracle` and
    /// `settler` are `bytes32` (not `address`) for the same non-EVM-carrying
    /// reason Across's current generation widened its fields — recover an EVM
    /// address with [`crate::across::addr_from_bytes32`] (shared helper, same
    /// left-padding convention).
    #[derive(Debug)]
    struct MandateOutput {
        bytes32 oracle;
        bytes32 settler;
        uint256 chainId;
        bytes32 token;
        uint256 amount;
        bytes32 recipient;
        bytes callbackData;
        bytes context;
    }

    /// One OIF intent. `inputs` is `[tokenAsUint256, amount][]` — the token is
    /// packed into a `uint256` the same way Across packs bytes32 fields (low
    /// 20 bytes an EVM address); `outputs` can in principle carry more than
    /// one leg (multiple outputs/chains), but this adapter's first cut only
    /// handles the single-input/single-output case real orders overwhelmingly
    /// are in practice (the captured fixture is one of each) — anything else
    /// is a deliberate skip, not a guess (see `normalize_open`, not yet wired).
    #[derive(Debug)]
    struct StandardOrder {
        address user;
        uint256 nonce;
        uint256 originChainId;
        uint32 expires;
        uint32 fillDeadline;
        address inputOracle;
        uint256[2][] inputs;
        MandateOutput[] outputs;
    }

    /// OIF `InputSettlerEscrow` `Open` — emitted by both `open()` and
    /// `openFor()` once input tokens are collected into escrow. Origin-chain,
    /// analogous to Across's `FundsDeposited` (the live shadow B4 pattern).
    ///
    /// AUDITED canonical signature (see module docs for the four-way check):
    /// `Open(bytes32,(address,uint256,uint256,uint32,uint32,address,uint256[2][],(bytes32,bytes32,uint256,bytes32,uint256,bytes32,bytes,bytes)[]))`
    /// topic0 = `0x9ff74bd56d00785b881ef9fa3f03d7b598686a39a9bcff89a6008db588b18a7b`.
    #[derive(Debug)]
    event Open(bytes32 indexed orderId, StandardOrder order);
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::keccak256;
    use alloy_sol_types::SolEvent;

    /// Independent-truth lock (mirrors
    /// `across::event::tests::topic0_matches_canonical_signature`): the
    /// `sol!`-derived signature/hash from the *typed* field declarations must
    /// equal `keccak256` of the canonical signature string hand-computed from
    /// the audited Solidity source (`cast keccak "Open(...)"`). A wrong field
    /// type/order in the binding shifts `SIGNATURE_HASH` and this fails.
    #[test]
    fn topic0_matches_canonical_signature() {
        const CANONICAL: &str = "Open(bytes32,(address,uint256,uint256,uint32,uint32,address,uint256[2][],(bytes32,bytes32,uint256,bytes32,uint256,bytes32,bytes,bytes)[]))";
        assert_eq!(
            Open::SIGNATURE, CANONICAL,
            "sol! signature drifted from the audited canonical signature"
        );
        assert_eq!(
            Open::SIGNATURE_HASH,
            keccak256(CANONICAL.as_bytes()),
            "topic0 must equal keccak of the canonical signature"
        );
        assert_eq!(
            format!("{:#x}", Open::SIGNATURE_HASH),
            "0x9ff74bd56d00785b881ef9fa3f03d7b598686a39a9bcff89a6008db588b18a7b",
        );
    }

    /// Real Base `Open` log (tx `0x504e78d9…`), captured verbatim via
    /// `eth_getTransactionReceipt` on a public RPC. Independent truth for the
    /// topic0 lock and (in `live`) the normalize path.
    const REAL_OPEN_LOG: &str = include_str!("testdata/open_base_usdc_to_polygon.json");

    /// The canonical topic0 must equal `topics[0]` of a log OIF's own
    /// `InputSettlerEscrow` actually emitted on Base — the same "would a real
    /// subscription ever match anything" check as
    /// `funds_deposited_topic0_matches_real_log`.
    #[test]
    fn open_topic0_matches_real_log() {
        let env: serde_json::Value = serde_json::from_str(REAL_OPEN_LOG).unwrap();
        let log = &env["result"][0];
        let real_topic0 = log["topics"][0].as_str().unwrap();
        assert_eq!(
            real_topic0.to_ascii_lowercase(),
            format!("{:#x}", Open::SIGNATURE_HASH),
            "captured Open log topic0 must equal the canonical topic0"
        );
        // topic0 + 1 indexed (orderId) — order itself is unindexed (in data).
        assert_eq!(log["topics"].as_array().unwrap().len(), 2);
        assert_eq!(
            log["address"].as_str().unwrap(),
            "0x1cc9260e285c2c8ac8d2e7102f3978056ec1d0a8",
            "captured log must be from the OIF-1-verified InputSettlerEscrow"
        );
    }

    /// The real log's `data` decodes through the `sol!` binding into sane,
    /// independently-verifiable values — not just a topic0 match. Single
    /// input, single output (validates scoping the adapter to that case
    /// first): Base USDC in, Polygon USDC out, routed through an output
    /// settler (`0x52602D7c…`, presumed `OutputSettlerSimple` by name — not
    /// independently verified, see module docs). Every value here was
    /// cross-checked by hand with `cast abi-decode` before this test was
    /// written (see module docs) — this test locks that same decode in Rust.
    #[test]
    fn open_log_decodes_to_real_order() {
        use alloy_primitives::{address, b256, B256, U256};

        let env: serde_json::Value = serde_json::from_str(REAL_OPEN_LOG).unwrap();
        let log = &env["result"][0];
        let topics: Vec<B256> = log["topics"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t.as_str().unwrap().parse().unwrap())
            .collect();
        let data = alloy_primitives::hex::decode(log["data"].as_str().unwrap()).unwrap();

        let ev = Open::decode_raw_log(topics, &data, true).expect("real Open log must decode");
        assert_eq!(
            ev.orderId,
            b256!("b7bb0acf1e6b454e91e575b8607b0b9fb5fba69608804c3e15da3f58fafe3cc1")
        );

        let order = &ev.order;
        assert_eq!(order.user, address!("8e0bBb4e85Bcd3941e141E87B81d117FdC3c956B"));
        assert_eq!(order.originChainId, U256::from(8453u64)); // Base
        assert_eq!(order.inputs.len(), 1, "captured order has exactly one input");
        assert_eq!(order.outputs.len(), 1, "captured order has exactly one output");

        // inputs[0] = [tokenAsUint256, amount]; the token is Base USDC packed
        // into the low 20 bytes, independently confirmed by `cast abi-decode`
        // and by python hex() of the raw uint256 before any Rust ran.
        let [input_token_u256, input_amount] = order.inputs[0];
        assert_eq!(
            format!("{input_token_u256:#x}"),
            "0x833589fcd6edb6e08f4c7c32d4f71b54bda02913", // Base USDC
        );
        assert_eq!(input_amount, U256::from(150_000u64)); // 0.15 USDC (6dp)

        let out = &order.outputs[0];
        assert_eq!(out.chainId, U256::from(137u64)); // Polygon
        assert_eq!(
            out.settler,
            "0x00000000000000000000000052602d7cc3d833f5d28ee6d01c7f82c9b2322e10"
                .parse::<B256>()
                .unwrap(),
            "output settler decode must match the real captured log (presumed \
             OutputSettlerSimple by name — not independently verified, see module docs)"
        );
        assert_eq!(out.amount, U256::from(143_000u64)); // 0.143 USDC (6dp)
    }
}
