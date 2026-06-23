//! `solver doctor` — preflight legibility (CLAUDE.md rule #7).
//!
//! Answers, **before** any run: which sources/chains will actually produce
//! data, and what is idle and why. The static parts (env presence, source
//! table) make no network calls. The one exception is the **Across Etherscan
//! V2 probe**: when a key is present, doctor does a single lightweight
//! `getblocknobytime` per Across chain to report real per-chain readiness
//! (DATA-OK / NO-V2 / ERROR) — because the free key silently covers only a
//! subset of chains, and finding that out at ingest time is exactly the kind
//! of quiet failure rule #7 forbids.
//!
//! Honest scope: historical **UniswapX** (HTTP, no key), historical **Across**
//! (`FilledRelay` via Etherscan V2, key-gated), and live **Across** (WS deposit
//! subscription, B4) are wired. Doctor now reports both historical (V2) and
//! live (WS) per-chain readiness: a live chain is ready when its
//! `ACROSS_WS_<CHAIN>` is set **and** we hold a verified SpokePool for it.
//! **OIF stays idle**: its settler address is UNVERIFIED and must be confirmed
//! on-chain before we trust or probe it.

use std::fmt::Write as _;
use std::future::Future;

use solver_ingest::across::spoke_pool;
use solver_ingest::{ACROSS_CHAINS, UNISWAPX_CHAINS};
use solver_types::ChainId;

/// Chains the live Across adapter (B4) subscribes to when their
/// `ACROSS_WS_<CHAIN>` is set. Matches the `solver live --chains
/// base,arbitrum,optimism` golden path; doctor reports per-chain WS readiness
/// for exactly this set.
const ACROSS_LIVE_CHAINS: &[ChainId] = &[ChainId::BASE, ChainId::ARBITRUM, ChainId::OPTIMISM];

/// The minimal command that produces a verifiable result with no API key.
pub const SUGGEST_NO_KEY: &str = "solver ingest uniswapx --days 14 && solver backtest";

/// Read-only environment snapshot the doctor reasons over. Injected (rather
/// than read from the process) so the render is unit-tested deterministically.
pub struct Env<'a> {
    pub lookup: &'a dyn Fn(&str) -> Option<String>,
}

impl Env<'_> {
    pub fn present(&self, key: &str) -> bool {
        (self.lookup)(key).map(|v| !v.trim().is_empty()).unwrap_or(false)
    }
}

/// Process-backed env accessor for the real CLI path.
pub fn process_env() -> impl Fn(&str) -> Option<String> {
    |k: &str| std::env::var(k).ok()
}

/// Per-chain Etherscan V2 readiness for Across historical ingest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProbeStatus {
    /// `getblocknobytime` returned a block — this chain will yield data.
    DataOk,
    /// Free key doesn't cover this chain (the "Free API access is not
    /// supported" envelope) — configured but will produce nothing.
    NoV2,
    /// No key set — we did not probe (and would not ingest).
    NeedsKey,
    /// Any other failure (bad key, rate limit, network) — carries the reason.
    Error(String),
}

impl ProbeStatus {
    pub fn label(&self) -> &str {
        match self {
            ProbeStatus::DataOk => "DATA-OK",
            ProbeStatus::NoV2 => "NO-V2",
            ProbeStatus::NeedsKey => "NEEDS-KEY",
            ProbeStatus::Error(_) => "ERROR",
        }
    }
    /// True only when this chain will actually produce Across data.
    pub fn is_data(&self) -> bool {
        matches!(self, ProbeStatus::DataOk)
    }
}

/// One Across chain's historical readiness: V2 probe outcome + whether we hold
/// a verified SpokePool for it.
#[derive(Debug, Clone)]
pub struct ChainProbe {
    pub chain: ChainId,
    pub status: ProbeStatus,
    pub has_spoke_pool: bool,
}

/// CoinGecko reachability for live pricing (`--live-prices`). A single
/// lightweight `/ping`; like the Across probe, the `fetch` is injected so the
/// CLI hits the network while tests pass a canned body (no network in CI).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CoinGeckoStatus {
    /// `/ping` answered with the expected `gecko_says` body.
    Reachable,
    /// Unreachable or an unexpected body — live pricing would fall back to the
    /// static feed (carries the reason).
    Error(String),
}

impl CoinGeckoStatus {
    pub fn label(&self) -> &str {
        match self {
            CoinGeckoStatus::Reachable => "REACHABLE",
            CoinGeckoStatus::Error(_) => "ERROR",
        }
    }
}

