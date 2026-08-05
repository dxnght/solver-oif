//! Live (real-time) intent sources for the Phase-1 shadow run.
//!
//! Where the historical Across path scans **destination**-chain `FilledRelay`
//! logs after the fact (see [`crate::across`]), the live path subscribes to
//! **origin**-chain `FundsDeposited` logs as they happen and runs the same
//! shadow router/policy decision — zero transactions (`solver-engine-spec.md`
//! §0, Phase 1).
//!
//! [`ws`] is the transport (a thin `eth_subscribe("logs")` WebSocket client).
//! This module adds the [`LiveSource`] trait (an async stream of
//! [`NormalizedIntent`]) and the Across adapter on top: it resolves
//! `ACROSS_WS_<CHAIN>` per requested origin chain, subscribes to that chain's
//! [`spoke_pool`] + the `FundsDeposited` `topic0`, and decodes each log via
//! [`normalize_deposit`].
//!
//! Legibility (CLAUDE.md rule #7): which chains are *open* (a WS endpoint is
//! configured) vs *idle* (none) is announced at startup and reflected in
//! [`SourceKind`]; a chain with no `ACROSS_WS_<CHAIN>` is a loud WARN + skip,
//! never a crash and never silence.

pub mod ws;

use alloy_primitives::{Address, B256};
use alloy_sol_types::SolEvent;
use anyhow::{Context, Result};
use chrono::{DateTime, Duration, Utc};
use tokio::sync::mpsc;
use tracing::{error, info, warn};

use solver_types::{
    known_decimals, AssetAmount, ChainId, ExclusivityWindow, IntentId, NormalizedIntent,
    SettlementType, Source, TokenRef, TxRef,
};

use crate::across::client::ACROSS_OO_LIVENESS_SECS;
use crate::across::{addr_from_bytes32, spoke_pool, FundsDeposited};

/// Channel depths. Generous enough that a brief decode/decision stall never
/// drops a log under normal deposit rates; backpressure (not loss) if exceeded.
const LOG_BUFFER: usize = 256;
const INTENT_BUFFER: usize = 256;

/// Whether a live source currently has any real-time open subscription, or is
/// idle (nothing configured). Pure legibility — surfaced at startup so a silent
/// "subscribed but nothing happens" run is impossible (rule #7).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceKind {
    /// At least one chain has a configured WS endpoint and verified SpokePool.
    RealtimeOpen,
    /// Nothing configured — the source produces no intents (and says so).
    Idle,
}

/// A real-time intent source: spawns its subscriptions and streams normalized
/// intents out on an [`mpsc::Receiver`] (the async stream). One implementation
/// today — Across deposits ([`AcrossLive`]); OIF stays idle until its address
/// is verified (it is not, rule #2).
///
/// The caller drives the receiver and drops it to stop; the underlying transport
/// tasks observe the drop and shut down ([`ws::stream_logs`]).
pub trait LiveSource {
    /// Source name for logs / output filenames (e.g. `"across"`).
    fn name(&self) -> &'static str;

    /// Spawn the subscriptions and return `(kind, receiver)`. `kind` reports
    /// whether the source is actually open or idle; `receiver` yields each
    /// decoded [`NormalizedIntent`].
    fn spawn(&self) -> (SourceKind, mpsc::Receiver<NormalizedIntent>);
}

/// Environment variable holding a chain's Across WS endpoint: the convention is
/// `ACROSS_WS_<CHAIN>` where `<CHAIN>` is the upper-cased [`ChainId::name`]
/// (e.g. `ACROSS_WS_ARBITRUM`). A free function so the naming is testable and
/// matches what `doctor` (LIV-5) probes.
pub fn across_ws_var(chain: ChainId) -> String {
    format!("ACROSS_WS_{}", chain.name().to_uppercase())
}

