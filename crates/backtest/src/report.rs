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
/// Worst-drift inspection list size (WFC-7).
const MAX_DRIFT_ROWS: usize = 10;
/// Per-route rows shown before collapsing the rest into a "+N more" line.
const MAX_ROUTE_ROWS: usize = 20;
/// Below this many checkable fills, precision/percentiles are flagged as
/// indicative — a tiny sample doesn't earn a confident rate.
const SMALL_SAMPLE: usize = 30;

/// Render a verify-link table cell: a clickable `[tx](url)` or "—" when there
/// is no link to show. Never a fabricated URL (the verify-link discipline).
fn verify_cell(url: &Option<String>) -> String {
    match url {
        Some(u) => format!("[tx]({u})"),
        None => "—".to_string(),
    }
}

/// A rate cell: a percentage, or "—" when the rate is `None` (uncheckable).
/// We show "—", never a fabricated 0%, so absence reads differently from zero.
fn pct_cell(v: Option<f64>) -> String {
    match v {
        Some(p) => format!("{:.0}%", p * 100.0),
        None => "—".to_string(),
    }
}

/// A money cell: a 2-dp USD figure, or "—" when `None` (not computable).
fn money_cell(v: Option<f64>) -> String {
    match v {
        Some(x) => format!("{x:.2}"),
        None => "—".to_string(),
    }
}