#[derive(Debug, Clone)]
pub struct DoctorReport {
    pub etherscan_key_present: bool,
    pub uniswapx_chains: Vec<ChainId>,
    /// (chain, `ACROSS_WS_<CHAIN>` present) — the live deposit subscription.
    pub across_ws_env: Vec<(ChainId, bool)>,
    /// (chain, `ACROSS_RPC_<CHAIN>` present) — the live-gas `eth_feeHistory`
    /// endpoint. A chain without one still works (static gas fallback); this
    /// just reports whether the LIVE gas number is available there.
    pub across_rpc_env: Vec<(ChainId, bool)>,
    /// Per-chain Across V2 readiness. `None` until the probe runs (e.g. the
    /// pure render tests); the CLI always populates it.
    pub across_probe: Option<Vec<ChainProbe>>,
    /// CoinGecko reachability for `--live-prices`. `None` until probed.
    pub coingecko: Option<CoinGeckoStatus>,
}

impl DoctorReport {
    /// True when at least one source will produce data on the no-key path.
    pub fn any_data(&self) -> bool {
        !self.uniswapx_chains.is_empty()
    }

    /// How many Across chains will actually yield data (DATA-OK), out of the
    /// configured set. `None` when the probe hasn't run.
    pub fn across_data_ratio(&self) -> Option<(usize, usize)> {
        self.across_probe
            .as_ref()
            .map(|p| (p.iter().filter(|c| c.status.is_data()).count(), p.len()))
    }

    /// How many live chains are WS-ready (`ACROSS_WS_<CHAIN>` set **and** a
    /// verified SpokePool), out of the configured live set. A static check (no
    /// network) — env presence + the SpokePool table.
    pub fn across_ws_ratio(&self) -> (usize, usize) {
        let ready = self
            .across_ws_env
            .iter()
            .filter(|(c, present)| *present && spoke_pool(*c).is_some())
            .count();
        (ready, self.across_ws_env.len())
    }

    /// How many live chains have an `ACROSS_RPC_<CHAIN>` set for live gas, out
    /// of the configured live set. Static env check (no network).
    pub fn across_rpc_ratio(&self) -> (usize, usize) {
        let set = self.across_rpc_env.iter().filter(|(_, present)| *present).count();
        (set, self.across_rpc_env.len())
    }
}

/// Gather the static report from the environment. Pure given `env`; no I/O.
/// `across_probe` is left `None` for the caller to fill via [`probe_across`].
pub fn gather(env: &Env) -> DoctorReport {
    let across_ws_env = ACROSS_LIVE_CHAINS
        .iter()
        .map(|c| (*c, env.present(&format!("ACROSS_WS_{}", c.name().to_uppercase()))))
        .collect();
    let across_rpc_env = ACROSS_LIVE_CHAINS
        .iter()
        .map(|c| (*c, env.present(&format!("ACROSS_RPC_{}", c.name().to_uppercase()))))
        .collect();
    DoctorReport {
        etherscan_key_present: env.present("ETHERSCAN_API_KEY"),
        uniswapx_chains: UNISWAPX_CHAINS.to_vec(),
        across_ws_env,
        across_rpc_env,
        across_probe: None,
        coingecko: None,
    }
}

/// Classify a `getblocknobytime` response body into a [`ProbeStatus`]. Pure —
/// the network is the caller's concern, so this is unit-tested with the exact
/// envelopes Etherscan returns (a real DATA-OK and the real unsupported-chain
/// body). `status:"1"` is DATA-OK; the "not supported" envelope is NO-V2;
/// anything else is a surfaced ERROR.
pub fn classify_probe(body: &str) -> ProbeStatus {
    #[derive(serde::Deserialize)]
    struct Env {
        status: String,
        #[serde(default)]
        message: String,
        #[serde(default)]
        result: serde_json::Value,
    }
    let env: Env = match serde_json::from_str(body) {
        Ok(e) => e,
        Err(e) => return ProbeStatus::Error(format!("unparsable response: {e}")),
    };
    if env.status == "1" {
        return ProbeStatus::DataOk;
    }
    let reason = env.result.as_str().filter(|s| !s.is_empty()).unwrap_or(&env.message);
    if reason.contains("not supported") {
        ProbeStatus::NoV2
    } else {
        ProbeStatus::Error(reason.to_string())
    }
}