/// A chain ready for a live subscription: it has both a verified SpokePool and
/// a resolved (non-empty) WS endpoint.
#[derive(Debug, Clone)]
pub struct OpenChain {
    pub chain: ChainId,
    pub ws_url: String,
    pub spoke: Address,
}

/// Split requested origin chains into *open* (verified SpokePool **and** a
/// non-empty `ACROSS_WS_<CHAIN>`) and *idle* (missing var, or no verified
/// SpokePool). `resolve` injects the env lookup so the split is unit-testable
/// without touching the process environment.
pub fn classify<F>(chains: &[ChainId], resolve: F) -> (Vec<OpenChain>, Vec<ChainId>)
where
    F: Fn(&str) -> Option<String>,
{
    let mut open = Vec::new();
    let mut idle = Vec::new();
    for &chain in chains {
        match (spoke_pool(chain), resolve(&across_ws_var(chain))) {
            (Some(spoke), Some(url)) if !url.trim().is_empty() => {
                open.push(OpenChain { chain, ws_url: url, spoke });
            }
            _ => idle.push(chain),
        }
    }
    (open, idle)
}

/// Decode a live Across `FundsDeposited` log (observed on origin `origin_chain`)
/// into a [`NormalizedIntent`].
///
/// Mirrors the historical [`crate::across::client`] fill normalize, but for the
/// **deposit** (open) side: `origin_chain` is the subscribed chain; `dest_chain`
/// is the event's `destinationChainId`; `tx_ref` points at the **origin**
/// deposit tx (not dest); and a deposit is still open, so `realized_output` and
/// `winning_filler` are `None`, `observed_at` is the wall clock (we see it now),
/// and the settlement constant is the same shared [`ACROSS_OO_LIVENESS_SECS`] as
/// the historical path. The `id` scheme matches the fill side
/// (`across:{origin}:{dest}:{depositId}`) so a deposit and its eventual fill
/// share one [`IntentId`].
pub fn normalize_deposit(origin_chain: ChainId, log: &ws::WsLog) -> Result<NormalizedIntent> {
    let topics: Vec<B256> = log
        .topics
        .iter()
        .map(|t| t.parse::<B256>())
        .collect::<std::result::Result<_, _>>()
        .context("unparsable log topic")?;
    let data = alloy_primitives::hex::decode(log.data.trim()).context("unparsable log data hex")?;
    let ev = FundsDeposited::decode_raw_log(topics, &data, true)
        .map_err(|e| anyhow::anyhow!("decoding FundsDeposited: {e}"))?;

    let dest_chain = ChainId(u64::try_from(ev.destinationChainId).map_err(|_| {
        anyhow::anyhow!("destinationChainId {} exceeds u64", ev.destinationChainId)
    })?);

    let in_tok = TokenRef::new(origin_chain, addr_from_bytes32(ev.inputToken));
    let out_tok = TokenRef::new(dest_chain, addr_from_bytes32(ev.outputToken));
    let in_dec = known_decimals(in_tok);
    let out_dec = known_decimals(out_tok);

    // Open / real-time: we observe the deposit now. `fillDeadline` bounds it.
    let observed_at = Utc::now();
    let deadline =
        DateTime::<Utc>::from_timestamp(ev.fillDeadline as i64, 0).unwrap_or(observed_at);

    // Exclusivity only when a designated relayer is set (nonzero word).
    let exclusivity = {
        let excl = addr_from_bytes32(ev.exclusiveRelayer);
        (!excl.is_zero()).then(|| ExclusivityWindow {
            exclusive_relayer: excl,
            deadline: DateTime::<Utc>::from_timestamp(ev.exclusivityDeadline as i64, 0)
                .unwrap_or(deadline),
        })
    };

    let id = IntentId(format!("across:{origin_chain}:{dest_chain}:{}", ev.depositId));
    let raw = serde_json::to_value(log).unwrap_or(serde_json::Value::Null);

    Ok(NormalizedIntent {
        id,
        source: Source::Across,
        origin_chain,
        dest_chain,
        input: AssetAmount::new(in_tok, ev.inputAmount, in_dec),
        output_min: AssetAmount::new(out_tok, ev.outputAmount, out_dec),
        output_decay: None, // Across margin is fixed at deposit — no Dutch decay.
        deadline,
        settlement: SettlementType::OptimisticOracle {
            challenge_window: Duration::seconds(ACROSS_OO_LIVENESS_SECS),
        },
        observed_at,
        winning_filler: None,  // open — not yet filled
        realized_output: None, // open — no realized output yet
        exclusivity,
        tx_ref: Some(TxRef {
            chain: origin_chain, // the DEPOSIT tx lives on the origin chain
            tx_hash: log.transaction_hash.clone(),
        }),
        raw,
    })
}