/// Per-route calibration table (WFC-6): the spec's "which routes are stably in
/// the green?" shown per route. Calibration only applies where we'd bid, so the
/// table shows the `would_fill` routes (ordered by intents seen) — not the
/// high-volume routes we always skip, which would otherwise fill the top with a
/// wall of "—" and bury the answer (legibility, rule #7). The full per-route
/// set, sorted by n, is in `metrics.json`. Every USD figure is net of our gas;
/// a would_fill route with no realized data to check still shows "—" there.
fn render_routes(s: &mut String, out: &BacktestOutput) {
    let _ = writeln!(s, "## Per-route calibration (would_fill routes, top {MAX_ROUTE_ROWS} by intents seen)\n");
    let bid: Vec<_> = out.routes.iter().filter(|r| r.would_fill > 0).collect();
    if bid.is_empty() {
        // The honest §3 answer when nothing cleared the policy: "none".
        let _ = writeln!(
            s,
            "_No route produced a would_fill across {} routes observed — by this run \
             there is no route we'd bid (a valid result: none stably in the green)._\n",
            out.routes.len()
        );
        return;
    }
    let _ = writeln!(
        s,
        "`prec(>0)` = any gross margin on the route; `prec(≥m)` = cleared our own \
         min_margin bar. **All USD net of our gas**; `mean_drift` > 0 = we \
         over-modeled. `—` = no realized data on that route to check. {} of {} \
         observed routes produced a would_fill.\n",
        bid.len(),
        out.routes.len()
    );
    let _ = writeln!(
        s,
        "| route | n | would_fill | prec(>0) | prec(≥m) | modeled_net $ | realized_net $ | mean_drift $ |"
    );
    let _ = writeln!(s, "|---|--:|--:|--:|--:|--:|--:|--:|");
    for r in bid.iter().take(MAX_ROUTE_ROWS) {
        // "—" rather than a misleading 0.00 when there's nothing in the column.
        let realized = (r.would_fill_with_realized > 0).then_some(r.realized_net_sum);
        let _ = writeln!(
            s,
            "| {} | {} | {} | {} | {} | {:.2} | {} | {} |",
            r.route_key,
            r.n,
            r.would_fill,
            pct_cell(r.precision()),
            pct_cell(r.market_supported_precision()),
            r.modeled_sum,
            money_cell(realized),
            money_cell(r.mean_drift()),
        );
    }
    if bid.len() > MAX_ROUTE_ROWS {
        let _ = writeln!(s, "\n_+{} more would_fill routes (full per-route set in metrics.json)_", bid.len() - MAX_ROUTE_ROWS);
    }
    let _ = writeln!(s);
}

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
         outcomes below and is never presented as realized profit. Modeled profit \
         assumes a fill at the auction's `output_min` floor (an **upper bound**); \
         the realized column shows what the winning filler actually captured. \
         Volatile-asset routes are priced too unreliably on static prices to be \
         trusted and never count as would_fill (they show as *suspect*).\n"
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
                "- profitable_quote_rate: {:.1}% ({}/{}) — *not a win rate*; counts \
                 positive modeled profit incl. low-confidence/suspect quotes, so it \
                 overstates actionable profit (see would_fill for the trusted set)",
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
    let n = m.would_fill_with_realized;
    let _ = writeln!(s, "## would_fill calibration (model vs realized)\n");
    let _ = writeln!(s, "- intents with priceable realized data: {}", m.with_realized);
    let _ = writeln!(s, "- would_fill that also have realized data (N): **{n}**");
    if n == 0 {
        let _ = writeln!(
            s,
            "- precision / drift: **n/a** — no would_fill intent carried priceable \
             realized data, so there is nothing to calibrate against (we refuse to \
             fabricate a number).\n"
        );
    } else {
        let caveat = if n < SMALL_SAMPLE {
            format!(" *(N={n} — small sample; precision and percentiles are indicative)*")
        } else {
            String::new()
        };
        // Two precisions, both explicitly labeled (generous bar vs our own bar).
        let _ = writeln!(
            s,
            "- **precision(>0): {:.1}%** ({}/{}) — was there *any* gross margin here at all?",
            m.would_fill_precision().unwrap() * 100.0,
            m.would_fill_confirmed,
            n
        );
        let _ = writeln!(
            s,
            "- **precision(≥margin): {:.1}%** ({}/{}) — could *we* have been in the green \
             by our own min_margin bar? (the honest cut){}",
            m.would_fill_market_supported_precision().unwrap() * 100.0,
            m.would_fill_market_supported,
            n,
            caveat
        );
        // Net↔net sums over the same N checkable fills — like-for-like.
        let modeled_net_checked: f64 = out
            .fills
            .iter()
            .filter(|r| r.realized_margin_usd.is_some())
            .map(|r| r.modeled_profit_usd)
            .sum();
        let _ = writeln!(
            s,
            "- modeled_net **${:.2}** vs realized_net **${:.2}** over those {} fills — \
             **both net of our gas** (like-for-like); the per-fill gap is the drift below.",
            modeled_net_checked, m.realized_net_usd_sum, n
        );
        // Drift distribution — the *shape* of the over-optimism, not just a mean.
        if let (Some(mean), Some(p)) = (m.mean_drift_usd(), m.drift_percentiles()) {
            let _ = writeln!(
                s,
                "- drift (modeled_net − realized_net), mean **${:.2}** — positive = we \
                 over-modeled. Distribution (USD):",
                mean
            );
            let _ = writeln!(
                s,
                "  - p10 {:.2} · p25 {:.2} · p50 {:.2} · p75 {:.2} · p90 {:.2} · min {:.2} · max {:.2}",
                p[0], p[1], p[2], p[3], p[4], p[5], p[6]
            );
        }
        let _ = writeln!(s);
    }

    // --- Per-route calibration (WFC-6) ---
    render_routes(&mut s, out);

    // --- Worst over-optimism (WFC-7) ---
    // A separate sort of a *copy* by drift desc — leaves the modeled-profit
    // ordering of `out.fills` untouched. Only rows with realized data have a
    // computable drift, so the rest are filtered out (not shown as 0).
    let _ = writeln!(s, "## Worst over-optimism (top {} by drift)\n", MAX_DRIFT_ROWS);
    let mut worst: Vec<_> = out.fills.iter().filter(|r| r.drift_usd().is_some()).collect();
    if worst.is_empty() {
        let _ = writeln!(
            s,
            "_None — no would_fill carried priceable realized data to compare against._\n"
        );
    } else {
        worst.sort_by(|a, b| {
            b.drift_usd()
                .unwrap()
                .partial_cmp(&a.drift_usd().unwrap())
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        let _ = writeln!(
            s,
            "Largest gaps where modeled_net beat realized_net (drift > 0 = we \
             over-modeled). **modeled_net and realized_net are both net of our \
             gas.** Click `verify` to check the realized fill on-chain.\n"
        );
        let _ = writeln!(s, "| route | modeled_net $ | realized_net $ | drift $ | verify |");
        let _ = writeln!(s, "|---|--:|--:|--:|---|");
        for r in worst.iter().take(MAX_DRIFT_ROWS) {
            let _ = writeln!(
                s,
                "| {} | {:.2} | {:.2} | {:.2} | {} |",
                r.route_key,
                r.modeled_profit_usd,
                r.realized_net_usd().unwrap(),
                r.drift_usd().unwrap(),
                verify_cell(&r.verify_url),
            );
        }
        let _ = writeln!(s);
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
            let _ = writeln!(
                s,
                "| {} | {:.2} | {:.2} | {} | {} |",
                r.route_key, r.modeled_profit_usd, r.confidence, realized, verify_cell(&r.verify_url)
            );
        }
        let _ = writeln!(s);
    }

    s
}

