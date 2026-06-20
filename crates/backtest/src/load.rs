//! Load + hygiene: read append-only JSONL into a clean, de-duplicated set of
//! [`NormalizedIntent`]s before any measurement (CLAUDE.md rule #9).
//!
//! Two hygiene rules, applied here and nowhere else so the rest of the
//! pipeline can trust its input:
//!   1. **Dedup by `IntentId`** — the same order can appear in overlapping
//!      ingest runs (append-only files); first occurrence wins.
//!   2. **Drop unknown-chain rows** — an intent whose origin or dest chain has
//!      no `name()` mapping can't be priced or linked; counting it would
//!      distort the denominators.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use solver_types::NormalizedIntent;

/// What load saw — surfaced in the report so the numbers are auditable
/// (every raw line is accounted for: kept + malformed + duplicates + dropped).
#[derive(Debug, Default, Clone)]
pub struct LoadStats {
    pub files: usize,
    pub raw_lines: usize,
    pub malformed: usize,
    pub duplicates: usize,
    pub dropped_unknown_chain: usize,
    pub kept: usize,
}

/// Recursively collect `*.jsonl` files under `dir` (sorted for determinism).
/// A missing dir is not an error — it yields an empty list so the caller can
/// report "0 intents" loudly rather than crash.
pub fn find_jsonl(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    if !dir.exists() {
        return Ok(out);
    }
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for entry in fs::read_dir(&d).with_context(|| format!("reading dir {}", d.display()))? {
            let p = entry?.path();
            if p.is_dir() {
                stack.push(p);
            } else if p.extension().is_some_and(|e| e == "jsonl") {
                out.push(p);
            }
        }
    }
    out.sort();
    Ok(out)
}

/// Load every intent under `data_dir`, applying the two hygiene rules.
pub fn load_intents(data_dir: &Path) -> Result<(Vec<NormalizedIntent>, LoadStats)> {
    let files = find_jsonl(data_dir)?;
    let mut stats = LoadStats { files: files.len(), ..Default::default() };
    let mut seen: HashSet<String> = HashSet::new();
    let mut kept: Vec<NormalizedIntent> = Vec::new();

    for f in &files {
        let content = fs::read_to_string(f).with_context(|| format!("reading {}", f.display()))?;
        for line in content.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            stats.raw_lines += 1;
            let intent: NormalizedIntent = match serde_json::from_str(line) {
                Ok(i) => i,
                Err(_) => {
                    stats.malformed += 1;
                    continue;
                }
            };
            if !seen.insert(intent.id.0.clone()) {
                stats.duplicates += 1;
                continue;
            }
            if intent.origin_chain.name() == "unknown" || intent.dest_chain.name() == "unknown" {
                stats.dropped_unknown_chain += 1;
                continue;
            }
            kept.push(intent);
        }
    }
    stats.kept = kept.len();
    Ok((kept, stats))
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{Address, U256};
    use chrono::Utc;
    use solver_types::{AssetAmount, ChainId, IntentId, NormalizedIntent, SettlementType, Source, TokenRef};

    fn intent(id: &str, origin: ChainId, dest: ChainId) -> NormalizedIntent {
        let t = TokenRef::new(origin, Address::repeat_byte(0x11));
        NormalizedIntent {
            id: IntentId(id.into()),
            source: Source::UniswapX,
            origin_chain: origin,
            dest_chain: dest,
            input: AssetAmount::new(t, U256::from(1_000_000u64), 6),
            output_min: AssetAmount::new(TokenRef::new(dest, Address::repeat_byte(0x11)), U256::from(999_000u64), 6),
            output_decay: None,
            deadline: Utc::now(),
            settlement: SettlementType::OffchainAttestation,
            observed_at: Utc::now(),
            winning_filler: None,
            realized_output: None,
            exclusivity: None,
            tx_ref: None,
            raw: serde_json::Value::Null,
        }
    }

    fn unique_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "solver-bt-{tag}-{}-{}",
            std::process::id(),
            Utc::now().timestamp_nanos_opt().unwrap_or_default()
        ));
        fs::create_dir_all(&d).unwrap();
        d
    }

    /// Dedup + chain hygiene against an independently-counted fixture: 4 lines
    /// across 2 files where one id repeats and one row routes to an unknown
    /// chain. Expect kept = 2, duplicates = 1, dropped = 1 — not a readback of
    /// the writer's own output.
    #[test]
    fn dedup_and_chain_hygiene() {
        let dir = unique_dir("hygiene");
        let a = intent("uniswapx:1", ChainId::BASE, ChainId::ARBITRUM);
        let a_dup = intent("uniswapx:1", ChainId::BASE, ChainId::ARBITRUM); // same id
        let b = intent("uniswapx:2", ChainId::BASE, ChainId::OPTIMISM);
        let bad = intent("uniswapx:3", ChainId::BASE, ChainId(987_654_321)); // unknown dest

        let f1 = dir.join("uniswapx-a.jsonl");
        let f2 = dir.join("uniswapx-b.jsonl");
        fs::write(&f1, format!("{}\n{}\n", serde_json::to_string(&a).unwrap(), serde_json::to_string(&a_dup).unwrap())).unwrap();
        fs::write(&f2, format!("{}\n\n{}\n", serde_json::to_string(&b).unwrap(), serde_json::to_string(&bad).unwrap())).unwrap();

        let (intents, stats) = load_intents(&dir).unwrap();
        assert_eq!(stats.files, 2);
        assert_eq!(stats.raw_lines, 4, "blank line must not count");
        assert_eq!(stats.duplicates, 1);
        assert_eq!(stats.dropped_unknown_chain, 1);
        assert_eq!(stats.kept, 2);
        assert_eq!(intents.len(), 2);
        fs::remove_dir_all(&dir).ok();
    }

    /// A malformed line is counted, not fatal — one bad row must not abort the
    /// whole load.
    #[test]
    fn malformed_line_is_counted_not_fatal() {
        let dir = unique_dir("malformed");
        let good = intent("uniswapx:9", ChainId::BASE, ChainId::ARBITRUM);
        fs::write(dir.join("x.jsonl"), format!("{{not json}}\n{}\n", serde_json::to_string(&good).unwrap())).unwrap();
        let (intents, stats) = load_intents(&dir).unwrap();
        assert_eq!(stats.malformed, 1);
        assert_eq!(intents.len(), 1);
        fs::remove_dir_all(&dir).ok();
    }

    /// A missing data dir yields an empty set, not an error (the report then
    /// says "0 intents" loudly).
    #[test]
    fn missing_dir_is_empty_not_error() {
        let (intents, stats) = load_intents(Path::new("/nonexistent/solver/data")).unwrap();
        assert!(intents.is_empty());
        assert_eq!(stats.files, 0);
    }
}
