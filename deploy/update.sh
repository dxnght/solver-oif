#!/usr/bin/env bash
# update.sh — pull, build, and restart the live service ON THE SERVER (BDEPLOY-1).
#
# Rust builds are heavy, so we build on the VPS rather than cross-compile. Run
# as a user who can sudo systemctl, from the repo checkout:
#   ./deploy/update.sh
#
# Assumes: repo at ~/solver-engine (adjust REPO), install root /opt/solver,
# binary at /opt/solver/bin/solver, service `solver-live`.

set -euo pipefail

REPO="${REPO:-$HOME/solver-engine}"
INSTALL="${INSTALL:-/opt/solver}"

echo "==> updating $REPO"
cd "$REPO"
git pull --ff-only

echo "==> building release (this can take a while)"
export PATH="$HOME/.cargo/bin:$PATH"
cargo build --workspace --release

echo "==> /verify gate before shipping"
cargo clippy --workspace -- -D warnings
cargo test --workspace

echo "==> installing binary → $INSTALL/bin/solver"
sudo install -m 0755 target/release/solver "$INSTALL/bin/solver"

echo "==> restarting service"
sudo systemctl restart solver-live
sleep 2
sudo systemctl --no-pager --lines=10 status solver-live || true

echo "==> done. tail logs:  journalctl -u solver-live -f"