fn render_metrics_json(load: &LoadStats, out: &BacktestOutput) -> Result<String> {
    let m = &out.metrics;
    let drift_percentiles = m.drift_percentiles().map(|p| {
        json!({
            "p10": p[0], "p25": p[1], "p50": p[2], "p75": p[3], "p90": p[4],
            "min": p[5], "max": p[6],
        })
    });
    let routes: Vec<_> = out
        .routes
        .iter()
        .map(|r| {
            json!({
                "route_key": r.route_key,
                "n": r.n,
                "would_fill": r.would_fill,
                "would_fill_with_realized": r.would_fill_with_realized,
                "confirmed": r.confirmed,
                "market_supported": r.market_supported,
                "modeled_net_usd_sum": r.modeled_sum,
                "realized_net_usd_sum": r.realized_net_sum,
                "mean_drift_usd": r.mean_drift(),
                "precision_gross_positive": r.precision(),
                "precision_market_supported": r.market_supported_precision(),
            })
        })
        .collect();
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
            "note": "all USD net of our gas; drift = modeled_net − realized_net (positive = over-modeled)",
            "with_realized": m.with_realized,
            "would_fill_with_realized": m.would_fill_with_realized,
            "would_fill_confirmed": m.would_fill_confirmed,
            "would_fill_market_supported": m.would_fill_market_supported,
            "would_fill_precision": m.would_fill_precision(),
            "would_fill_market_supported_precision": m.would_fill_market_supported_precision(),
            "realized_margin_usd_sum": m.realized_margin_usd_sum,
            "realized_net_usd_sum": m.realized_net_usd_sum,
            "mean_drift_usd": m.mean_drift_usd(),
            "drift_percentiles_usd": drift_percentiles,
        },
        "routes": routes,
    });
    serde_json::to_string_pretty(&v).context("serializing metrics.json")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metrics::{BacktestOutput, FillRow, Metrics};
    use crate::route_summary::RouteSummary;

    /// A coherent one-fill fixture: modeled_net 4.20, gas 0.09, realized_margin
    /// −0.30 → realized_net −0.39, drift 4.59. Neither confirmed (gross < 0) nor
    /// market-supported (net < $0.50). All the calibration fields are set to
    /// match that single row so the render is internally consistent.
    fn sample() -> (LoadStats, BacktestOutput) {
        let route_key = "base->arbitrum/0x8335…2913->0xaf88…5831".to_string();
        let load = LoadStats { files: 1, raw_lines: 3, kept: 3, duplicates: 0, malformed: 0, dropped_unknown_chain: 0 };
        let mut m = Metrics { total: 3, quoted: 3, would_fill: 1, profitable_quotes: 2, modeled_profit_usd_sum: 4.20, ..Default::default() };
        m.fill_reasons.insert("fill".into(), 1);
        m.fill_reasons.insert("unprofitable".into(), 2);
        m.with_realized = 1;
        m.would_fill_with_realized = 1;
        m.would_fill_confirmed = 0;
        m.would_fill_market_supported = 0;
        m.realized_margin_usd_sum = -0.30;
        m.realized_net_usd_sum = -0.39;
        m.drift_samples_usd = vec![4.59];
        let fills = vec![FillRow {
            id: "uniswapx:a".into(),
            route_key: route_key.clone(),
            modeled_profit_usd: 4.20,
            gas_usd: 0.09,
            confidence: 0.9,
            realized_margin_usd: Some(-0.30),
            verify_url: Some("https://arbiscan.io/tx/0xabc".into()),
        }];
        let routes = vec![RouteSummary {
            route_key,
            n: 3,
            would_fill: 1,
            would_fill_with_realized: 1,
            confirmed: 0,
            market_supported: 0,
            modeled_sum: 4.20,
            realized_net_sum: -0.39,
            drift_sum: 4.59,
        }];
        (load, BacktestOutput { metrics: m, fills, routes })
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
        assert!(md.contains("[tx](https://arbiscan.io/tx/0xabc)"), "verify link must render");
        assert!(md.contains("not a win rate"));
        assert!(!md.contains("annualized"), "no annualized projections (rule #1)");

        // Both precisions, both labeled (WFC-8). The fixture row is neither
        // confirmed nor market-supported → both read 0.0%.
        assert!(md.contains("precision(>0): 0.0%"));
        assert!(md.contains("precision(≥margin): 0.0%"));
        assert!(md.contains("any* gross margin here at all"));
        assert!(md.contains("in the green"));
        // Net↔net footing caption is present and the small-N caveat fires.
        assert!(md.contains("both net of our gas"));
        assert!(md.contains("small sample"));

        // Drift distribution renders (single sample 4.59 → every percentile 4.59).
        assert!(md.contains("Distribution (USD)"));
        assert!(md.contains("p50 4.59"));

        // Per-route section renders the one route with its mean drift.
        assert!(md.contains("## Per-route calibration"));
        assert!(md.contains("mean_drift"));

        // Worst-drift section renders the net-footed row: realized_net −0.39,
        // drift 4.59.
        assert!(md.contains("## Worst over-optimism"));
        assert!(md.contains("-0.39"), "realized_net must be net of gas");
        assert!(md.contains("4.59"), "drift must be net↔net");
    }

    /// metrics.json carries the WFC-5/6 fields: both precisions, the drift
    /// percentiles, and the per-route array — so a machine reader gets the
    /// same calibration depth as the human summary.
    #[test]
    fn metrics_json_has_calibration_depth_and_routes() {
        let (load, out) = sample();
        let js = render_metrics_json(&load, &out).unwrap();
        let v: serde_json::Value = serde_json::from_str(&js).unwrap();
        let cal = &v["calibration"];
        assert_eq!(cal["would_fill_market_supported"], 0);
        assert_eq!(cal["would_fill_precision"], 0.0);
        assert_eq!(cal["would_fill_market_supported_precision"], 0.0);
        assert!((cal["realized_net_usd_sum"].as_f64().unwrap() - (-0.39)).abs() < 1e-9);
        assert!((cal["mean_drift_usd"].as_f64().unwrap() - 4.59).abs() < 1e-9);
        assert!((cal["drift_percentiles_usd"]["p50"].as_f64().unwrap() - 4.59).abs() < 1e-9);
        // Per-route array present and self-describing.
        let routes = v["routes"].as_array().unwrap();
        assert_eq!(routes.len(), 1);
        assert_eq!(routes[0]["n"], 3);
        assert_eq!(routes[0]["would_fill"], 1);
        assert!((routes[0]["mean_drift_usd"].as_f64().unwrap() - 4.59).abs() < 1e-9);
    }

    /// The worst-drift list sorts a *copy* by drift desc and excludes rows with
    /// no realized data (no drift to compute) — without disturbing the
    /// modeled-profit ordering of the verifiable-fills table.
    #[test]
    fn worst_drift_orders_by_drift_desc_and_skips_unrealized() {
        let load = LoadStats { files: 1, raw_lines: 3, kept: 3, duplicates: 0, malformed: 0, dropped_unknown_chain: 0 };
        let m = Metrics { total: 3, quoted: 3, would_fill: 3, ..Default::default() };
        let row = |key: &str, modeled: f64, realized: Option<f64>, url: Option<&str>| FillRow {
            id: key.into(),
            route_key: key.into(),
            modeled_profit_usd: modeled,
            gas_usd: 0.0,
            confidence: 0.9,
            realized_margin_usd: realized,
            verify_url: url.map(Into::into),
        };
        // drift: r1 = 10−2 = 8, r2 = 5−4 = 1, r3 has no realized → excluded.
        let fills = vec![
            row("r1", 10.0, Some(2.0), Some("https://example/r1")),
            row("r2", 5.0, Some(4.0), None),
            row("r3", 3.0, None, None),
        ];
        let md = render_summary(&load, &BacktestOutput { metrics: m, fills, routes: vec![] });

        let wd_start = md.find("## Worst over-optimism").unwrap();
        let wd_end = md.find("## Verifiable fills").unwrap();
        let wd = &md[wd_start..wd_end];
        assert!(wd.find("r1").unwrap() < wd.find("r2").unwrap(), "higher drift first");
        assert!(!wd.contains("r3"), "no realized data → no drift row");
        assert!(wd.contains("8.00") && wd.contains("1.00"), "drift values render");
        assert!(wd.contains("[tx](https://example/r1)"), "clickable verify link");
    }

    /// The empty case is loud and tells the reader exactly how to get data —
    /// no silent empty report (legibility, CLAUDE.md rule #7).
    #[test]
    fn empty_run_is_loud_with_next_step() {
        let load = LoadStats::default();
        let out = BacktestOutput { metrics: Metrics::default(), fills: vec![], routes: vec![] };
        let md = render_summary(&load, &out);
        assert!(md.contains("No data"));
        assert!(md.contains("solver ingest uniswapx"));
    }
}
