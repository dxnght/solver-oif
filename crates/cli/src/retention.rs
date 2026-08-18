//! Retention: delete old `decisions-*.jsonl` under `solver live`'s `--out`
//! dir so a long-running (24/7 VPS) service doesn't grow disk usage forever.
//! POLL-1 fixed the RPC-request cost of running unattended; this is the
//! matching fix for disk (small VPS, `BRESIL-1` runs on 1 vCPU/2.5GB/15GB).
//!
//! Files are already day-rotated by the live loop (`decisions-<src>-
//! <YYYYMMDD>.jsonl`, one per UTC calendar day), so retention is whole-file:
//! no partial trimming, no need to touch what's inside a file we keep.
//! `checkpoint-<source>-<chain>.json` (POLL-1's per-chain resume state) is a
//! different filename shape entirely and is never matched — it must survive
//! forever (it is tiny and never grows; it is not what fills the disk).
//!
//! **Interaction with the 7d dashboard window (`aggregates::compute`):**
//! that window reads every `decisions-*.jsonl` under the same dir and keeps
//! whatever `observed_at` falls within the last 7 days — so retention must
//! keep at least 8 calendar days of files (today, plus the 7 before it) or
//! it can delete a file that still holds records inside the 7d window,
//! silently truncating a number the dashboard presents as "7d" (rule #7: a
//! wrong number must never be quiet). [`MIN_SAFE_RETENTION_DAYS`] is that
//! floor; the caller (`live.rs`) warns loudly if configured below it.

use std::path::{Path, PathBuf};

use chrono::{Duration, NaiveDate};
use tracing::warn;

/// Default retention: the 7d dashboard window's minimum safe floor
/// ([`MIN_SAFE_RETENTION_DAYS`]) plus one day of slack.
pub const DEFAULT_RETENTION_DAYS: i64 = 9;

/// The least retention that can never truncate the 7d dashboard window (see
/// module docs): today's file plus the 7 calendar days before it.
pub const MIN_SAFE_RETENTION_DAYS: i64 = 8;

/// Parse the `YYYYMMDD` day out of a `decisions-<label>-<YYYYMMDD>.jsonl`
/// filename. `None` for anything else (a different prefix/suffix, an
/// unparsable date) — the caller then leaves that file alone rather than
/// guessing. Source labels (`across`, `oif`, `across+oif`) never contain
/// `-`, so the day is always the segment after the *last* `-`.
fn file_day(name: &str) -> Option<NaiveDate> {
    let stem = name.strip_prefix("decisions-")?.strip_suffix(".jsonl")?;
    let date_str = stem.rsplit('-').next()?;
    NaiveDate::parse_from_str(date_str, "%Y%m%d").ok()
}

/// Delete every `decisions-*.jsonl` under `dir` whose day is older than
/// `retention_days` calendar days before `today`. `retention_days <= 0`
/// means "keep forever" (a no-op, not an error) — an explicit opt-out, not
/// an accidental one (0 would otherwise read as "delete everything").
/// Returns the paths actually deleted, for the caller to log (never silent —
/// rule #7). A missing `dir`, or a file that fails to delete, is a `warn!`
/// and otherwise ignored — pruning is best-effort housekeeping, not a
/// decision-affecting path.
pub fn prune_old_decisions(dir: &Path, today: NaiveDate, retention_days: i64) -> Vec<PathBuf> {
    let mut pruned = Vec::new();
    if retention_days <= 0 {
        return pruned;
    }
    let cutoff = today - Duration::days(retention_days);

    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return pruned, // nothing written yet — nothing to prune
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else { continue };
        let Some(day) = file_day(name) else { continue }; // not a dated decisions file
        if day < cutoff {
            match std::fs::remove_file(&path) {
                Ok(()) => pruned.push(path),
                Err(e) => warn!(file = %path.display(), error = %e,
                    "retention: failed to delete old decisions file"),
            }
        }
    }
    pruned
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn unique_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "solver-retention-{tag}-{}-{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
        ));
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn file_day_parses_dated_decisions_files() {
        assert_eq!(
            file_day("decisions-across-20260101.jsonl"),
            NaiveDate::from_ymd_opt(2026, 1, 1)
        );
        assert_eq!(
            file_day("decisions-across+oif-20260818.jsonl"),
            NaiveDate::from_ymd_opt(2026, 8, 18)
        );
    }

    /// The checkpoint files (POLL-1) must never be mistaken for a dated
    /// decisions file — wrong prefix AND wrong extension, doubly excluded.
    #[test]
    fn file_day_ignores_checkpoint_and_garbage() {
        assert_eq!(file_day("checkpoint-across-base.json"), None);
        assert_eq!(file_day("decisions-across-notadate.jsonl"), None);
        assert_eq!(file_day("random.txt"), None);
    }

    /// Core contract: a file older than the retention window is deleted, one
    /// within it survives, and the checkpoint file is never touched — proven
    /// against the real filesystem (rule #3: independent truth, not a mock).
    #[test]
    fn prune_deletes_old_keeps_recent_and_checkpoints() {
        let dir = unique_dir("basic");
        let today = NaiveDate::from_ymd_opt(2026, 8, 18).unwrap();
        fs::write(dir.join("decisions-across-20260101.jsonl"), "old").unwrap(); // way old
        fs::write(dir.join("decisions-across-20260817.jsonl"), "recent").unwrap(); // yesterday
        fs::write(dir.join("checkpoint-across-base.json"), r#"{"last_block":1}"#).unwrap();

        let pruned = prune_old_decisions(&dir, today, 9);

        assert_eq!(pruned.len(), 1, "only the 2026-01-01 file is older than 9 days");
        assert!(!dir.join("decisions-across-20260101.jsonl").exists());
        assert!(dir.join("decisions-across-20260817.jsonl").exists(), "within retention");
        assert!(dir.join("checkpoint-across-base.json").exists(), "checkpoint never pruned");
    }

    /// `retention_days <= 0` is an explicit "keep forever" — nothing is
    /// deleted, not even a file from years ago.
    #[test]
    fn zero_or_negative_retention_keeps_everything() {
        let dir = unique_dir("keep-forever");
        let today = NaiveDate::from_ymd_opt(2026, 8, 18).unwrap();
        fs::write(dir.join("decisions-across-20200101.jsonl"), "ancient").unwrap();

        assert_eq!(prune_old_decisions(&dir, today, 0), Vec::<PathBuf>::new());
        assert_eq!(prune_old_decisions(&dir, today, -5), Vec::<PathBuf>::new());
        assert!(dir.join("decisions-across-20200101.jsonl").exists());
    }

    /// A missing directory (nothing written yet) is a no-op, not a panic —
    /// same legibility contract as `aggregates::compute`.
    #[test]
    fn missing_dir_is_a_noop() {
        let today = NaiveDate::from_ymd_opt(2026, 8, 18).unwrap();
        let pruned = prune_old_decisions(Path::new("/does/not/exist/solver-retention"), today, 9);
        assert!(pruned.is_empty());
    }
}
