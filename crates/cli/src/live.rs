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
use solver_pricing::GasSource;
use solver_router::{Decision, PriceSource, RouterV0};
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

/// Run the live shadow loop. `sources` selects which live sources to subscribe
/// to (only `across` is wired; `oif` stays idle/unverified). `duration` bounds
/// the run in seconds (None = until interrupted). Decisions land under `out_dir`.
pub async fn run(
    chains: Vec<ChainId>,
    sources: Vec<String>,
    duration: Option<u64>,
    out_dir: &str,
    dashboard_port: Option<u16>,
    prices: PriceSource,
    gas: GasSource,
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
        prices = prices.label(), gas = gas.label(),
        "live shadow started (zero transactions)");

    // Optional live dashboard: serves the SAME decisions over SSE (rule #5 —
    // consume the one stream, never re-derive). Without `--dashboard-port` this
    // stays `None` and the loop behaves exactly as B4.
    let dashboard = match dashboard_port {
        Some(port) => {
            let handle = crate::dashboard::serve(port).await?;
            info!("dashboard: http://127.0.0.1:{}", handle.addr.port());
            Some(handle)
        }
        None => None,
    };

    let router = RouterV0::new(prices, gas);
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
                    // Fan out to the dashboard (if any) in ADDITION to the
                    // durable JSONL write — never instead of it.
                    if let Some(d) = &dashboard {
                        d.publish(&record);
                    }
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
