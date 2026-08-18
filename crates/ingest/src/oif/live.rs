//! OIF live source: polls origin-chain `InputSettlerEscrow` `Open` events, the
//! OIF analog of [`crate::live::AcrossLive`]'s `FundsDeposited` poll (same
//! shadow philosophy — zero transactions).
//!
//! Reuses the **same** checkpointed `eth_getLogs` transport ([`crate::live::poll`])
//! Across uses, over the **same** `ACROSS_RPC_<CHAIN>` endpoint (chain-generic,
//! not Across-specific — see [`crate::live::across_rpc_var`] docs); only the
//! address/topic0 being polled and the log→[`NormalizedIntent`] decode differ.
//!
//! **First-cut scope guard:** `StandardOrder` can in principle carry more
//! than one input and more than one output (see `oif::event` docs), but
//! `NormalizedIntent` models exactly one input leg and one output leg. Rather
//! than guess which input/output "counts" for a multi-leg order, this adapter
//! only normalizes the single-input/single-output case (the shape every real
//! order captured while building this was) and skips (loud `WARN`, never a
//! silent drop of data into a wrong shape) anything else — the honest PENDING
//! stance (CLAUDE.md rule #2), not a guess.

use std::path::PathBuf;
use std::time::Duration;

use alloy_primitives::{Address, B256, U256};
use alloy_sol_types::SolEvent;
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use tokio::sync::mpsc;
use tracing::{info, warn};

use solver_types::{
    known_decimals, AssetAmount, ChainId, IntentId, NormalizedIntent, SettlementType, Source,
    TokenRef, TxRef,
};

use crate::across::addr_from_bytes32;
use crate::live::{across_rpc_var, poll, LiveSource, SourceKind};
use crate::oif::{input_settler_escrow, Open};

/// Channel depth — same rationale/size as the Across live path (LOG_BUFFER /
/// INTENT_BUFFER in `crate::live`): generous enough that a brief decode stall
/// never drops a log at OIF's current (low) real-world order rate.
const INTENT_BUFFER: usize = 256;

/// A chain ready for a live OIF poll: a verified `InputSettlerEscrow`
/// (OIF-1) **and** a resolved, non-empty `ACROSS_RPC_<CHAIN>`.
#[derive(Debug, Clone)]
struct OpenChain {
    chain: ChainId,
    rpc_url: String,
    settler: Address,
}

/// Split requested origin chains into *open* / *idle* — mirrors
/// [`crate::live::classify`] but keyed on [`input_settler_escrow`] instead of
/// `spoke_pool`. Kept separate rather than generalizing the Across version:
/// two call sites don't yet justify the abstraction (rule #5), and the two
/// verified-address functions have different signatures.
fn classify<F>(chains: &[ChainId], resolve: F) -> (Vec<OpenChain>, Vec<ChainId>)
where
    F: Fn(&str) -> Option<String>,
{
    let mut open = Vec::new();
    let mut idle = Vec::new();
    for &chain in chains {
        match (input_settler_escrow(chain), resolve(&across_rpc_var(chain))) {
            (Some(settler), Some(url)) if !url.trim().is_empty() => {
                open.push(OpenChain { chain, rpc_url: url, settler });
            }
            _ => idle.push(chain),
        }
    }
    (open, idle)
}

/// Recover an EVM address from an OIF `inputs[i][0]` token slot: the token is
/// packed into the low 20 bytes of a `uint256` (see `oif::event` docs) — the
/// same left-padding convention [`addr_from_bytes32`] recovers from a
/// `bytes32`, just starting from a `U256`'s big-endian bytes instead.
fn addr_from_u256(v: U256) -> Address {
    Address::from_slice(&v.to_be_bytes::<32>()[12..32])
}

