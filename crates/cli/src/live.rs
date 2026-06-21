//! `solver live` — Phase-1 live shadow loop (zero transactions).
//!
//! Subscribes to real-time intents via a [`LiveSource`] (Across deposits today),
//! runs the **same** shared decision as the backtest ([`RouterV0::decide`] →
//! `would_fill` + reason, the single [`FillPolicy`]), and appends one
//! [`DecisionRecord`] per intent to `reports/live/decisions-<src>-<day>.jsonl`.
//! No bids, no fills — we only *measure* what we would have decided.
//!
//! Legibility (CLAUDE.md rule #7): the source announces its open/idle chain
//! inventory at startup; a periodic heartbeat reports throughput; an idle source
//! (no `ACROSS_WS_<CHAIN>` configured) returns loudly instead of hanging in
//! silence. Prices/gas are static here (live feeds are a later bucket) — same as
//! the backtest, so the decision is comparable.

use std::path::Path;
use std::time::Duration as StdDuration;

use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use solver_ingest::{AcrossLive, JsonlSink, LiveSource, SourceKind};
use solver_pricing::StaticGasModel;
use solver_router::{Decision, RouterV0, StaticPriceFeed};
use solver_types::{ChainId, FillPolicy, NormalizedIntent};
use tracing::{info, warn};

/// Default output directory for live artifacts.
pub const LIVE_OUT_DIR: &str = "reports/live";

/// How often to emit a throughput heartbeat.
const HEARTBEAT_EVERY: StdDuration = StdDuration::from_secs(60);

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
            verify_url: intent.verify_url(),
            retrospective: false,
        }
    }
}

/// Run the live shadow loop. `sources` selects which live sources to subscribe
/// to (only `across` is wired; `oif` stays idle/unverified). `duration` bounds
/// the run in seconds (None = until interrupted). Decisions land under `out_dir`.
pub async fn run(
    chains: Vec<ChainId>,
    sources: Vec<String>,
    duration: Option<u64>,
    out_dir: &str,
) -> Result<()> {
    // Source selection. Only Across is a real-time source; OIF is unverified
    // (rule #2) and anything else is unknown — both are announced, not silent.
    let mut want_across = false;
    for s in &sources {
        match s.as_str() {
            "across" => want_across = true,
            "oif" => warn!(
                source = "oif",
                "IDLE — settler address UNVERIFIED (rule #2); not subscribing"
            ),
            other => warn!(source = %other, "unknown live source — ignored"),
        }
    }
    if !want_across {
        warn!("no real-time open source requested — idle (only 'across' is wired)");
        return Ok(());
    }

    let source = AcrossLive::new(chains);
    let (kind, mut rx) = source.spawn(); // logs the open=[…]/idle=[…] inventory
    if kind == SourceKind::Idle {
        warn!(
            source = source.name(),
            "no real-time open source — idle (set ACROSS_WS_<CHAIN> to subscribe)"
        );
        return Ok(());
    }

    let path = Path::new(out_dir).join(format!(
        "decisions-{}-{}.jsonl",
        source.name(),
        Utc::now().format("%Y%m%d")
    ));
    info!(source = source.name(), out = %path.display(), duration_s = ?duration,
        "live shadow started (zero transactions)");

    let router = RouterV0::new(StaticPriceFeed::reference(), StaticGasModel);
    let policy = FillPolicy::default();

    // Lazy sink — no confusing 0-byte file if the window is quiet (rule #7).
    let mut sink: Option<JsonlSink> = None;
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
                    if sink.is_none() {
                        sink = Some(JsonlSink::open(&path).await?);
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
            info!(source = source.name(), decisions = total, out = %path.display(),
                "live shadow done");
        }
        None => info!(source = source.name(), "live shadow done — no intents observed"),
    }
    Ok(())
}
