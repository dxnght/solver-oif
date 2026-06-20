//! `solver` — the engine binary. See `CLAUDE.md` §Команди.
//!
//! Verified-core scope (bucket B0): the no-key golden path (`ingest uniswapx`
//! → `backtest`) plus `doctor` (preflight legibility). Sources beyond UniswapX
//! are idle by design (Across pending B2; OIF unverified) and say so loudly
//! rather than failing silently (legibility is first-class — `CLAUDE.md` §7).

mod doctor;

use std::path::PathBuf;

use clap::{Parser, Subcommand};
use solver_ingest::{UniswapXIngest, UniswapXIngestConfig};

/// Default on-disk locations for the no-key golden path.
const UNISWAPX_DATA_DIR: &str = "data/uniswapx";
const DATA_DIR: &str = "data";

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
    Doctor {
        /// Print only the minimal no-key command (scriptable), nothing else.
        #[arg(long)]
        suggest: bool,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_target(false)
        .init();

    let cli = Cli::parse();
    match cli.command {
        Command::Ingest { source, days } => run_ingest(&source, days).await,
        Command::Backtest { out_dir } => run_backtest(&out_dir).await,
        Command::Doctor { suggest } => {
            run_doctor(suggest);
            Ok(())
        }
    }
}

async fn run_ingest(source: &str, days: u32) -> anyhow::Result<()> {
    match source {
        "uniswapx" => {
            let cfg = UniswapXIngestConfig {
                days_back: days as i64,
                ..UniswapXIngestConfig::defaults(PathBuf::from(UNISWAPX_DATA_DIR))
            };
            let n = UniswapXIngest::new(cfg)?.run().await?;
            println!("ingest uniswapx: wrote {n} intents into {UNISWAPX_DATA_DIR}/");
            if n == 0 {
                eprintln!(
                    "note: 0 intents written — the API returned nothing for this window. \
                     Try a longer --days, or check network reachability."
                );
            }
            Ok(())
        }
        "across" => anyhow::bail!(
            "source 'across': IDLE in verified-core (live adapter pending bucket B2). \
             Use `solver doctor` to see what will produce data."
        ),
        "oif" => anyhow::bail!(
            "source 'oif': IDLE — settler address UNVERIFIED, must be confirmed on-chain \
             first (see PROJECT-STATUS). Not ingesting."
        ),
        other => anyhow::bail!(
            "unknown source '{other}'. Verified-core source is 'uniswapx'. \
             Run `solver doctor`."
        ),
    }
}

async fn run_backtest(out_dir: &str) -> anyhow::Result<()> {
    let summary = solver_backtest::run(std::path::Path::new(DATA_DIR), std::path::Path::new(out_dir)).await?;
    let m = &summary.metrics;
    println!(
        "backtest: loaded {} intents ({} files), quoted {}, would_fill {} → {}/",
        summary.load.kept, summary.load.files, m.quoted, m.would_fill, summary.out_dir
    );
    match m.would_fill_precision() {
        Some(p) => println!(
            "would_fill_precision: {:.1}% ({}/{} checkable) — MODELED on static prices",
            p * 100.0,
            m.would_fill_confirmed,
            m.would_fill_with_realized
        ),
        None => println!("would_fill_precision: n/a (no checkable realized data yet)"),
    }
    if summary.load.kept == 0 {
        eprintln!("note: 0 intents loaded — run `solver ingest uniswapx --days 14` first.");
    }
    Ok(())
}

fn run_doctor(suggest_only: bool) {
    if suggest_only {
        println!("{}", doctor::SUGGEST_NO_KEY);
        return;
    }
    let lookup = doctor::process_env();
    let report = doctor::gather(&doctor::Env { lookup: &lookup });
    print!("{}", doctor::render(&report));
}
