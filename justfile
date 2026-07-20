# justfile — golden commands. Run `just` to list. Requires `just` (cargo install just).
# Keeps the verified paths in one place so "which command actually works" is never a guess.

export PATH := env_var('HOME') + "/.cargo/bin:" + env_var('PATH')

# List available recipes.
default:
    @just --list

# /verify — run after every commit. Compiler is the source of truth, not "on paper".
verify:
    cargo build --workspace
    cargo clippy --workspace -- -D warnings
    cargo test --workspace

build:
    cargo build --workspace --release

# Preflight: what is configured, what will actually produce data. Run before any ingest/live.
doctor:
    ./target/release/solver doctor

# No-key golden path: real data → verifiable report (with explorer links).
demo:
    ./target/release/solver ingest uniswapx --days 14
    ./target/release/solver backtest --out-dir reports/demo
    @echo "report → reports/demo/phase0_report.md"

# Historical ingest (Across needs ETHERSCAN_API_KEY; UniswapX needs none).
ingest-uniswapx days="14":
    ./target/release/solver ingest uniswapx --days {{days}}
ingest-across days="14":
    ./target/release/solver ingest across --days {{days}}

backtest out="reports/run":
    ./target/release/solver backtest --out-dir {{out}}

# Live shadow (needs ACROSS_WS_* in .env; idles + warns without them).
live duration="600":
    ./target/release/solver live --chains base,arbitrum,optimism --sources across \
        --dashboard-port 8088 --duration {{duration}} --out reports/live

# Quick terminal summary of live decisions (reason / gas-source distribution)
# straight from the JSONL — no server needed, works on an old reports/live/
# dir with nothing running. For a live, time-windowed view (24h/7d,
# exclusivity %) run `solver live --dashboard-port` and hit GET /aggregates
# instead (BAGG-1) — this recipe doesn't window by time, it's a raw tally.
summary dir="reports/live":
    @grep -h -o '"reason":"[a-z_]*"' {{dir}}/decisions-*.jsonl | sort | uniq -c
    @grep -h -o '"gas_source":"[a-z]*"' {{dir}}/decisions-*.jsonl | sort | uniq -c

# BDEPLOY-1: after `git push`, one command to ship — SSH in and run the same
# pull/build/verify/restart update.sh a server admin would run by hand.
# Usage: just deploy-remote host=you@your.vps
deploy-remote host:
    ssh {{host}} '~/solver-oif/deploy/update.sh'
