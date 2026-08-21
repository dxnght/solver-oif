//! `solver live` — Phase-1 live shadow loop (zero transactions).
//!
//! Polls real-time-ish intents via one or more [`LiveSource`]s (Across
//! deposits, OIF `Open` orders — OIF-2), merges them into one stream, runs
//! the **same** shared decision as the backtest ([`RouterV0::decide`] →
//! `would_fill` + reason, the single [`FillPolicy`]), and appends one
//! [`DecisionRecord`] per intent to `reports/live/decisions-<srcs>-<day>.jsonl`
//! (`<srcs>` names every source that ended up genuinely open, e.g.
//! `across+oif`). No bids, no fills — we only *measure* what we would have
//! decided.
//!
//! Legibility (CLAUDE.md rule #7): each source announces its own open/idle
//! chain inventory at startup; a periodic heartbeat reports throughput; a
//! source with nothing configured (no `ACROSS_RPC_<CHAIN>`) is a loud
//! per-source `WARN`, not silence; if *every* requested source ends up idle,
//! the whole run returns loudly rather than sitting open with nothing to
//! show. Prices/gas are static here (live feeds are a later bucket) — same as
//! the backtest, so the decision is comparable.

use std::path::{Path, PathBuf};
use std::time::Duration as StdDuration;

use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use solver_ingest::{AcrossLive, JsonlSink, LiveSource, OifLive, SourceKind};
use solver_pricing::GasSource;
use solver_router::{Decision, PriceSource, RouterV0};
use solver_types::{ChainId, FillPolicy, NormalizedIntent};
use tokio::sync::mpsc;
use tracing::{info, warn};

/// Default output directory for live artifacts.
pub const LIVE_OUT_DIR: &str = "reports/live";

/// How often to emit a throughput heartbeat.
const HEARTBEAT_EVERY: StdDuration = StdDuration::from_secs(60);

/// Depth of the channel every per-source receiver is forwarded into. Each
/// `LiveSource` already buffers internally at a similar size (see
/// `solver_ingest::live`/`oif::live`); this only needs to absorb the brief
/// window between a source's own channel yielding an intent and this loop's
/// `select!` picking it up.
const MERGED_BUFFER: usize = 256;

/// One shadow decision, appended to `decisions-<src>-<day>.jsonl`.
///
/// Additive, `Serialize`+`Deserialize` (durable JSONL — add fields, never
/// rename). `retrospective: false` marks this as a real-time decision (a
/// backtest replay over the same schema would set it `true`). The quote-derived
/// numbers are `Option` because a **skipped** intent has no quote — we record
/// `null` rather than a misleading `0.0` (rule #2: don't fabricate).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DecisionRecord {
    pub intent_id: String,
    /// Route key: `origin→dest/inTok→outTok` (same as the backtest).
    pub route: String,
    /// When the intent was observed (origin deposit time / wall clock for live).
    pub observed_at: DateTime<Utc>,
    /// When we evaluated it (the `eval_at` passed to the router).
    pub as_of: DateTime<Utc>,
    pub would_fill: bool,
    /// Stable reason string — a `SkipReason` or `FillReason` (never a new one).
    pub reason: String,
    pub expected_profit_usd: Option<f64>,
    pub confidence: Option<f64>,
    pub gas_usd: Option<f64>,
    /// Which model produced `gas_usd` — `etherscan`/`feehistory`/`static`. Lets
    /// a reader tell a live gas reading from a static fallback. `#[serde(default)]`
    /// so records written before this field existed still deserialize.
    #[serde(default)]
    pub gas_source: Option<String>,
    /// Clickable explorer link to the backing tx (origin deposit), when known.
    pub verify_url: Option<String>,
    pub retrospective: bool,
}

impl DecisionRecord {
    /// Build from the shared [`Decision`] for a live intent evaluated at `as_of`.
    fn from_decision(intent: &NormalizedIntent, as_of: DateTime<Utc>, d: &Decision) -> Self {
        let q = d.quote();
        DecisionRecord {
            intent_id: intent.id.0.clone(),
            route: intent.route_key(),
            observed_at: intent.observed_at,
            as_of,
            would_fill: d.would_fill(),
            reason: d.reason().to_string(),
            expected_profit_usd: q.map(|q| q.expected_profit_usd),
            confidence: q.map(|q| q.confidence),
            gas_usd: q.map(|q| q.gas_usd),
            gas_source: q.map(|q| q.gas_source.as_str().to_string()),
            verify_url: intent.verify_url(),
            retrospective: false,
        }
    }
}

