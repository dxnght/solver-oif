//! `solver` — the engine binary. See `CLAUDE.md` §Команди.
//!
//! Verified-core scope (bucket B0): the no-key golden path (`ingest uniswapx`
//! → `backtest`) plus `doctor` (preflight legibility). Each subcommand is
//! wired in its own sprint; arms that are not yet implemented say so loudly
//! rather than failing silently (legibility is first-class — `CLAUDE.md` §7).

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "solver", version, about = "Cross-chain intent engine (ERC-7683 / OIF) — research tooling")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Ingest historical intents from a source into `data/<source>/*.jsonl`.
    Ingest {
        /// Source to ingest (verified-core: `uniswapx`).
        source: String,
        /// Lookback window in days.
        #[arg(long, default_value_t = 14)]
        days: u32,
    },
    /// Run the backtest over ingested data and write a report.
    Backtest {
        /// Output directory for the report.
        #[arg(long, default_value = "reports/run")]
        out_dir: String,
    },
    /// Preflight: print what is configured and which chains/sources will
    /// actually produce data, before any run.
    Doctor,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Ingest { source, days } => {
            anyhow::bail!("ingest {source} (--days {days}): not implemented in this sprint yet")
        }
        Command::Backtest { out_dir } => {
            anyhow::bail!("backtest (--out-dir {out_dir}): not implemented in this sprint yet")
        }
        Command::Doctor => {
            anyhow::bail!("doctor: not implemented in this sprint yet")
        }
    }
}
