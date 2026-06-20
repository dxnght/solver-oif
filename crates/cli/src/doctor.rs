//! `solver doctor` — read-only preflight legibility (CLAUDE.md rule #7).
//!
//! Answers, **before** any run: which sources/chains will actually produce
//! data, and what is idle and why. It makes **no network calls and no
//! mutations** — it reports configuration and environment only, so it is
//! deterministic and safe to run anywhere.
//!
//! Honest scope for this restart: the only wired source is historical
//! **UniswapX** (HTTP, no key). The live **Across** adapter and its Etherscan
//! V2 block-time probe are not built yet — the doctor reports them as idle
//! (pending bucket B2) rather than probing machinery that does not exist.
//! **OIF stays idle**: its settler address is UNVERIFIED and must be confirmed
//! on-chain before we trust or probe it (PROJECT-STATUS).

use std::fmt::Write as _;

use solver_ingest::UNISWAPX_CHAINS;
use solver_types::ChainId;

/// Chains we'd watch for live Across once the adapter lands (B2). Used only to
/// report `ACROSS_WS_<CHAIN>` env presence today — nothing subscribes yet.
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

#[derive(Debug, Clone)]
pub struct DoctorReport {
    pub etherscan_key_present: bool,
    pub uniswapx_chains: Vec<ChainId>,
    /// (chain, `ACROSS_WS_<CHAIN>` present).
    pub across_ws_env: Vec<(ChainId, bool)>,
}

impl DoctorReport {
    /// True when at least one source will produce data on the no-key path.
    pub fn any_data(&self) -> bool {
        !self.uniswapx_chains.is_empty()
    }
}

/// Gather the report from the environment. Pure given `env`; no I/O.
pub fn gather(env: &Env) -> DoctorReport {
    let across_ws_env = ACROSS_LIVE_CHAINS
        .iter()
        .map(|c| (*c, env.present(&format!("ACROSS_WS_{}", c.name().to_uppercase()))))
        .collect();
    DoctorReport {
        etherscan_key_present: env.present("ETHERSCAN_API_KEY"),
        uniswapx_chains: UNISWAPX_CHAINS.to_vec(),
        across_ws_env,
    }
}

/// Render the human report. Pure (string out) so it's snapshot-testable.
pub fn render(r: &DoctorReport) -> String {
    let mut s = String::new();

    let _ = writeln!(s, "solver doctor — preflight (read-only, no network)\n");

    // Loud top line on legibility of the data situation.
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
    let across_env_summary = {
        let set: Vec<&str> = r
            .across_ws_env
            .iter()
            .filter(|(_, present)| *present)
            .map(|(c, _)| c.name())
            .collect();
        if set.is_empty() { "no ACROSS_WS_*".to_string() } else { set.join(",") }
    };
    let _ = writeln!(
        s,
        "across              live        {:<19}  IDLE (adapter pending B2)",
        across_env_summary
    );
    let _ = writeln!(
        s,
        "oif                 live        n/a                  IDLE (address UNVERIFIED)"
    );
    let _ = writeln!(
        s,
        "etherscan v2        (probe)     {:<19}  UNUSED until Across lands (B2)\n",
        if r.etherscan_key_present { "ETHERSCAN_API_KEY set" } else { "no key" }
    );

    // One-line legibility summary.
    let _ = writeln!(
        s,
        "Historical: uniswapx ready on {} chains (no key). \
         Live: across=idle(B2), uniswapx-realtime=n/a, oif=idle(unverified).",
        r.uniswapx_chains.len()
    );

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
        let base = r.across_ws_env.iter().find(|(c, _)| *c == ChainId::BASE).unwrap();
        assert!(base.1, "ACROSS_WS_BASE present");
        let arb = r.across_ws_env.iter().find(|(c, _)| *c == ChainId::ARBITRUM).unwrap();
        assert!(!arb.1, "ACROSS_WS_ARBITRUM absent");
    }

    /// The default (empty env) render is loud-positive about the no-key path,
    /// keeps OIF explicitly UNVERIFIED, and prints the suggest command. These
    /// are the legibility guarantees (rule #7).
    #[test]
    fn render_is_legible_and_honest() {
        let lookup = env_from(HashMap::new());
        let r = gather(&Env { lookup: &lookup });
        let out = render(&r);
        assert!(out.contains("NO API key"));
        assert!(out.contains("DATA-OK"));
        assert!(out.contains("oif"));
        assert!(out.contains("UNVERIFIED"));
        assert!(out.contains("IDLE (adapter pending B2)"));
        assert!(out.contains(SUGGEST_NO_KEY));
    }

    /// If no source would produce data, the top line is a loud ✗ — a silent
    /// "all idle" is forbidden.
    #[test]
    fn render_is_loud_when_no_data() {
        let r = DoctorReport {
            etherscan_key_present: false,
            uniswapx_chains: vec![],
            across_ws_env: vec![],
        };
        let out = render(&r);
        assert!(out.contains("✗ NOTHING will produce data"));
    }
}
