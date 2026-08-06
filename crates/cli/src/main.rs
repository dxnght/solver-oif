//! `solver` — the engine binary. See `CLAUDE.md` §Команди.
//!
//! Scope: the no-key golden path (`ingest uniswapx` → `backtest`), the
//! key-gated cross-chain `ingest across` (Across `FilledRelay` logs via
//! Etherscan V2), and `doctor` (preflight legibility). OIF stays idle by design
//! (settler address unverified) and says so loudly rather than failing
//! silently (legibility is first-class — `CLAUDE.md` §7).

mod aggregates;
mod dashboard;
mod doctor;
mod live;

use std::path::PathBuf;

use clap::{Parser, Subcommand};
use solver_ingest::{AcrossIngest, AcrossIngestConfig, UniswapXIngest, UniswapXIngestConfig};
use solver_pricing::{GasSource, LadderGasModel, StaticGasModel};
use solver_router::{LivePriceFeed, PriceSource, StaticPriceFeed};
use solver_types::ChainId;

/// Default on-disk locations per source.
const UNISWAPX_DATA_DIR: &str = "data/uniswapx";
const ACROSS_DATA_DIR: &str = "data/across";

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
        /// Input directory to scan (recursively) for `*.jsonl`. Default is
        /// all of `data/` (every ingested source); narrow this to reproduce
        /// a report generated when the source mix on disk was different,
        /// e.g. `data/uniswapx` alone.
        #[arg(long, default_value = "data")]
        data_dir: String,
        /// Price the long-tail via live CoinGecko (by contract address) instead
        /// of the static feed. Default off (deterministic CI).
        #[arg(long)]
        live_prices: bool,
        /// Use live `eth_feeHistory` gas instead of the static per-chain
        /// estimate (needs `ACROSS_RPC_<CHAIN>`). Default off.
        #[arg(long)]
        live_gas: bool,
        /// Force static prices/gas for a byte-deterministic run — overrides
        /// `--live-prices`/`--live-gas` (so a repro never touches the network).
        #[arg(long)]
        reproducible: bool,
    },
    /// Phase-1 live shadow: subscribe to real-time intents and log shadow
    /// decisions. Zero transactions. Needs `ACROSS_WS_<CHAIN>` per chain.
    Live {
        /// Comma-separated origin chains, e.g. `base,arbitrum,optimism`.
        #[arg(long)]
        chains: String,
        /// Comma-separated sources. Only `across` is wired; `oif` stays idle.
        #[arg(long, default_value = "across")]
        sources: String,
        /// Stop after N seconds (omit to run until interrupted).
        #[arg(long)]
        duration: Option<u64>,
        /// Output directory for the decisions JSONL.
        #[arg(long, default_value = live::LIVE_OUT_DIR)]
        out: String,
        /// Serve a live observability dashboard on `127.0.0.1:<port>` (SSE of
        /// the same shadow decisions). Omit to keep the plain B4 behavior.
        #[arg(long)]
        dashboard_port: Option<u16>,
        /// Price the long-tail via live CoinGecko instead of the static feed —
        /// the cross-chain stream is mostly long-tail, so without this it is
        /// nearly all `price_unknown`. Default off.
        #[arg(long)]
        live_prices: bool,
        /// Force static per-chain gas instead of the **default** live ladder
        /// (etherscan→feeHistory→static). For a deterministic/offline live run.
        #[arg(long)]
        static_gas: bool,
        /// Force static prices/gas (overrides `--live-prices`, forces static gas).
        #[arg(long)]
        reproducible: bool,
    },
    /// Preflight: print what is configured and which chains/sources will
    /// actually produce data, before any run.
    Doctor {
        /// Print only the minimal no-key command (scriptable), nothing else.
        #[arg(long)]
        suggest: bool,
    },
}

/// Parse a comma-separated chain list (`base,arbitrum,…`) into [`ChainId`]s,
/// erroring loudly on an unknown name (legibility — never a silent drop).
fn parse_chains(s: &str) -> anyhow::Result<Vec<ChainId>> {
    let mut out = Vec::new();
    for name in s.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        let c = ChainId::from_name(name).ok_or_else(|| {
            anyhow::anyhow!(
                "unknown chain '{name}'. Known examples: base, arbitrum, optimism, \
                 ethereum, polygon."
            )
        })?;
        out.push(c);
    }
    if out.is_empty() {
        anyhow::bail!("no chains given; pass e.g. --chains base,arbitrum,optimism");
    }
    Ok(out)
}

