//! BAGG-1: persistent 24h/7d flow aggregates, re-derived from the durable
//! decisions JSONL rather than kept only in memory — so a service restart (or
//! a redeploy) never resets the headline dashboard numbers back to zero
//! (CLAUDE.md rule #9: the JSONL is ground truth, not the in-memory replay
//! buffer `dashboard.rs` keeps for "recent flow").
//!
//! Recomputed wholesale from disk on every refresh (once at `live` startup,
//! then every heartbeat) rather than maintained incrementally: the dataset is
//! niche-scale (rule #5 — no premature abstraction), and a full recompute
//! can't drift from the source of truth the way an incremental add/evict
//! counter could.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Duration, Utc};
use serde::Serialize;
use tracing::debug;

use crate::live::DecisionRecord;

/// Tally for one lookback window.
#[derive(Debug, Clone, Default, Serialize)]
pub struct WindowAgg {
    pub total: u64,
    pub would_fill: u64,
    pub reasons: HashMap<String, u64>,
    pub routes: HashMap<String, u64>,
    pub dest_chains: HashMap<String, u64>,
}

impl WindowAgg {
    fn bump(&mut self, rec: &DecisionRecord) {
        self.total += 1;
        if rec.would_fill {
            self.would_fill += 1;
        }
        *self.reasons.entry(rec.reason.clone()).or_insert(0) += 1;
        let (pair, dest) = route_pair_dest(&rec.route);
        *self.routes.entry(pair).or_insert(0) += 1;
        *self.dest_chains.entry(dest).or_insert(0) += 1;
    }
}

/// Both windows the dashboard offers, plus when they were computed (lets a
/// reader tell a fresh read from a stale one if a refresh ever stalls).
#[derive(Debug, Clone, Serialize)]
pub struct Aggregates {
    pub computed_at: DateTime<Utc>,
    pub last_24h: WindowAgg,
    pub last_7d: WindowAgg,
}

impl Aggregates {
    fn empty(now: DateTime<Utc>) -> Self {
        Aggregates { computed_at: now, last_24h: WindowAgg::default(), last_7d: WindowAgg::default() }
    }
}

/// Mirrors the JS `parseRoute` in `dashboard.html`: `"base->arbitrum/833589->af88d0"`
/// → `("base → arbitrum", "arbitrum")`. Kept in lockstep deliberately — same
/// route key, same pair label — so the persisted and live-session numbers
/// never disagree on formatting for the same underlying route.
fn route_pair_dest(route: &str) -> (String, String) {
    let chain_seg = route.split('/').next().unwrap_or(route);
    let mut parts = chain_seg.split("->");
    let origin = parts.next().unwrap_or("?");
    let dest = parts.next().unwrap_or("?");
    (format!("{origin} → {dest}"), dest.to_string())
}

/// Collect `decisions-*.jsonl` files under `dir` — the live loop's durable
/// output (`live.rs`). A missing dir is not an error: it means nothing has
/// been written yet, so the caller gets empty windows, not a crash.
fn find_decision_files(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return out,
    };
    for entry in entries.flatten() {
        let p = entry.path();
        let name = match p.file_name().and_then(|n| n.to_str()) {
            Some(n) => n,
            None => continue,
        };
        if name.starts_with("decisions-") && name.ends_with(".jsonl") {
            out.push(p);
        }
    }
    out.sort();
    out
}