/// Across live source: subscribes to origin-chain `FundsDeposited` deposits.
pub struct AcrossLive {
    /// Requested origin chains (already parsed/validated by the caller).
    chains: Vec<ChainId>,
}

impl AcrossLive {
    pub fn new(chains: Vec<ChainId>) -> Self {
        Self { chains }
    }
}

impl LiveSource for AcrossLive {
    fn name(&self) -> &'static str {
        "across"
    }

    fn spawn(&self) -> (SourceKind, mpsc::Receiver<NormalizedIntent>) {
        let (open, idle) = classify(&self.chains, |var| std::env::var(var).ok());

        // Startup inventory — loud, before any data flows (rule #7).
        let open_names: Vec<&str> = open.iter().map(|o| o.chain.name()).collect();
        let idle_names: Vec<&str> = idle.iter().map(|c| c.name()).collect();
        info!(source = "across", open = ?open_names, idle = ?idle_names,
            "live real-time inventory");
        for c in &idle {
            warn!(source = "across", chain = %c, var = %across_ws_var(*c),
                "no ACROSS_WS endpoint — chain idle (no real-time stream)");
        }

        let (tx, rx) = mpsc::channel(INTENT_BUFFER);
        if open.is_empty() {
            warn!(source = "across", "no real-time open source — idle");
            return (SourceKind::Idle, rx); // tx dropped → receiver closes immediately
        }
        for oc in open {
            spawn_chain(oc, tx.clone());
        }
        // Drop our own sender so the receiver closes once every chain task ends.
        drop(tx);
        (SourceKind::RealtimeOpen, rx)
    }
}