/// Build the price + gas sources. Callers pass already-resolved booleans —
/// each command applies its own default and folds in the `--reproducible`
/// override — so a CI/repro run never reaches the network.
fn build_pricing(live_prices: bool, ladder_gas: bool) -> (PriceSource, GasSource) {
    let prices = if live_prices {
        PriceSource::Live(LivePriceFeed::from_env())
    } else {
        PriceSource::Static(StaticPriceFeed::reference())
    };
    let gas = if ladder_gas {
        GasSource::Ladder(Box::new(LadderGasModel::from_env()))
    } else {
        GasSource::Static(StaticGasModel)
    };
    (prices, gas)
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
        Command::Backtest { out_dir, data_dir, live_prices, live_gas, reproducible } => {
            // Backtest defaults to static gas (deterministic CI); --live-gas
            // opts into the ladder, --reproducible forces static.
            let (prices, gas) =
                build_pricing(live_prices && !reproducible, live_gas && !reproducible);
            run_backtest(&data_dir, &out_dir, prices, gas).await
        }
        Command::Live {
            chains,
            sources,
            duration,
            out,
            dashboard_port,
            live_prices,
            static_gas,
            reproducible,
        } => {
            let chains = parse_chains(&chains)?;
            let sources: Vec<String> = sources
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
            // Live defaults to the real-gas ladder (so a run is never silently
            // on static because someone forgot a flag); --static-gas or
            // --reproducible forces static.
            let (prices, gas) =
                build_pricing(live_prices && !reproducible, !static_gas && !reproducible);
            live::run(chains, sources, duration, &out, dashboard_port, prices, gas).await
        }
        Command::Doctor { suggest } => {
            run_doctor(suggest).await;
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

async fn run_backtest(data_dir: &str, out_dir: &str, prices: PriceSource, gas: GasSource) -> anyhow::Result<()> {
    let (prices_label, gas_label) = (prices.label(), gas.label());
    let summary =
        solver_backtest::run(std::path::Path::new(data_dir), std::path::Path::new(out_dir), &prices, gas)
            .await?;
    let m = &summary.metrics;
    println!(
        "backtest: loaded {} intents ({} files), quoted {}, would_fill {} \
         [prices={prices_label} gas={gas_label}] → {}/",
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

/// Etherscan V2 multichain base for the doctor probe (matches the ingest
/// client's default). Kept here so doctor needs no ingest-internal access.
const ETHERSCAN_V2_BASE: &str = "https://api.etherscan.io/v2";

async fn run_doctor(suggest_only: bool) {
    if suggest_only {
        println!("{}", doctor::SUGGEST_NO_KEY);
        return;
    }
    let lookup = doctor::process_env();
    let mut report = doctor::gather(&doctor::Env { lookup: &lookup });

    // Live per-chain Across V2 readiness. With a key we issue one
    // getblocknobytime per chain (the free key silently covers only some);
    // without a key we probe nothing and every chain reports NEEDS-KEY.
    let key = std::env::var("ETHERSCAN_API_KEY").ok().filter(|k| !k.trim().is_empty());
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // Same User-Agent the live feed uses, so the CoinGecko probe below reflects
    // what `--live-prices` will actually get (CoinGecko 403s a missing UA).
    let http = reqwest::Client::builder()
        .user_agent(solver_router::USER_AGENT)
        .build()
        .unwrap_or_default();
    let probes = doctor::probe_across(key.is_some(), |chain| {
        let http = http.clone();
        let key = key.clone();
        async move {
            let key = key.unwrap_or_default(); // only reached when key.is_some()
            let url = format!(
                "{ETHERSCAN_V2_BASE}/api?chainid={}&module=block&action=getblocknobytime\
                 &timestamp={now}&closest=before&apikey={key}",
                chain.0,
            );
            let body = http.get(&url).send().await?.text().await?;
            // Stay well under the free tier's rate cap between chains.
            tokio::time::sleep(std::time::Duration::from_millis(220)).await;
            Ok(body)
        }
    })
    .await;
    report.across_probe = Some(probes);

    // CoinGecko reachability for --live-prices (single /ping; injected fetch).
    let cg = doctor::probe_coingecko(|| {
        let http = http.clone();
        async move {
            let body = http
                .get("https://api.coingecko.com/api/v3/ping")
                .send()
                .await?
                .text()
                .await?;
            Ok(body)
        }
    })
    .await;
    report.coingecko = Some(cg);

    print!("{}", doctor::render(&report));
}