/// Decode a live OIF `Open` log (observed on origin `origin_chain`) into a
/// [`NormalizedIntent`]. Mirrors [`crate::live::normalize_deposit`]'s shape:
/// `origin_chain` is the polled chain, the intent is still open
/// (`winning_filler`/`realized_output` both `None`), `observed_at` is the
/// wall clock, and `tx_ref` points at the origin `open()` tx.
///
/// Errors (all a `WARN`-and-skip at the call site, never a panic): the log
/// doesn't decode as `Open`; `order.originChainId` disagrees with the chain
/// we're polling (would mean a misconfigured `ACROSS_RPC_<CHAIN>` pointed at
/// the wrong network — the contract itself refuses to let `open()` succeed
/// with a mismatched `originChainId`, so this should be unreachable on a
/// correctly-configured poll, but checking costs nothing and a silent
/// wrong-chain label is exactly the failure mode rule #9 exists to catch);
/// not exactly one input and one output (see module scope-guard docs).
pub fn normalize_open(origin_chain: ChainId, log: &poll::RawLog) -> Result<NormalizedIntent> {
    let topics: Vec<B256> = log
        .topics
        .iter()
        .map(|t| t.parse::<B256>())
        .collect::<std::result::Result<_, _>>()
        .context("unparsable log topic")?;
    let data = alloy_primitives::hex::decode(log.data.trim()).context("unparsable log data hex")?;
    let ev = Open::decode_raw_log(topics, &data, true).map_err(|e| anyhow::anyhow!("decoding Open: {e}"))?;
    let order = &ev.order;

    let event_origin = u64::try_from(order.originChainId)
        .map_err(|_| anyhow::anyhow!("originChainId {} exceeds u64", order.originChainId))?;
    if event_origin != origin_chain.0 {
        anyhow::bail!(
            "Open log's originChainId ({event_origin}) does not match the polled chain \
             ({origin_chain}) — check ACROSS_RPC_{} points at the right network",
            origin_chain.name().to_uppercase()
        );
    }

    if order.inputs.len() != 1 || order.outputs.len() != 1 {
        anyhow::bail!(
            "order has {} input(s) and {} output(s) — only single-input/single-output orders \
             are normalized today (see oif::live module docs)",
            order.inputs.len(),
            order.outputs.len()
        );
    }
    let [input_token, input_amount] = order.inputs[0];
    let output = &order.outputs[0];
    let dest_chain = ChainId(
        u64::try_from(output.chainId)
            .map_err(|_| anyhow::anyhow!("output chainId {} exceeds u64", output.chainId))?,
    );

    let in_tok = TokenRef::new(origin_chain, addr_from_u256(input_token));
    let out_tok = TokenRef::new(dest_chain, addr_from_bytes32(output.token));
    let in_dec = known_decimals(in_tok);
    let out_dec = known_decimals(out_tok);

    let observed_at = Utc::now();
    let deadline = DateTime::<Utc>::from_timestamp(order.fillDeadline as i64, 0).unwrap_or(observed_at);

    let id = IntentId(format!("oif:{}", ev.orderId));
    let raw = serde_json::to_value(log).unwrap_or(serde_json::Value::Null);

    Ok(NormalizedIntent {
        id,
        source: Source::Oif,
        origin_chain,
        dest_chain,
        input: AssetAmount::new(in_tok, input_amount, in_dec),
        output_min: AssetAmount::new(out_tok, output.amount, out_dec),
        output_decay: None, // StandardOrder has no Dutch-decay schedule.
        deadline,
        // OIF escrows the input at open() and releases it on proof of fill —
        // exactly the ResourceLock shape (see solver_types::SettlementType).
        settlement: SettlementType::ResourceLock,
        observed_at,
        winning_filler: None,  // open — not yet filled
        realized_output: None, // open — no realized output yet
        // No verified exclusivity-window field on StandardOrder itself (unlike
        // Across's explicit exclusiveRelayer) — MandateOutput.context could
        // encode one per some order type, but that encoding isn't verified
        // here, so honestly None rather than a guess (rule #2).
        exclusivity: None,
        tx_ref: Some(TxRef { chain: origin_chain, tx_hash: log.transaction_hash.clone() }),
        raw,
    })
}

/// OIF live source: polls origin-chain `InputSettlerEscrow` `Open` events.
/// Mirrors [`crate::live::AcrossLive`] exactly at the `LiveSource` boundary —
/// `solver live --sources oif` gets the same idle/open legibility and
/// shutdown semantics.
pub struct OifLive {
    chains: Vec<ChainId>,
    checkpoint_dir: PathBuf,
    interval: Duration,
}

impl OifLive {
    pub fn new(chains: Vec<ChainId>, checkpoint_dir: impl Into<PathBuf>, interval: Duration) -> Self {
        Self { chains, checkpoint_dir: checkpoint_dir.into(), interval }
    }
}

impl LiveSource for OifLive {
    fn name(&self) -> &'static str {
        "oif"
    }

    fn spawn(&self) -> (SourceKind, mpsc::Receiver<NormalizedIntent>) {
        let (open, idle) = classify(&self.chains, |var| std::env::var(var).ok());

        let open_names: Vec<&str> = open.iter().map(|o| o.chain.name()).collect();
        let idle_names: Vec<&str> = idle.iter().map(|c| c.name()).collect();
        info!(source = "oif", open = ?open_names, idle = ?idle_names, "live real-time-ish inventory (poll)");
        for c in &idle {
            warn!(source = "oif", chain = %c, var = %across_rpc_var(*c),
                "no ACROSS_RPC endpoint (or no verified InputSettlerEscrow) — chain idle");
        }

        let (tx, rx) = mpsc::channel(INTENT_BUFFER);
        if open.is_empty() {
            warn!(source = "oif", "no real-time-ish open source — idle");
            return (SourceKind::Idle, rx);
        }
        for oc in open {
            spawn_chain(oc, self.checkpoint_dir.clone(), self.interval, tx.clone());
        }
        drop(tx);
        (SourceKind::RealtimeOpen, rx)
    }
}

