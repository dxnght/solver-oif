//! `solver` — the engine binary. See `CLAUDE.md` §Команди.
//!
//! Scope: the no-key golden path (`ingest uniswapx` → `backtest`), the
//! key-gated cross-chain `ingest across` (Across `FilledRelay` logs via
//! Etherscan V2), and `doctor` (preflight legibility). OIF stays idle by design
//! (settler address unverified) and says so loudly rather than failing
//! silently (legibility is first-class — `CLAUDE.md` §7).

mod doctor;

use std::path::PathBuf;

use clap::{Parser, Subcommand};
use solver_ingest::{AcrossIngest, AcrossIngestConfig, UniswapXIngest, UniswapXIngestConfig};

/// Default on-disk locations per source.
const UNISWAPX_DATA_DIR: &str = "data/uniswapx";
const ACROSS_DATA_DIR: &str = "data/across";
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
        /// Source to ingest: `uniswapx` (no key) or `across` (needs key).
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
        "across" => {
            // Key-gated: Across ingest reads logs via the Etherscan V2 API.
            // Resolve the key here so the user gets one clear message (and the
            // `doctor` pointer) instead of a deep error — rule #7 legibility.
            let key = std::env::var("ETHERSCAN_API_KEY")
                .ok()
                .filter(|k| !k.trim().is_empty())
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "source 'across' needs ETHERSCAN_API_KEY (Etherscan V2 multichain key). \
                         Set it in .env or the environment. Run `solver doctor` for readiness."
                    )
                })?;
            let cfg = AcrossIngestConfig {
                days_back: days as i64,
                api_key: key,
                ..AcrossIngestConfig::defaults(PathBuf::from(ACROSS_DATA_DIR))
            };
            let n = AcrossIngest::new(cfg)?.run().await?;
            println!("ingest across: wrote {n} intents into {ACROSS_DATA_DIR}/");
            if n == 0 {
                eprintln!(
                    "note: 0 intents written — no in-window fills, or the free Etherscan key \
                     does not cover the configured chains. Run `solver doctor`."
                );
            }
            Ok(())
        }
        "oif" => anyhow::bail!(
            "source 'oif': IDLE — settler address UNVERIFIED, must be confirmed on-chain \
             first (see PROJECT-STATUS). Not ingesting."
        ),
        other => anyhow::bail!(
            "unknown source '{other}'. Known sources: 'uniswapx' (no key), \
             'across' (needs ETHERSCAN_API_KEY). Run `solver doctor`."
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
