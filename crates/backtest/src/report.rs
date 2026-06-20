//! Report rendering. Writes `summary.md` (human, with verify-links) and
//! `metrics.json` (machine) into the run's out-dir.
//!
//! The report's job is **honest legibility**: every USD number is labeled
//! MODELED, the would_fill belief is shown next to the realized check, and
//! each fill carries a click-through block-explorer link so a reader can
//! verify the underlying tx themselves (the verify-link discipline — a `None`
//! link is shown as "—", never a fabricated URL).

use std::fmt::Write as _;
use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use serde_json::json;

use crate::load::LoadStats;
use crate::metrics::BacktestOutput;

const MAX_FILL_ROWS: usize = 25;

/// Write `summary.md` + `metrics.json` into `out_dir` (created if absent).
pub fn write_report(out_dir: &Path, load: &LoadStats, out: &BacktestOutput) -> Result<()> {
    fs::create_dir_all(out_dir).with_context(|| format!("creating out-dir {}", out_dir.display()))?;
    fs::write(out_dir.join("summary.md"), render_summary(load, out))
        .with_context(|| format!("writing {}/summary.md", out_dir.display()))?;
    fs::write(out_dir.join("metrics.json"), render_metrics_json(load, out)?)
        .with_context(|| format!("writing {}/metrics.json", out_dir.display()))?;
    Ok(())
}

/// Render the human summary. Pure (string in → string out) so it's unit-tested
/// without touching the filesystem.
pub fn render_summary(load: &LoadStats, out: &BacktestOutput) -> String {
    let m = &out.metrics;
    let mut s = String::new();

    let _ = writeln!(s, "# Backtest — Solver Engine (research artifact)\n");
    let _ = writeln!(
        s,
        "> All USD figures are **MODELED on static reference prices**, not live \
         quotes. `would_fill` is a model belief; it is validated against realized \
         outcomes below and is never presented as realized profit.\n"
    );

    if m.total == 0 {
        let _ = writeln!(
            s,
            "## ⚠ No data\n\nLoad found **0 intents** under the data dir \
             (files scanned: {}). Run the no-key golden path first:\n\n\
             ```\nsolver ingest uniswapx --days 14\nsolver backtest\n```\n",
            load.files
        );
        return s;
    }

    // --- Load ---
    let _ = writeln!(s, "## Load\n");
    let _ = writeln!(s, "- files scanned: {}", load.files);
    let _ = writeln!(s, "- raw lines: {}", load.raw_lines);
    let _ = writeln!(
        s,
        "- kept: **{}** (deduped {}, malformed {}, unknown-chain dropped {})\n",
        load.kept, load.duplicates, load.malformed, load.dropped_unknown_chain
    );

    // --- Decisions ---
    let _ = writeln!(s, "## Decisions\n");
    let _ = writeln!(s, "- intents evaluated: {}", m.total);
    let _ = writeln!(s, "- quoted: {}", m.quoted);
    let _ = writeln!(s, "- would_fill (model belief): **{}**", m.would_fill);
    match m.profitable_quote_rate() {
        Some(r) => {
            let _ = writeln!(
                s,
                "- profitable_quote_rate: {:.1}% ({}/{}) — *not a win rate*",
                r * 100.0,
                m.profitable_quotes,
                m.quoted
            );
        }
        None => {
            let _ = writeln!(s, "- profitable_quote_rate: n/a (nothing quoted)");
        }
    }
    let _ = writeln!(
        s,
        "- modeled profit, sum over would_fill: **${:.2}** [MODELED]\n",
        m.modeled_profit_usd_sum
    );

    if !m.skipped.is_empty() {
        let _ = writeln!(s, "### Skipped (by reason)\n");
        for (reason, n) in &m.skipped {
            let _ = writeln!(s, "- {reason}: {n}");
        }
        let _ = writeln!(s);
    }
    if !m.fill_reasons.is_empty() {
        let _ = writeln!(s, "### Quote outcomes (by FillPolicy reason)\n");
        for (reason, n) in &m.fill_reasons {
            let _ = writeln!(s, "- {reason}: {n}");
        }
        let _ = writeln!(s);
    }

    // --- Calibration ---
    let _ = writeln!(s, "## would_fill calibration (model vs realized)\n");
    let _ = writeln!(s, "- intents with priceable realized data: {}", m.with_realized);
    let _ = writeln!(s, "- would_fill that also have realized data: {}", m.would_fill_with_realized);
    let _ = writeln!(s, "- confirmed (realized gross margin > 0): {}", m.would_fill_confirmed);
    match m.would_fill_precision() {
        Some(p) => {
            let _ = writeln!(
                s,
                "- **would_fill_precision: {:.1}%** — of the fills we can check, the \
                 share the market confirms had margin.\n",
                p * 100.0
            );
        }
        None => {
            let _ = writeln!(
                s,
                "- would_fill_precision: **n/a** — no would_fill intent carried \
                 priceable realized data (don't fabricate a number).\n"
            );
        }
    }

    // --- Verifiable fills ---
    let _ = writeln!(s, "## Verifiable fills (top {} by modeled profit)\n", MAX_FILL_ROWS);
    if out.fills.is_empty() {
        let _ = writeln!(s, "_None — no intent cleared the FillPolicy._\n");
    } else {
        let _ = writeln!(s, "| route | modeled $ | conf | realized $ | verify |");
        let _ = writeln!(s, "|---|--:|--:|--:|---|");
        for r in out.fills.iter().take(MAX_FILL_ROWS) {
            let realized = match r.realized_margin_usd {
                Some(v) => format!("{v:.2}"),
                None => "—".to_string(),
            };
            let verify = match &r.verify_url {
                Some(u) => format!("[tx]({u})"),
                None => "—".to_string(),
            };
            let _ = writeln!(
                s,
                "| {} | {:.2} | {:.2} | {} | {} |",
                r.route_key, r.modeled_profit_usd, r.confidence, realized, verify
            );
        }
        let _ = writeln!(s);
    }

    s
}