/// Probe each Across chain's V2 readiness. `fetch` is injected so the CLI
/// supplies a real HTTP GET while tests supply canned bodies (no network in
/// CI, per the AX-6 mock requirement). When `key_present` is false we do **not**
/// call `fetch` at all — every chain is NEEDS-KEY.
pub async fn probe_across<F, Fut>(key_present: bool, fetch: F) -> Vec<ChainProbe>
where
    F: Fn(ChainId) -> Fut,
    Fut: Future<Output = anyhow::Result<String>>,
{
    let mut out = Vec::with_capacity(ACROSS_CHAINS.len());
    for &chain in ACROSS_CHAINS {
        let has_spoke_pool = spoke_pool(chain).is_some();
        let status = if !key_present {
            ProbeStatus::NeedsKey
        } else {
            match fetch(chain).await {
                Ok(body) => classify_probe(&body),
                Err(e) => ProbeStatus::Error(e.to_string()),
            }
        };
        out.push(ChainProbe { chain, status, has_spoke_pool });
    }
    out
}

/// Classify a CoinGecko `/ping` body. The live API answers
/// `{"gecko_says":"(V3) To the Moon!"}`; anything else means live pricing would
/// fall back to static. Pure — the network is the caller's concern.
pub fn classify_coingecko(body: &str) -> CoinGeckoStatus {
    if body.contains("gecko_says") {
        CoinGeckoStatus::Reachable
    } else {
        CoinGeckoStatus::Error(format!("unexpected /ping response: {body}"))
    }
}

/// Probe CoinGecko reachability via an injected `fetch` (a real GET in the CLI,
/// a canned body in tests). A fetch error is itself an `Error` status — live
/// pricing degrades to static, which doctor must say *before* a run (rule #7).
pub async fn probe_coingecko<F, Fut>(fetch: F) -> CoinGeckoStatus
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = anyhow::Result<String>>,
{
    match fetch().await {
        Ok(body) => classify_coingecko(&body),
        Err(e) => CoinGeckoStatus::Error(e.to_string()),
    }
}

