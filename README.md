# Solver Engine

A cross-chain intent engine for **ERC-7683 / OIF**, written in Rust. It ingests
intents from Across and UniswapX, normalizes them into one model, and runs a
white-box router to study which intents could be filled profitably — across the
**long-tail of L2↔L2 routes** that large solvers and analytics tools tend to ignore.

## Honest status (read this first)

The original commercial thesis — earning by filling long-tail cross-chain intents
with zero inventory — was **falsified during Phase 0**: the honest cross-chain edge
is ≈ 0 (incumbent capital, latency, and volume competition eat the margin). This
repo is therefore a **measurement / observability / research artifact** and a
candidate for **grant-funded intent tooling**, not a money-making bot.

That falsification is the project's most valuable output. The engine's real
contribution is **measurement discipline**: every headline number is traceable,
validated against realized on-chain outcomes, and never claimed without data.

Scope: **Phase 0** (historical backtest) and **Phase 1** (live shadow — zero
transactions). Phase 2 (real filling) is parked.

This is research tooling. **Nothing here is financial advice.**

## What it does

- **Ingest** historical and live intents (Across, UniswapX), normalized to one
  `NormalizedIntent` type.
- **Route & quote** each intent (asset-class routing, Dutch decay, gas, confidence).
- **Decide** with a single `FillPolicy` (`would_fill`), applied identically in
  backtest and live so the answer never depends on the code path.
- **Validate** `would_fill` against realized outcomes (precision + margin drift).
- **Observe** in real time via a zero-dependency SSE dashboard, and tie every
  intent to a block-explorer transaction link so the numbers are verifiable.

## Quickstart (no API key needed)

```bash
export PATH="$HOME/.cargo/bin:$PATH"
cargo build --workspace

# See what will actually produce data before running anything:
./target/release/solver doctor

# No-key path (network only) → a populated, verifiable report:
./target/release/solver ingest uniswapx --days 14
./target/release/solver backtest --out-dir reports/run1
```

Live shadow (needs `ACROSS_WS_<CHAIN>` websocket RPC endpoints; without them the
source idles and says so):

```bash
cp .env.example .env   # fill in your endpoints; never commit .env
source .env
./target/release/solver live --chains base,arbitrum,optimism --sources across \
    --dashboard-port 8088 --duration 600 --out reports/live
# open http://127.0.0.1:8088
```

Live dashboard without duration:

```bash
source .env #must! otherwise - warnings.
./target/release/solver live --chains base,arbitrum,optimism --sources across --live-prices --dashboard-port 8088
```

## Architecture

A 6-crate workspace: `types` (data model), `ingest` (historical + live sources),
`pricing` (gas / sourcing), `router` (the quoting core), `backtest` (run + report +
calibration), `cli` (the `solver` binary, dashboard, doctor). See
`solver-engine-spec.md` for the design and `CLAUDE.md` for invariants.

## Verify

```bash
cargo build --workspace && cargo clippy --workspace -- -D warnings && cargo test --workspace
```

## License

Dual-licensed under MIT and Apache-2.0.