fn render_metrics_json(load: &LoadStats, out: &BacktestOutput) -> Result<String> {
    let m = &out.metrics;
    let v = json!({
        "pricing": "static-reference (MODELED, not live)",
        "load": {
            "files": load.files,
            "raw_lines": load.raw_lines,
            "kept": load.kept,
            "duplicates": load.duplicates,
            "malformed": load.malformed,
            "dropped_unknown_chain": load.dropped_unknown_chain,
        },
        "decisions": {
            "total": m.total,
            "quoted": m.quoted,
            "would_fill": m.would_fill,
            "profitable_quotes": m.profitable_quotes,
            "profitable_quote_rate": m.profitable_quote_rate(),
            "modeled_profit_usd_sum": m.modeled_profit_usd_sum,
            "skipped": m.skipped,
            "fill_reasons": m.fill_reasons,
        },
        "calibration": {
            "with_realized": m.with_realized,
            "would_fill_with_realized": m.would_fill_with_realized,
            "would_fill_confirmed": m.would_fill_confirmed,
            "would_fill_precision": m.would_fill_precision(),
        },
    });
    serde_json::to_string_pretty(&v).context("serializing metrics.json")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metrics::{BacktestOutput, FillRow, Metrics};

    fn sample() -> (LoadStats, BacktestOutput) {
        let load = LoadStats { files: 1, raw_lines: 3, kept: 3, duplicates: 0, malformed: 0, dropped_unknown_chain: 0 };
        let mut m = Metrics { total: 3, quoted: 3, would_fill: 1, profitable_quotes: 2, modeled_profit_usd_sum: 4.20, ..Default::default() };
        m.fill_reasons.insert("fill".into(), 1);
        m.fill_reasons.insert("unprofitable".into(), 2);
        m.with_realized = 1;
        m.would_fill_with_realized = 1;
        m.would_fill_confirmed = 0;
        let fills = vec![FillRow {
            id: "uniswapx:a".into(),
            route_key: "base->arbitrum/0x8335…2913->0xaf88…5831".into(),
            modeled_profit_usd: 4.20,
            confidence: 0.9,
            realized_margin_usd: Some(-0.30),
            verify_url: Some("https://arbiscan.io/tx/0xabc".into()),
        }];
        (load, BacktestOutput { metrics: m, fills })
    }

    /// A non-empty run renders the load/decisions/calibration sections, labels
    /// figures MODELED, and emits a clickable verify-link in the fills table.
    /// These are the CORE-6 exit properties — checked on the rendered text.
    #[test]
    fn summary_has_sections_and_verify_link() {
        let (load, out) = sample();
        let md = render_summary(&load, &out);
        assert!(md.contains("MODELED"));
        assert!(md.contains("## Decisions"));
        assert!(md.contains("would_fill calibration"));
        assert!(md.contains("would_fill_precision: 0.0%"));
        assert!(md.contains("[tx](https://arbiscan.io/tx/0xabc)"), "verify link must render");
        assert!(md.contains("not a win rate"));
    }

    /// The empty case is loud and tells the reader exactly how to get data —
    /// no silent empty report (legibility, CLAUDE.md rule #7).
    #[test]
    fn empty_run_is_loud_with_next_step() {
        let load = LoadStats::default();
        let out = BacktestOutput { metrics: Metrics::default(), fills: vec![] };
        let md = render_summary(&load, &out);
        assert!(md.contains("No data"));
        assert!(md.contains("solver ingest uniswapx"));
    }
}