/// Everything about the run except pricing (kept separate — `prices`/`gas`
/// are already their own resolved sources by the time the CLI calls `run`).
/// Bundled into one struct rather than passed positionally so `run` stays
/// under clippy's argument-count lint without an `#[allow]`.
pub struct LiveConfig {
    pub chains: Vec<ChainId>,
    /// Which live sources to poll — `across` and `oif` are both wired
    /// (OIF-2); an unrecognized name is a loud `WARN` + ignore.
    pub sources: Vec<String>,
    /// Bounds the run in seconds (`None` = until interrupted).
    pub duration: Option<u64>,
    /// Decisions and per-chain poll checkpoints land under here.
    pub out_dir: String,
    /// How often each chain is polled (POLL-1 — checkpointed `eth_getLogs`,
    /// not a WS push).
    pub poll_interval: StdDuration,
    /// Delete `decisions-*.jsonl` older than this many calendar days, checked
    /// every heartbeat (`crate::retention`). `0` = keep forever.
    pub retention_days: i64,
    pub dashboard_port: Option<u16>,
}

/// Run the live shadow loop — see [`LiveConfig`] for the run parameters.
pub async fn run(config: LiveConfig, prices: PriceSource, gas: GasSource) -> Result<()> {
    let LiveConfig {
        chains,
        sources,
        duration,
        out_dir,
        poll_interval,
        retention_days,
        dashboard_port,
    } = config;
    if retention_days > 0 && retention_days < crate::retention::MIN_SAFE_RETENTION_DAYS {
        warn!(
            retention_days,
            min_safe = crate::retention::MIN_SAFE_RETENTION_DAYS,
            "retention_days is below the safe floor — the dashboard's 7d aggregate \
             window may be truncated (some records up to 7 days old could already \
             be pruned before the window reads them)"
        );
    }

    // Source selection: one LiveSource per recognized name. An unknown name
    // is announced, not silent (rule #7).
    let mut wanted: Vec<Box<dyn LiveSource>> = Vec::new();
    for s in &sources {
        match s.as_str() {
            "across" => wanted.push(Box::new(AcrossLive::new(chains.clone(), &out_dir, poll_interval))),
            "oif" => wanted.push(Box::new(OifLive::new(chains.clone(), &out_dir, poll_interval))),
            other => warn!(source = %other, "unknown live source — ignored"),
        }
    }
    if wanted.is_empty() {
        warn!("no real-time source requested — idle (known sources: 'across', 'oif')");
        return Ok(());
    }

    // Spawn every requested source (each logs its own open=[…]/idle=[…] chain
    // inventory) and forward all of them into one merged stream — the rest of
    // this loop is source-agnostic, exactly like a single-source run.
    let (merged_tx, mut rx) = mpsc::channel::<NormalizedIntent>(MERGED_BUFFER);
    let mut open_names: Vec<&'static str> = Vec::new();
    for source in &wanted {
        let (kind, mut src_rx) = source.spawn();
        if kind == SourceKind::RealtimeOpen {
            open_names.push(source.name());
        }
        let tx = merged_tx.clone();
        tokio::spawn(async move {
            while let Some(intent) = src_rx.recv().await {
                if tx.send(intent).await.is_err() {
                    break; // merged consumer gone — stop forwarding
                }
            }
        });
    }
    drop(merged_tx); // rx closes once every forwarder above has ended

    if open_names.is_empty() {
        warn!(
            "no real-time-ish open source — idle (set ACROSS_RPC_<CHAIN>, \
             or run `solver doctor`)"
        );
        return Ok(());
    }
    let source_label = open_names.join("+");

    info!(sources = %source_label, out_dir = %out_dir, duration_s = ?duration,
        prices = prices.label(), gas = gas.label(),
        "live shadow started (zero transactions)");

    // Optional live dashboard: serves the SAME decisions over SSE (rule #5 —
    // consume the one stream, never re-derive). Without `--dashboard-port` this
    // stays `None` and the loop behaves exactly as B4.
    let dashboard = match dashboard_port {
        Some(port) => {
            let handle = crate::dashboard::serve(port).await?;
            info!("dashboard: http://127.0.0.1:{}", handle.addr.port());
            // BAGG-1: seed the persisted 24h/7d aggregates from disk BEFORE the
            // loop starts, so a restart's first page load already reflects
            // history instead of resetting to zero while new decisions trickle in.
            let agg = crate::aggregates::compute(Path::new(&out_dir), Utc::now());
            handle.set_aggregates(agg);
            Some(handle)
        }
        None => None,
    };

    let router = RouterV0::new(prices, gas);
    let policy = FillPolicy::default();

    // Lazy sink — no confusing 0-byte file if the window is quiet (rule #7).
    // `sink_day` is the UTC day (`%Y%m%d`) the open sink's file is stamped
    // with; reopened onto a fresh path whenever the day advances (LIV-ROLL).
    // A sink opened once and reused for the process's whole lifetime would
    // keep writing under its FIRST day's filename forever — `retention.rs`
    // deletes by the day parsed FROM the filename, so a long-running process
    // (this is a 24/7 systemd service) would eventually have its own
    // still-open, actively-written file pruned out from under it once that
    // stale name aged past the retention window (rule #7: this was silent).
    let mut sink: Option<JsonlSink> = None;
    let mut sink_day: Option<String> = None;
    let mut sink_path: Option<PathBuf> = None;
    let mut total = 0usize;
    let mut last_heartbeat = 0usize;

    // --duration bound as a single timer; `pending` when unbounded.
    let stop = async {
        match duration {
            Some(s) => tokio::time::sleep(StdDuration::from_secs(s)).await,
            None => std::future::pending::<()>().await,
        }
    };
    tokio::pin!(stop);
    let mut heartbeat = tokio::time::interval(HEARTBEAT_EVERY);

    loop {
        tokio::select! {
            _ = &mut stop => {
                info!(duration_s = ?duration, "live duration elapsed — stopping");
                break;
            }
            _ = heartbeat.tick() => {
                info!(intents = total, since_last = total - last_heartbeat, "live heartbeat");
                last_heartbeat = total;
                // BAGG-1: refresh the persisted aggregates from disk on the same
                // cadence as the heartbeat — cheap at this scale (rule #5: no
                // premature incremental-eviction machinery) and keeps `/aggregates`
                // from drifting stale for long between restarts.
                if let Some(d) = &dashboard {
                    let agg = crate::aggregates::compute(Path::new(&out_dir), Utc::now());
                    d.set_aggregates(agg);
                }
                // Retention: same cadence, cheap directory scan (a handful of
                // files at this scale) — deletes nothing unless a file has
                // actually aged out (rule #7: loud only when something happens).
                let pruned = crate::retention::prune_old_decisions(
                    Path::new(&out_dir), Utc::now().date_naive(), retention_days,
                );
                if !pruned.is_empty() {
                    info!(count = pruned.len(), retention_days, "retention: pruned old decisions files");
                }
            }
            maybe = rx.recv() => match maybe {
                None => {
                    info!("live source stream closed — stopping");
                    break;
                }
                Some(intent) => {
                    let as_of = Utc::now();
                    let decision = router.decide(&intent, as_of, &policy).await;
                    let record = DecisionRecord::from_decision(&intent, as_of, &decision);
                    info!(
                        id = %record.intent_id, route = %record.route,
                        would_fill = record.would_fill, reason = %record.reason,
                        profit_usd = ?record.expected_profit_usd, "shadow decision"
                    );
                    // Fan out to the dashboard (if any) in ADDITION to the
                    // durable JSONL write — never instead of it.
                    if let Some(d) = &dashboard {
                        d.publish(&record);
                    }
                    // Roll to a fresh file the instant the UTC day advances —
                    // see the `sink_day` doc comment above for why a sink
                    // opened once and reused forever is unsafe here.
                    let today = as_of.format("%Y%m%d").to_string();
                    if sink_day.as_deref() != Some(today.as_str()) {
                        if let Some(mut old) = sink.take() {
                            old.flush_silent().await?;
                        }
                        let new_path = decisions_path(&out_dir, &source_label, &today);
                        info!(out = %new_path.display(), "live: opening decisions file for new day");
                        sink = Some(JsonlSink::open(&new_path).await?);
                        sink_path = Some(new_path);
                        sink_day = Some(today);
                    }
                    let s = sink.as_mut().expect("sink opened");
                    s.write(&record).await?;
                    s.flush_silent().await?; // durable per decision (low live rate)
                    total += 1;
                }
            }
        }
    }

    match sink {
        Some(mut s) => {
            s.flush_silent().await?;
            let out = sink_path.map(|p| p.display().to_string()).unwrap_or_default();
            info!(sources = %source_label, decisions = total, out = %out, "live shadow done");
        }
        None => info!(sources = %source_label, "live shadow done — no intents observed"),
    }
    Ok(())
}

