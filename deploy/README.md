# Deploy — public intent-flow dashboard (beta)

Bring the live shadow + dashboard up as a public, TLS-terminated site on a
single VPS. **Tested live 2026-08-05 on Zomro (1 vCPU / 2.5GB RAM / 15GB disk,
Ubuntu 24.04)** — the smallest end of what this needs; Hetzner or any similar
Debian/Ubuntu box works the same way, with more headroom. Three layers:

- **BRESIL-1** — `solver-live.service`: the live loop + dashboard, 24/7, restart-on-crash.
- **BNET-1** — `Caddyfile`: the only public listener; auto-HTTPS → loopback:8088.
- **BDEPLOY-1** — `update.sh` / `just deploy`: pull → build → verify → restart.

**Security model:** the app binds `127.0.0.1:8088` only — never internet-facing.
Caddy (ports 80/443) is the sole public surface. Secrets (RPC URLs, Etherscan
key) live in `/opt/solver/solver.env` (chmod 600) and are read into the process
env; the dashboard serves only public decision records, never the keys. No auth,
because the data is public on-chain flow — that is the point.

---

## 1. One-time server setup

`solver` only ever *runs* the service — it never builds or deploys, so
`nologin` is correct for it and it needs no sudo rights of its own. Build and
deploy as **your own sudo-capable admin account** instead (below); only the
final `install`/`systemctl` steps need `sudo`, which your account already has.

```bash
# --- a dedicated unprivileged user + install root (runs the service only) ---
sudo useradd --system --shell /usr/sbin/nologin solver
sudo mkdir -p /opt/solver/bin /opt/solver/reports /opt/solver/data /var/log/caddy
sudo chown -R solver:solver /opt/solver/reports /opt/solver/data

# --- Rust toolchain, as YOUR admin account (build on the box) ---
curl https://sh.rustup.rs -sSf | sh -s -- -y
source "$HOME/.cargo/env"
rustup component add clippy   # update.sh runs the full /verify gate, needs this

# --- system C toolchain (rustup does NOT bring this) ---
# minimal Ubuntu images don't ship a linker; without it every build fails on
# `error: linker `cc` not found` at the first proc-macro crate.
sudo apt install -y build-essential pkg-config

# --- Caddy (Debian/Ubuntu) ---
sudo apt install -y debian-keyring debian-archive-keyring apt-transport-https curl
curl -1sLf 'https://dl.cloudsmith.io/public/caddy/stable/gpg.key' \
  | sudo gpg --dearmor -o /usr/share/keyrings/caddy-stable-archive-keyring.gpg
curl -1sLf 'https://dl.cloudsmith.io/public/caddy/stable/debian.deb.txt' \
  | sudo tee /etc/apt/sources.list.d/caddy-stable.list
sudo apt update && sudo apt install -y caddy

# --- firewall: only 80/443 public; 8088 stays loopback ---
sudo ufw allow 22,80,443/tcp
sudo ufw enable
```

## 2. Code + config

```bash
# clone as YOUR OWN account (adjust the URL/branch) — NOT the solver service user
git clone https://github.com/dxnght/solver-oif ~/solver-oif

# secrets: written by you, owned by the unprivileged runtime user
sudo cp ~/solver-oif/deploy/solver.env.example /opt/solver/solver.env
sudo $EDITOR /opt/solver/solver.env                # fill in keys
sudo chown solver:solver /opt/solver/solver.env
sudo chmod 600 /opt/solver/solver.env

# first build + binary
cd ~/solver-oif
# on a small box (1-2 vCPU), cap parallelism — the release profile is
# `codegen-units=1` + `lto=thin` (deliberately, for binary quality), which is
# memory-hungry; a few concurrent rustc/LLVM processes can OOM a 2-3GB VPS.
# Add swap too if RAM is under ~4GB (`fallocate -l 2G /swapfile && ...`).
export CARGO_BUILD_JOBS=1
cargo build --workspace --release
sudo install -m 0755 target/release/solver /opt/solver/bin/solver
rm -rf target   # debug+release build artifacts (a few GB) — only the
                # installed binary needs to persist, disk is often tight
```