/// Render the human report. Pure (string out) so it's snapshot-testable.
pub fn render(r: &DoctorReport) -> String {
    let mut s = String::new();

    let _ = writeln!(s, "solver doctor — preflight\n");

    // Loud top line on legibility of the no-key data situation.
    if r.any_data() {
        let _ = writeln!(
            s,
            "✓ A verifiable result is available with NO API key: historical UniswapX.\n"
        );
    } else {
        let _ = writeln!(
            s,
            "✗ NOTHING will produce data with the current build/config. See below.\n"
        );
    }

    // Source table.
    let _ = writeln!(s, "source              mode        key/env              status");
    let _ = writeln!(s, "------------------  ----------  -------------------  ------------------------------");
    let _ = writeln!(
        s,
        "uniswapx            historical  none needed          DATA-OK on {} chains",
        r.uniswapx_chains.len()
    );
    let across_status = match (r.etherscan_key_present, r.across_data_ratio()) {
        (true, Some((n, m))) => format!("DATA-OK on {n}/{m} chains (V2 probe)"),
        _ => "NEEDS-KEY (set ETHERSCAN_API_KEY)".to_string(),
    };
    let _ = writeln!(
        s,
        "across              historical  {:<19}  {}",
        if r.etherscan_key_present { "ETHERSCAN_API_KEY set" } else { "no key" },
        across_status,
    );
    let across_ws_summary = {
        let set: Vec<&str> = r
            .across_ws_env
            .iter()
            .filter(|(_, present)| *present)
            .map(|(c, _)| c.name())
            .collect();
        if set.is_empty() { "no ACROSS_WS_*".to_string() } else { set.join(",") }
    };
    let (ws_ready, ws_total) = r.across_ws_ratio();
    let ws_status = if ws_ready > 0 {
        format!("READY on {ws_ready}/{ws_total} chains (WS)")
    } else {
        "IDLE (no ACROSS_WS_*)".to_string()
    };
    let _ = writeln!(
        s,
        "across (live)       live        {:<19}  {}",
        across_ws_summary, ws_status,
    );
    let _ = writeln!(
        s,
        "oif                 live        n/a                  IDLE (address UNVERIFIED)\n"
    );

    // Per-chain Across V2 readiness — the real probe (or NEEDS-KEY).
    if let Some(probe) = &r.across_probe {
        let _ = writeln!(s, "Across historical — per-chain Etherscan V2 readiness:");
        for c in probe {
            let note = match &c.status {
                ProbeStatus::NoV2 => "  (free key: chain unsupported)".to_string(),
                ProbeStatus::Error(e) => format!("  ({e})"),
                _ => String::new(),
            };
            let _ = writeln!(
                s,
                "  {:<10}  {:<9}  spoke {}{}",
                c.chain.name(),
                c.status.label(),
                if c.has_spoke_pool { "✓" } else { "✗ PENDING" },
                note,
            );
        }
        if let Some((n, m)) = r.across_data_ratio() {
            let _ = writeln!(s, "  → Across will produce data on {n}/{m} configured chains.");
        }
        let _ = writeln!(s);
    }

    // Per-chain live WS readiness (LIV-5): ACROSS_WS_<CHAIN> presence + spoke.
    if !r.across_ws_env.is_empty() {
        let _ = writeln!(s, "Across live — per-chain WS readiness (ACROSS_WS_<CHAIN>):");
        for (c, present) in &r.across_ws_env {
            let _ = writeln!(
                s,
                "  {:<10}  {:<10}  spoke {}",
                c.name(),
                if *present { "WS-SET" } else { "WS-MISSING" },
                if spoke_pool(*c).is_some() { "✓" } else { "✗ PENDING" },
            );
        }
        let (n, m) = r.across_ws_ratio();
        let _ = writeln!(s, "  → Live Across will subscribe on {n}/{m} configured chains.");
        let _ = writeln!(s);
    }

    // Live pricing readiness (LP-5): CoinGecko reachability (--live-prices) and
    // per-chain ACROSS_RPC presence (--live-gas). Both degrade to static, so a
    // run is legible about whether the LIVE numbers will actually be available.
    if r.coingecko.is_some() || !r.across_rpc_env.is_empty() {
        let _ = writeln!(s, "Live pricing — readiness (--live-prices / --live-gas):");
        if let Some(cg) = &r.coingecko {
            let note = match cg {
                CoinGeckoStatus::Error(e) => format!("  ({e})"),
                CoinGeckoStatus::Reachable => String::new(),
            };
            let _ = writeln!(s, "  coingecko   {:<10}  --live-prices source{}", cg.label(), note);
        }
        for (c, present) in &r.across_rpc_env {
            let _ = writeln!(
                s,
                "  {:<10}  {:<10}  live gas via ACROSS_RPC_{}",
                c.name(),
                if *present { "RPC-SET" } else { "RPC-MISSING" },
                c.name().to_uppercase(),
            );
        }
        let (n, m) = r.across_rpc_ratio();
        let _ = writeln!(s, "  → Live gas on {n}/{m} configured chains (others use static gas).");
        let _ = writeln!(s);
    }

    // One-line legibility summary.
    let across_line = match (r.etherscan_key_present, r.across_data_ratio()) {
        (true, Some((n, m))) => format!("across=historical {n}/{m} chains"),
        _ => "across=needs-key".to_string(),
    };
    let (ws_n, ws_m) = r.across_ws_ratio();
    let _ = writeln!(
        s,
        "Historical: uniswapx ready on {} chains (no key), {across_line}. \
         Live: across-ws={ws_n}/{ws_m} set, oif=idle(unverified).",
        r.uniswapx_chains.len()
    );
    let (rpc_n, rpc_m) = r.across_rpc_ratio();
    let cg = match &r.coingecko {
        Some(CoinGeckoStatus::Reachable) => "reachable",
        Some(CoinGeckoStatus::Error(_)) => "unreachable",
        None => "not-probed",
    };
    let _ = writeln!(s, "Live pricing: coingecko={cg}, across-rpc={rpc_n}/{rpc_m} set (else static).");

    // Always show the next step (scriptable form is `--suggest`).
    let _ = writeln!(s, "\nNext (no key needed):\n    {SUGGEST_NO_KEY}");

    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env_from(map: HashMap<&'static str, &'static str>) -> impl Fn(&str) -> Option<String> {
        move |k: &str| map.get(k).map(|v| v.to_string())
    }

    /// gather reflects exactly the injected env: ETHERSCAN present, only the
    /// Base WS var present. Independent fixture, not readback.
    #[test]
    fn gather_reads_injected_env() {
        let lookup = env_from(HashMap::from([
            ("ETHERSCAN_API_KEY", "abc"),
            ("ACROSS_WS_BASE", "wss://x"),
        ]));
        let env = Env { lookup: &lookup };
        let r = gather(&env);
        assert!(r.etherscan_key_present);
        assert!(!r.uniswapx_chains.is_empty());
        assert!(r.across_probe.is_none(), "gather is static; probe runs separately");
        let base = r.across_ws_env.iter().find(|(c, _)| *c == ChainId::BASE).unwrap();
        assert!(base.1, "ACROSS_WS_BASE present");
        let arb = r.across_ws_env.iter().find(|(c, _)| *c == ChainId::ARBITRUM).unwrap();
        assert!(!arb.1, "ACROSS_WS_ARBITRUM absent");
    }

    /// classify_probe maps the **real** Etherscan envelopes: a DATA-OK
    /// getblocknobytime, the exact unsupported-chain body the free key returns
    /// for Base/Optimism, and an unexpected error. Independent truth (captured
    /// bodies), not a constant vs itself.
    #[test]
    fn classify_probe_maps_real_envelopes() {
        assert_eq!(
            classify_probe(r#"{"status":"1","message":"OK","result":"25363619"}"#),
            ProbeStatus::DataOk
        );
        let unsupported = r#"{"status":"0","message":"NOTOK","result":"Free API access is not supported for this chain. Please upgrade your api plan for full chain coverage. https://etherscan.io/apis"}"#;
        assert_eq!(classify_probe(unsupported), ProbeStatus::NoV2);
        let bad = r#"{"status":"0","message":"NOTOK","result":"Invalid API Key"}"#;
        assert!(matches!(classify_probe(bad), ProbeStatus::Error(e) if e.contains("Invalid API Key")));
    }

    /// AX-6 mock probe (no network): with a key, a fake `fetch` returns a
    /// DATA-OK body for ethereum/polygon/arbitrum and the unsupported body for
    /// optimism/base — mirroring what the free key actually does. The probe
    /// must classify each correctly and the ratio must be 3/5.
    #[tokio::test]
    async fn probe_across_classifies_per_chain_mocked() {
        let probes = probe_across(true, |chain| async move {
            let ok = r#"{"status":"1","message":"OK","result":"123"}"#;
            let no = r#"{"status":"0","message":"NOTOK","result":"Free API access is not supported for this chain."}"#;
            Ok(match chain {
                ChainId::OPTIMISM | ChainId::BASE => no.to_string(),
                _ => ok.to_string(),
            })
        })
        .await;
        let report = DoctorReport {
            etherscan_key_present: true,
            uniswapx_chains: UNISWAPX_CHAINS.to_vec(),
            across_ws_env: vec![],
            across_rpc_env: vec![],
            across_probe: Some(probes),
            coingecko: None,
        };
        assert_eq!(report.across_data_ratio(), Some((3, 5)));
        let out = render(&report);
        assert!(out.contains("DATA-OK on 3/5 chains"));
        assert!(out.contains("NO-V2"));
        assert!(out.contains("free key: chain unsupported"));
        assert!(out.contains("→ Across will produce data on 3/5 configured chains."));
    }

    /// Without a key, the probe makes **no** network call and every chain is
    /// NEEDS-KEY (the fetch closure panics if called — proving it isn't).
    #[tokio::test]
    async fn probe_across_no_key_skips_network() {
        let probes = probe_across(false, |_chain| async move {
            panic!("fetch must not be called when no key is present");
        })
        .await;
        assert_eq!(probes.len(), ACROSS_CHAINS.len());
        assert!(probes.iter().all(|p| p.status == ProbeStatus::NeedsKey));
        // Every configured Across chain still has a verified SpokePool.
        assert!(probes.iter().all(|p| p.has_spoke_pool));
    }

    /// The default (empty env, no probe) render is loud-positive about the
    /// no-key path, keeps OIF explicitly UNVERIFIED, reports Across as
    /// needs-key, and prints the suggest command. Legibility guarantees (#7).
    #[test]
    fn render_is_legible_and_honest() {
        let lookup = env_from(HashMap::new());
        let r = gather(&Env { lookup: &lookup });
        let out = render(&r);
        assert!(out.contains("NO API key"));
        assert!(out.contains("DATA-OK on"));
        assert!(out.contains("oif"));
        assert!(out.contains("UNVERIFIED"));
        assert!(out.contains("across=needs-key"));
        // Empty env → live Across is idle, every live chain WS-MISSING.
        assert!(out.contains("IDLE (no ACROSS_WS_*)"));
        assert!(out.contains("WS-MISSING"));
        assert!(out.contains("across-ws=0/3 set"));
        assert!(out.contains(SUGGEST_NO_KEY));
    }

    /// LIV-5: with one `ACROSS_WS_<CHAIN>` set, doctor reports per-chain WS
    /// readiness (WS-SET vs WS-MISSING) and the live ratio — so a half-configured
    /// live run is legible *before* it starts, not a silent partial subscribe.
    #[test]
    fn render_shows_live_ws_readiness() {
        let lookup = env_from(HashMap::from([("ACROSS_WS_BASE", "wss://x")]));
        let r = gather(&Env { lookup: &lookup });
        assert_eq!(r.across_ws_ratio(), (1, 3));
        let out = render(&r);
        assert!(out.contains("per-chain WS readiness"));
        assert!(out.contains("WS-SET"), "base is set");
        assert!(out.contains("WS-MISSING"), "arbitrum/optimism unset");
        assert!(out.contains("READY on 1/3 chains (WS)"));
        assert!(out.contains("Live Across will subscribe on 1/3 configured chains."));
        assert!(out.contains("across-ws=1/3 set"));
    }

    /// If no source would produce data, the top line is a loud ✗.
    #[test]
    fn render_is_loud_when_no_data() {
        let r = DoctorReport {
            etherscan_key_present: false,
            uniswapx_chains: vec![],
            across_ws_env: vec![],
            across_rpc_env: vec![],
            across_probe: None,
            coingecko: None,
        };
        let out = render(&r);
        assert!(out.contains("✗ NOTHING will produce data"));
    }

    /// LP-5: gather reads `ACROSS_RPC_<CHAIN>` presence per live chain (the
    /// live-gas endpoint), independent of the WS vars.
    #[test]
    fn gather_reads_rpc_env() {
        let lookup = env_from(HashMap::from([("ACROSS_RPC_ARBITRUM", "https://arb")]));
        let r = gather(&Env { lookup: &lookup });
        assert_eq!(r.across_rpc_ratio(), (1, 3));
        let arb = r.across_rpc_env.iter().find(|(c, _)| *c == ChainId::ARBITRUM).unwrap();
        assert!(arb.1, "ACROSS_RPC_ARBITRUM present");
        let base = r.across_rpc_env.iter().find(|(c, _)| *c == ChainId::BASE).unwrap();
        assert!(!base.1, "ACROSS_RPC_BASE absent");
    }

    /// LP-5: classify_coingecko maps the **real** `/ping` body to Reachable and
    /// an unexpected body to Error — independent truth (the real ping shape).
    #[test]
    fn classify_coingecko_maps_ping() {
        assert_eq!(
            classify_coingecko(r#"{"gecko_says":"(V3) To the Moon!"}"#),
            CoinGeckoStatus::Reachable
        );
        assert!(matches!(classify_coingecko("<html>503</html>"), CoinGeckoStatus::Error(_)));
    }

    /// LP-5: probe_coingecko classifies a canned ok body and surfaces a fetch
    /// error as Error — no network in CI (the closure is the seam).
    #[tokio::test]
    async fn probe_coingecko_mocked() {
        let ok = probe_coingecko(|| async {
            Ok(r#"{"gecko_says":"(V3) To the Moon!"}"#.to_string())
        })
        .await;
        assert_eq!(ok, CoinGeckoStatus::Reachable);
        let err = probe_coingecko(|| async { anyhow::bail!("offline") }).await;
        assert!(matches!(err, CoinGeckoStatus::Error(e) if e.contains("offline")));
    }

    /// LP-5: render shows the live-pricing block — CoinGecko status + per-chain
    /// RPC readiness — so a `--live-*` run is legible before it starts.
    #[test]
    fn render_shows_live_pricing_readiness() {
        let lookup = env_from(HashMap::from([("ACROSS_RPC_BASE", "https://base")]));
        let mut r = gather(&Env { lookup: &lookup });
        r.coingecko = Some(CoinGeckoStatus::Reachable);
        let out = render(&r);
        assert!(out.contains("Live pricing — readiness"));
        assert!(out.contains("REACHABLE"));
        assert!(out.contains("RPC-SET"), "base rpc set");
        assert!(out.contains("RPC-MISSING"), "arb/opt rpc missing");
        assert!(out.contains("Live gas on 1/3 configured chains"));
        assert!(out.contains("Live pricing: coingecko=reachable, across-rpc=1/3 set"));
    }
}