/// Wire one open chain: a transport task (`ws::stream_logs`) feeding a decode
/// task that normalizes each log onto the shared intents channel.
fn spawn_chain(oc: OpenChain, intents: mpsc::Sender<NormalizedIntent>) {
    let chain = oc.chain;
    let cfg = ws::WsConfig {
        ws_url: oc.ws_url,
        address: oc.spoke.to_string(), // EIP-55 0x… address
        topic0: format!("{:#x}", FundsDeposited::SIGNATURE_HASH),
        label: chain.name().to_string(),
    };

    let (log_tx, mut log_rx) = mpsc::channel::<ws::WsLog>(LOG_BUFFER);

    // Transport: subscribe + stream raw logs (reconnect/backoff inside).
    tokio::spawn(async move {
        if let Err(e) = ws::stream_logs(cfg, log_tx).await {
            // Fatal (eth_subscribe rejected) — loud, not silent.
            error!(chain = %chain, error = %e, "live WS source stopped (fatal)");
        }
    });

    // Decode: raw log → NormalizedIntent → shared stream.
    tokio::spawn(async move {
        while let Some(log) = log_rx.recv().await {
            match normalize_deposit(chain, &log) {
                Ok(intent) => {
                    if intents.send(intent).await.is_err() {
                        break; // consumer gone — stop
                    }
                }
                Err(e) => warn!(chain = %chain, error = %e, "skipping live deposit log"),
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::U256;

    /// The SAME committed real Arbitrum deposit (tx `0x7bcd2bf7…`) used by the
    /// LIV-1 topic0 lock — reused here as independent truth for the live
    /// normalize path. Parsed straight from the Etherscan envelope into a
    /// [`ws::WsLog`] (extra Etherscan fields like `timeStamp` are ignored).
    const REAL_DEPOSIT: &str = include_str!("../across/testdata/fundsdeposited_arb_usdt.json");

    fn real_deposit_log() -> ws::WsLog {
        let env: serde_json::Value = serde_json::from_str(REAL_DEPOSIT).unwrap();
        serde_json::from_value(env["result"][0].clone()).unwrap()
    }

    /// The env-var convention is `ACROSS_WS_<UPPER-CHAIN-NAME>` — `doctor`
    /// (LIV-5) and the adapter must agree on this exact spelling, else a
    /// configured endpoint would look unset (silent idle).
    #[test]
    fn ws_var_name_is_across_ws_upper_chain() {
        assert_eq!(across_ws_var(ChainId::ARBITRUM), "ACROSS_WS_ARBITRUM");
        assert_eq!(across_ws_var(ChainId::OPTIMISM), "ACROSS_WS_OPTIMISM");
        assert_eq!(across_ws_var(ChainId::BASE), "ACROSS_WS_BASE");
    }

    /// `classify` splits open from idle by the injected resolver — open carries
    /// the resolved URL + verified spoke; a missing var (Optimism here) and an
    /// empty-string var (Base) both fall to idle (an empty endpoint is not a
    /// usable subscription).
    #[test]
    fn classify_splits_open_and_idle() {
        let chains = vec![ChainId::ARBITRUM, ChainId::OPTIMISM, ChainId::BASE];
        let (open, idle) = classify(&chains, |var| match var {
            "ACROSS_WS_ARBITRUM" => Some("wss://arb.example/ws".to_string()),
            "ACROSS_WS_BASE" => Some("   ".to_string()), // present but blank → idle
            _ => None,                                    // Optimism unset
        });
        let open_chains: Vec<ChainId> = open.iter().map(|o| o.chain).collect();
        assert_eq!(open_chains, vec![ChainId::ARBITRUM]);
        assert_eq!(open[0].ws_url, "wss://arb.example/ws");
        assert_eq!(open[0].spoke, spoke_pool(ChainId::ARBITRUM).unwrap());
        // Both the unset and the blank chain are idle.
        assert_eq!(idle, vec![ChainId::OPTIMISM, ChainId::BASE]);
    }

    /// LIV-3 core: a real deposit log decodes into an **open origin** intent —
    /// origin = the subscribed chain (Arbitrum), dest = `destinationChainId`
    /// (Optimism = 10), no realized output / winner yet, and `tx_ref` on the
    /// **origin** deposit tx (the deposit/fill asymmetry vs the historical
    /// path). Values are the fixture's independently-decoded ground truth.
    #[test]
    fn normalize_real_deposit_is_open_origin_intent() {
        let log = real_deposit_log();
        let intent = normalize_deposit(ChainId::ARBITRUM, &log).unwrap();

        assert_eq!(intent.source, Source::Across);
        assert_eq!(intent.origin_chain, ChainId::ARBITRUM); // the subscribed chain
        assert_eq!(intent.dest_chain, ChainId::OPTIMISM); // destinationChainId = 10
        assert_eq!(intent.id.0, "across:arbitrum:optimism:4413465"); // depositId 0x435819

        // Open / real-time invariants.
        assert!(intent.realized_output.is_none(), "deposit has no realized output yet");
        assert!(intent.winning_filler.is_none(), "deposit has no winner yet");
        assert!(intent.output_decay.is_none(), "Across has no Dutch decay");

        // tx_ref is the DEPOSIT tx, on the ORIGIN chain (not dest).
        let tx = intent.tx_ref.as_ref().expect("deposit carries its origin-chain tx");
        assert_eq!(tx.chain, ChainId::ARBITRUM);
        assert_eq!(
            tx.tx_hash,
            "0x7bcd2bf7968003eb3db3b1fa01e5be92760eceb72b34ad117ad3a83105d3fd24"
        );
        // and the verify_url therefore points at the origin explorer.
        assert_eq!(
            intent.verify_url(),
            Some(format!("https://arbiscan.io/tx/{}", tx.tx_hash))
        );

        // Clean fixture: zero exclusiveRelayer → no window.
        assert!(intent.exclusivity.is_none(), "exclusiveRelayer is zero → no window");

        // Same shared settlement constant as the historical fill path.
        assert!(matches!(
            intent.settlement,
            SettlementType::OptimisticOracle { challenge_window }
                if challenge_window.num_seconds() == ACROSS_OO_LIVENESS_SECS
        ));

        // Amounts/tokens decoded from the data, denominated per side. Hex
        // literals are lifted verbatim from the fixture's data words, so this
        // checks the field mapping, not a re-derivation.
        assert_eq!(intent.input.token.chain, ChainId::ARBITRUM);
        assert_eq!(intent.output_min.token.chain, ChainId::OPTIMISM);
        assert_eq!(intent.input.amount, U256::from(0x05f515e1u64));
        assert_eq!(intent.output_min.amount, U256::from(0x05f4103cu64));
        assert!(
            intent.input.amount > intent.output_min.amount,
            "input exceeds min output by the relayer fee"
        );
        assert_eq!(intent.input.decimals, 6); // USDT, 6dp via shared resolver
        assert_eq!(intent.output_min.decimals, 6);

        // fillDeadline (0x6a37cb4f) → the intent deadline.
        assert_eq!(intent.deadline.timestamp(), 0x6a37cb4f);
    }

    /// LIV-6: the exclusivity-**present** branch of `normalize_deposit`. The
    /// clean real fixture has a zero `exclusiveRelayer`; here we synthesize the
    /// exclusivity case by setting the two words the fixture left empty —
    /// `exclusivityDeadline` (data word 6) and `exclusiveRelayer` (word 8),
    /// with a real Across exclusive-relayer address. A clearly synthetic variant
    /// of the real log (NOT presented as on-chain), exercising the same branch
    /// the historical exclusive-fill test covers for the fill side.
    #[test]
    fn normalize_deposit_exclusivity_branch() {
        let mut log = real_deposit_log();
        let mut data = alloy_primitives::hex::decode(log.data.trim()).unwrap();
        // Non-indexed word layout: 0 inputToken, 1 outputToken, 2 inputAmount,
        // 3 outputAmount, 4 quoteTimestamp, 5 fillDeadline, 6 exclusivityDeadline,
        // 7 recipient, 8 exclusiveRelayer, 9 msg-offset, 10 msg-len.
        let set_tail = |data: &mut [u8], word: usize, val: &[u8]| {
            let end = (word + 1) * 32;
            data[end - val.len()..end].copy_from_slice(val);
        };
        // exclusivityDeadline between quoteTimestamp and fillDeadline.
        set_tail(&mut data, 6, &0x6a37_b000u32.to_be_bytes());
        let relayer =
            alloy_primitives::hex::decode("15652636f3898f550b257b89926d5566821c32e1").unwrap();
        set_tail(&mut data, 8, &relayer); // low 20 bytes of word 8
        log.data = format!("0x{}", alloy_primitives::hex::encode(&data));

        let intent = normalize_deposit(ChainId::ARBITRUM, &log).unwrap();
        let excl = intent.exclusivity.expect("nonzero exclusiveRelayer → window");
        assert_eq!(
            excl.exclusive_relayer,
            "0x15652636f3898f550b257b89926d5566821c32e1".parse::<Address>().unwrap()
        );
        assert_eq!(excl.deadline.timestamp(), 0x6a37_b000);
    }
}