/// Wire one open chain: a transport task ([`poll::poll_loop`]) feeding a
/// decode task that normalizes each log onto the shared intents channel —
/// identical structure to Across's `spawn_chain`.
fn spawn_chain(
    oc: OpenChain,
    checkpoint_dir: PathBuf,
    interval: Duration,
    intents: mpsc::Sender<NormalizedIntent>,
) {
    let chain = oc.chain;
    let cfg = poll::PollConfig {
        rpc_url: oc.rpc_url,
        address: oc.settler.to_string(),
        topic0: format!("{:#x}", Open::SIGNATURE_HASH),
        label: chain.name().to_string(),
        checkpoint_path: checkpoint_dir.join(format!("checkpoint-oif-{}.json", chain.name())),
        interval,
    };

    let (log_tx, mut log_rx) = mpsc::channel::<poll::RawLog>(256);

    tokio::spawn(poll::poll_loop(cfg, log_tx));

    tokio::spawn(async move {
        while let Some(log) = log_rx.recv().await {
            match normalize_open(chain, &log) {
                Ok(intent) => {
                    if intents.send(intent).await.is_err() {
                        break;
                    }
                }
                Err(e) => warn!(chain = %chain, error = %e, "skipping live Open log"),
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The same real Base `Open` log the `oif::event` topic0/decode tests use
    /// — independent truth for the normalize path too, not a synthetic log.
    const REAL_OPEN_LOG: &str = include_str!("testdata/open_base_usdc_to_polygon.json");

    fn real_open_log() -> poll::RawLog {
        let env: serde_json::Value = serde_json::from_str(REAL_OPEN_LOG).unwrap();
        serde_json::from_value(env["result"][0].clone()).unwrap()
    }

    /// OIF now resolves the **same** `ACROSS_RPC_<CHAIN>` var Across polling
    /// (and the gas ladder) already use — no separate OIF-specific env surface.
    #[test]
    fn classify_uses_shared_across_rpc_var() {
        assert_eq!(across_rpc_var(ChainId::BASE), "ACROSS_RPC_BASE");
    }

    /// `addr_from_u256` recovers the low-20-byte EVM address from a packed
    /// token slot — checked against the real captured order's input token
    /// (Base USDC) and the zero sentinel.
    #[test]
    fn addr_from_u256_recovers_low_20_bytes() {
        let base_usdc = U256::from_str_radix("833589fcd6edb6e08f4c7c32d4f71b54bda02913", 16).unwrap();
        assert_eq!(addr_from_u256(base_usdc), "0x833589fcd6edb6e08f4c7c32d4f71b54bda02913".parse::<Address>().unwrap());
        assert_eq!(addr_from_u256(U256::ZERO), Address::ZERO);
    }

    /// End-to-end normalize on the real captured order: Base USDC (0.15) →
    /// Polygon USDC (0.143), `ResourceLock` settlement, still-open (no
    /// winning_filler/realized_output), tx_ref on the origin `open()` tx.
    /// Every figure here is the same independently-verified real value the
    /// `oif::event` decode test locks — this test proves the *mapping* into
    /// `NormalizedIntent`, not just the ABI decode.
    #[test]
    fn normalize_open_maps_real_order() {
        let log = real_open_log();
        let intent = normalize_open(ChainId::BASE, &log).expect("real single-io order must normalize");

        assert_eq!(intent.id, IntentId("oif:0xb7bb0acf1e6b454e91e575b8607b0b9fb5fba69608804c3e15da3f58fafe3cc1".to_string()));
        assert_eq!(intent.source, Source::Oif);
        assert_eq!(intent.origin_chain, ChainId::BASE);
        assert_eq!(intent.dest_chain, ChainId::POLYGON);

        assert_eq!(intent.input.token.address, "0x833589fcd6edb6e08f4c7c32d4f71b54bda02913".parse::<Address>().unwrap());
        assert_eq!(intent.input.amount, U256::from(150_000u64));
        assert_eq!(intent.input.decimals, 6); // Base USDC — known_decimals

        assert_eq!(intent.output_min.token.address, "0x3c499c542cef5e3811e1192ce70d8cc03d5c3359".parse::<Address>().unwrap());
        assert_eq!(intent.output_min.amount, U256::from(143_000u64));
        assert_eq!(intent.output_min.decimals, 6); // Polygon USDC

        assert_eq!(intent.settlement, SettlementType::ResourceLock);
        assert!(intent.winning_filler.is_none());
        assert!(intent.realized_output.is_none());
        assert!(intent.exclusivity.is_none());
        assert_eq!(
            intent.tx_ref,
            Some(TxRef {
                chain: ChainId::BASE,
                tx_hash: "0x504e78d99fcbcd1dd9c26a13906b475a1d5d720bf9b8cbed6ec68358844f1b57".to_string(),
            })
        );
    }

    /// A wrong polled-chain label (the "pointed `ACROSS_RPC_<CHAIN>` at the
    /// wrong network" misconfiguration) must be a loud error, not a
    /// mislabeled intent — the real order's `originChainId` is Base, so
    /// claiming it was observed on Arbitrum must fail.
    #[test]
    fn normalize_open_rejects_origin_chain_mismatch() {
        let log = real_open_log();
        let err = normalize_open(ChainId::ARBITRUM, &log).unwrap_err().to_string();
        assert!(err.contains("does not match the polled chain"), "got: {err}");
    }
}