## 3. Service + proxy

```bash
# systemd
sudo cp ~/solver-oif/deploy/solver-live.service /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable --now solver-live
journalctl -u solver-live -f          # confirm: "live shadow started", heartbeats

# sanity: dashboard answers on loopback
curl -s localhost:8088/healthz         # → ok

# Caddy: put your domain in the Caddyfile, then install + reload
sudo sed -i 's/intents.example.com/YOUR.DOMAIN/' ~/solver-oif/deploy/Caddyfile
sudo cp ~/solver-oif/deploy/Caddyfile /etc/caddy/Caddyfile
sudo caddy validate --config /etc/caddy/Caddyfile
sudo systemctl reload caddy
```

Point `YOUR.DOMAIN`'s A/AAAA record at the VPS IP first — Caddy provisions the
Let's Encrypt cert automatically on first request. Then open
`https://YOUR.DOMAIN` — live feed should stream within a minute of flow.

No domain yet? [sslip.io](https://sslip.io) resolves any `<name>.<ip-with
-dashes>.sslip.io` to that IP with zero registration — e.g. for
`45.131.182.254` use `solver.45-131-182-254.sslip.io` as `YOUR.DOMAIN` above.
Caddy gets a real Let's Encrypt cert for it exactly the same way. Good enough
for beta; swap in a real domain later with the same one-line `sed`.

If `reload` fails with `permission denied` on a log path — the `log { output
file ... }` block needs the `caddy` system user to own that directory
specifically (a `mkdir` done as root before installing the package can leave
it root-owned, and Caddy's graceful-reload-on-error means the OLD config
silently keeps running, so the failure is easy to miss). Simplest fix, and
what `deploy/Caddyfile` does now: `output stdout`, read via `journalctl -u
caddy` — no extra directory, no permission surface.

## 4. Updates (BDEPLOY-1)

On the server, as your own admin account (needs `sudo` for `install` and
`systemctl restart` — `solver` itself never runs this, it only runs the
service):

```bash
~/solver-oif/deploy/update.sh
```

Or from your laptop, one command after `git push`:

```bash
just deploy-remote host=you@your.vps
```

which just SSHes in and runs the same script (see the justfile).

---

## Known beta limits (honest)

- **Needs a redeploy to pick up POLL-1 (2026-08-18).** The live source was
  originally a WS `eth_subscribe` push; the 24/7 `solver-live.service` on
  this VPS running it against real Across traffic on 3 chains was enough to
  eat a meaningful share of the Infura free daily quota every night — an
  uncapped cost driven by chain activity, not by anything the service
  controls. `crates/ingest/src/live/poll.rs` replaces it with checkpointed
  `eth_getLogs` polling (default every 60s, `--poll-interval-secs`), a fixed
  request ceiling per chain. Needs `ACROSS_RPC_<CHAIN>` (already in
  `solver.env` for gas — no new secret), not `ACROSS_WS_<CHAIN>`/
  `OIF_WS_<CHAIN>` (delete those, no longer read). Verified locally against a
  real Base RPC endpoint; **not yet redeployed to this VPS** — `git pull &&
  update.sh` (or `just deploy-remote`) still needed.
- **`update.sh` doesn't yet install `build-essential`/`clippy`.** First-time
  server setup (§1 above) needs both; a redeploy on an already-provisioned
  box doesn't. Small gap, not yet closed — do §1 once by hand before the
  first `update.sh`/`just deploy-remote` run.
- **Aggregates recompute wholesale from disk (BAGG-1 done, not incremental).**
  `/aggregates` is recomputed from every `decisions-*.jsonl` under `--out` once
  at startup and every heartbeat (60s) — correct and restart-safe, but a full
  rescan, not an incrementally-maintained counter. Fine at this project's
  scale; would need revisiting if the decisions log ever got large.
- **Across-only.** OIF idle, UniswapX is retrospective-poll. Cross-chain Across
  flow is the MVP scope; `/about` says so.
- **Niche by design.** This is a developer/researcher/grant tool, not a
  high-traffic site; no CDN/rate-limit tuning needed yet (Caddy can add both
  later if it ever matters).
