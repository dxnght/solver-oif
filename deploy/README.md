# Deploy — public intent-flow dashboard (beta)

Bring the live shadow + dashboard up as a public, TLS-terminated site on a
single VPS (tested target: Hetzner). Three layers:

- **BRESIL-1** — `solver-live.service`: the live loop + dashboard, 24/7, restart-on-crash.
- **BNET-1** — `Caddyfile`: the only public listener; auto-HTTPS → loopback:8088.
- **BDEPLOY-1** — `update.sh` / `just deploy`: pull → build → verify → restart.

**Security model:** the app binds `127.0.0.1:8088` only — never internet-facing.
Caddy (ports 80/443) is the sole public surface. Secrets (RPC/WS URLs, Etherscan
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
cd ~/solver-oif && cargo build --workspace --release
sudo install -m 0755 target/release/solver /opt/solver/bin/solver
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
sudo systemctl reload caddy
```

Point `YOUR.DOMAIN`'s A/AAAA record at the VPS IP first — Caddy provisions the
Let's Encrypt cert automatically on first request. Then open
`https://YOUR.DOMAIN` — live feed should stream within a minute of flow.

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

- **Aggregates are in-memory (BAGG-1 pending).** Top-routes / exclusivity % are
  computed from flow since the process started; a restart resets them until
  BAGG-1 adds persistent 24h/7d rollups. `Restart=on-failure` keeps the process
  up for long stretches, so this bites only on deploys.
- **Across-only.** OIF idle, UniswapX is retrospective-poll. Cross-chain Across
  flow is the MVP scope; the "coverage" note on the page should say so.
- **Niche by design.** This is a developer/researcher/grant tool, not a
  high-traffic site; no CDN/rate-limit tuning needed yet (Caddy can add both
  later if it ever matters).