/// The day-stamped decisions file path for `source_label` under `out_dir`,
/// e.g. `("reports/live", "across", "20260821")` →
/// `reports/live/decisions-across-20260821.jsonl`. A free function so the
/// rotation this is built for (a new day → a new path) is unit-testable
/// without a live sink or the wall clock.
fn decisions_path(out_dir: &str, source_label: &str, day: &str) -> PathBuf {
    Path::new(out_dir).join(format!("decisions-{source_label}-{day}.jsonl"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{address, Address, U256};
    use chrono::Duration;
    use solver_pricing::StaticGasModel;
    use solver_router::{RouterV0, StaticPriceFeed};
    use solver_types::{AssetAmount, IntentId, SettlementType, Source, TokenRef, TxRef};

    const BASE_USDC: Address = address!("833589fCD6eDb6E08f4c7C32D4f71b54bdA02913");
    const ARB_USDC: Address = address!("af88d065e77c8cC2239327C5EDb3A432268e5831");
    // 64-hex tx, set on the ORIGIN chain (deposit) like a live Across intent.
    const TX: &str = "0xabc0000000000000000000000000000000000000000000000000000000000001";

    /// `decisions_path` shape: `<out_dir>/decisions-<source_label>-<day>.jsonl`.
    #[test]
    fn decisions_path_is_day_stamped() {
        let p = decisions_path("reports/live", "across", "20260821");
        assert_eq!(p, Path::new("reports/live/decisions-across-20260821.jsonl"));
    }

    /// LIV-ROLL regression: two different days must produce two different
    /// paths for the same source — this is exactly the rotation the live
    /// loop's `sink_day != today` check relies on to reopen onto a new file
    /// instead of writing forever under the day it started on.
    #[test]
    fn decisions_path_differs_across_days() {
        let a = decisions_path("reports/live", "across", "20260820");
        let b = decisions_path("reports/live", "across", "20260821");
        assert_ne!(a, b, "a day rollover must produce a different decisions-file path");
    }

    /// A stable Base→Arbitrum intent (origin tx_ref), the shape a live Across
    /// deposit takes once normalized. `out_units < in_units` → fillable.
    fn stable_intent(id: &str, in_units: u64, out_units: u64) -> NormalizedIntent {
        let now = Utc::now();
        let in_tok = TokenRef::new(ChainId::BASE, BASE_USDC);
        let out_tok = TokenRef::new(ChainId::ARBITRUM, ARB_USDC);
        NormalizedIntent {
            id: IntentId(id.into()),
            source: Source::Across,
            origin_chain: ChainId::BASE,
            dest_chain: ChainId::ARBITRUM,
            input: AssetAmount::new(in_tok, U256::from(in_units), 6),
            output_min: AssetAmount::new(out_tok, U256::from(out_units), 6),
            output_decay: None,
            deadline: now + Duration::minutes(5),
            settlement: SettlementType::OptimisticOracle {
                challenge_window: Duration::seconds(1800),
            },
            observed_at: now,
            winning_filler: None,
            realized_output: None,
            exclusivity: None,
            tx_ref: Some(TxRef { chain: ChainId::BASE, tx_hash: TX.into() }),
            raw: serde_json::Value::Null,
        }
    }

    /// FP-1 proof-by-execution: drive the SAME intent through both real code
    /// paths — `backtest::compute` and the live `decide` + `DecisionRecord` —
    /// and assert identical `would_fill` AND `reason`. Catches a regression
    /// where either path stops using the shared `decide()` (the old live
    /// `>$0.50` vs backtest `>$0` split). One fillable intent, one underwater.
    #[tokio::test]
    async fn live_decision_matches_backtest() {
        let policy = FillPolicy::default();
        let cases = [
            stable_intent("across:base:arbitrum:1", 1_000_000_000, 990_000_000), // fill
            stable_intent("across:base:arbitrum:2", 1_000_000_000, 1_005_000_000), // underwater
        ];
        for intent in &cases {
            // Backtest path (its real aggregator).
            let bt = solver_backtest::compute(
                std::slice::from_ref(intent),
                &StaticPriceFeed::reference(),
                StaticGasModel,
                policy,
            )
            .await;
            let bt_would_fill = bt.metrics.would_fill == 1;
            let bt_reason = bt
                .metrics
                .fill_reasons
                .keys()
                .next()
                .or_else(|| bt.metrics.skipped.keys().next())
                .expect("exactly one decision for one intent")
                .clone();

            // Live path (same eval_at; static prices/gas → deterministic).
            let router = RouterV0::new(StaticPriceFeed::reference(), StaticGasModel);
            let eval_at = intent.observed_at;
            let decision = router.decide(intent, eval_at, &policy).await;
            let rec = DecisionRecord::from_decision(intent, eval_at, &decision);

            assert_eq!(rec.would_fill, bt_would_fill, "would_fill diverged for {}", rec.intent_id);
            assert_eq!(rec.reason, bt_reason, "reason diverged for {}", rec.intent_id);
        }
    }

    /// LIV-6 / exit-criteria: a DecisionRecord row carries the durable shape the
    /// decisions JSONL promises — `retrospective:false`, a `verify_url` on the
    /// backing (origin) tx, a stable `reason` string, quoted ⇒ numbers present —
    /// and round-trips through serde (additive JSONL, add fields never rename).
    #[tokio::test]
    async fn decision_record_shape_and_roundtrip() {
        let policy = FillPolicy::default();
        let intent = stable_intent("across:base:arbitrum:9", 1_000_000_000, 990_000_000);
        let router = RouterV0::new(StaticPriceFeed::reference(), StaticGasModel);
        let eval_at = intent.observed_at;
        let decision = router.decide(&intent, eval_at, &policy).await;
        let rec = DecisionRecord::from_decision(&intent, eval_at, &decision);

        assert!(!rec.retrospective, "live rows are real-time, not retrospective");
        // verify_url points at the backing tx on its chain (origin deposit).
        assert_eq!(rec.verify_url, Some(format!("https://basescan.org/tx/{TX}")));
        // reason is one of the stable Skip/FillReason strings — never a new one.
        const STABLE: &[&str] = &[
            "fill", "suspect", "unprofitable", "below_margin", "price_unknown", "expired",
            "exclusive_to_other", "dust_amount", "unsupported_chain",
        ];
        assert!(STABLE.contains(&rec.reason.as_str()), "non-stable reason {}", rec.reason);
        // This stable route quotes → all three quote numbers present together.
        assert!(
            rec.expected_profit_usd.is_some()
                && rec.confidence.is_some()
                && rec.gas_usd.is_some(),
            "quoted ⇒ numbers present"
        );
        // GAS-1: the gas source tag rides into the record. This router uses the
        // static gas model, so the quoted row is tagged "static".
        assert_eq!(rec.gas_source.as_deref(), Some("static"), "quoted ⇒ gas_source present");

        // Durable JSONL: serialize → deserialize → identical key fields.
        let line = serde_json::to_string(&rec).unwrap();
        let back: DecisionRecord = serde_json::from_str(&line).unwrap();
        assert_eq!(back.intent_id, rec.intent_id);
        assert_eq!(back.would_fill, rec.would_fill);
        assert_eq!(back.reason, rec.reason);
        assert_eq!(back.retrospective, rec.retrospective);
        assert_eq!(back.verify_url, rec.verify_url);
        assert_eq!(back.expected_profit_usd, rec.expected_profit_usd);
        assert_eq!(back.gas_source, rec.gas_source);
    }
}