/// Recompute both windows from every `decisions-*.jsonl` under `dir`, as of
/// `now`. Malformed lines are skipped (older schema drift, a half-flushed
/// write) — never fatal, since this only feeds a dashboard readout, not a
/// decision.
pub fn compute(dir: &Path, now: DateTime<Utc>) -> Aggregates {
    let mut agg = Aggregates::empty(now);
    let day_ago = now - Duration::hours(24);
    let week_ago = now - Duration::days(7);

    for path in find_decision_files(dir) {
        let content = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(e) => {
                debug!(file = %path.display(), error = %e, "aggregates: skipping unreadable file");
                continue;
            }
        };
        for line in content.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let rec: DecisionRecord = match serde_json::from_str(line) {
                Ok(r) => r,
                Err(_) => continue,
            };
            if rec.observed_at >= week_ago {
                agg.last_7d.bump(&rec);
            }
            if rec.observed_at >= day_ago {
                agg.last_24h.bump(&rec);
            }
        }
    }
    agg
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn unique_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "solver-agg-{tag}-{}-{}",
            std::process::id(),
            Utc::now().timestamp_nanos_opt().unwrap_or_default()
        ));
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn rec(id: &str, observed_at: DateTime<Utc>, would_fill: bool, reason: &str, route: &str) -> DecisionRecord {
        DecisionRecord {
            intent_id: id.into(),
            route: route.into(),
            observed_at,
            as_of: observed_at,
            would_fill,
            reason: reason.into(),
            expected_profit_usd: None,
            confidence: None,
            gas_usd: None,
            gas_source: None,
            verify_url: None,
            retrospective: false,
        }
    }

    /// A record inside 24h counts in both windows; one older than 24h but
    /// within 7d counts only in the weekly window; one older than 7d counts
    /// in neither. This is BAGG-1's exit criterion in miniature: the numbers
    /// come from files on disk, not from anything held only in memory.
    #[test]
    fn windows_bucket_by_age() {
        let now = Utc::now();
        let dir = unique_dir("bucket");
        let lines = [
            rec("a", now - Duration::hours(1), true, "fill", "base->arbitrum/x->y"),
            rec("b", now - Duration::hours(30), false, "exclusive_to_other", "base->arbitrum/x->y"),
            rec("c", now - Duration::days(10), false, "exclusive_to_other", "base->arbitrum/x->y"),
        ];
        let body = lines.iter().map(|r| serde_json::to_string(r).unwrap()).collect::<Vec<_>>().join("\n");
        fs::write(dir.join("decisions-across-20260101.jsonl"), body).unwrap();

        let agg = compute(&dir, now);
        assert_eq!(agg.last_24h.total, 1, "only the 1h-old record is within 24h");
        assert_eq!(agg.last_24h.would_fill, 1);
        assert_eq!(agg.last_7d.total, 2, "1h and 30h old are both within 7d; 10d old is not");
        assert_eq!(agg.last_7d.reasons.get("exclusive_to_other").copied(), Some(1));
    }

    /// A missing directory (nothing ingested yet) is empty windows, not a
    /// panic — legibility (rule #7): idle should read as "0", not crash.
    #[test]
    fn missing_dir_is_empty_not_error() {
        let agg = compute(Path::new("/does/not/exist/solver-agg-missing"), Utc::now());
        assert_eq!(agg.last_24h.total, 0);
        assert_eq!(agg.last_7d.total, 0);
    }

    /// A malformed line is skipped, not fatal (mirrors `backtest::load` hygiene).
    #[test]
    fn malformed_line_is_skipped() {
        let now = Utc::now();
        let dir = unique_dir("malformed");
        let good = rec("a", now, true, "fill", "base->arbitrum/x->y");
        fs::write(
            dir.join("decisions-across-20260101.jsonl"),
            format!("{{not json}}\n{}\n", serde_json::to_string(&good).unwrap()),
        )
        .unwrap();

        let agg = compute(&dir, now);
        assert_eq!(agg.last_24h.total, 1);
    }

    /// Two routes to the same dest chain from different origins aggregate the
    /// dest-chain tally across both, but keep the route (origin→dest) tally
    /// distinct — the concentration panel needs both cuts.
    #[test]
    fn dest_chain_tally_merges_across_routes() {
        let now = Utc::now();
        let dir = unique_dir("dest");
        let lines = [
            rec("a", now, false, "exclusive_to_other", "base->arbitrum/x->y"),
            rec("b", now, false, "exclusive_to_other", "optimism->arbitrum/x->y"),
        ];
        let body = lines.iter().map(|r| serde_json::to_string(r).unwrap()).collect::<Vec<_>>().join("\n");
        fs::write(dir.join("decisions-across-20260101.jsonl"), body).unwrap();

        let agg = compute(&dir, now);
        assert_eq!(agg.last_24h.dest_chains.get("arbitrum").copied(), Some(2));
        assert_eq!(agg.last_24h.routes.get("base → arbitrum").copied(), Some(1));
        assert_eq!(agg.last_24h.routes.get("optimism → arbitrum").copied(), Some(1));
    }
}
