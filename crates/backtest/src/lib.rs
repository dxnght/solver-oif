//! Backtest — load (dedup + hygiene) → metrics (RouterV0 + FillPolicy, incl.
//! would_fill calibration) → report (with verify-links). See `CLAUDE.md`
//! §Архітектура.
//!
//! Verified-core scope (bucket B0): the no-key cycle `ingest uniswapx →
//! backtest` must yield a non-empty report whose every fill carries a
//! verifiable block-explorer link. `would_fill` is a **model belief** here; it
//! becomes trust only once validated against realized outcomes (bucket B1).

pub mod load;
pub mod metrics;
pub mod report;

pub use load::{load_intents, LoadStats};
pub use metrics::{compute, BacktestOutput, FillRow, Metrics};

use std::path::Path;

use anyhow::Result;
use solver_router::StaticPriceFeed;
use solver_types::FillPolicy;

/// One-line-able outcome of a run, for the CLI to echo.
#[derive(Debug, Clone)]
pub struct RunSummary {
    pub load: LoadStats,
    pub metrics: Metrics,
    pub out_dir: String,
}

/// Load `data_dir`, run the backtest with the Phase-0 static feed + default
/// [`FillPolicy`], write the report into `out_dir`, and return a summary.
/// An empty data dir is not an error — it produces a loud "0 intents" report.
pub async fn run(data_dir: &Path, out_dir: &Path) -> Result<RunSummary> {
    let (intents, load) = load_intents(data_dir)?;
    let out = compute(&intents, StaticPriceFeed::reference(), FillPolicy::default()).await;
    report::write_report(out_dir, &load, &out)?;
    Ok(RunSummary {
        load,
        metrics: out.metrics,
        out_dir: out_dir.display().to_string(),
    })
}
