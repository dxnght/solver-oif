#!/usr/bin/env python3
"""verify_fix.py — one-glance verdict on the decimals-bug + sanity-ceiling fix.

Reads one or more live decisions JSONL files and checks that the fix holds:
  * no would_fill with an implausibly large profit (the decimals-bug symptom),
  * the sanity ceiling / cross-class gate is demoting would-be garbage to suspect,
  * `unknown_decimals` skips are active (honest refusal to value unlisted decimals),
  * gas comes from real sources (etherscan/feehistory), not stale static.

Usage:
    python3 verify_fix.py reports/verify-fix/decisions-across-*.jsonl

Exit code 0 if clean, 1 if a RED FLAG is found (would_fill with |profit|>THRESHOLD).
"""
import sys, json, collections

# A would_fill above this (USD) is almost certainly a pricing artifact, not a
# real Across margin. Across relayer fees on typical $1k–$50k flow are cents to
# a few dollars; the decimals bug produced profit ≈ full input notional.
BIG = 50.0

paths = sys.argv[1:]
if not paths:
    print("usage: python3 verify_fix.py <decisions.jsonl> [more.jsonl ...]")
    sys.exit(2)

rows = []
for p in paths:
    try:
        with open(p) as f:
            for line in f:
                line = line.strip()
                if line:
                    rows.append(json.loads(line))
    except FileNotFoundError:
        print(f"  (skipping missing file: {p})")

if not rows:
    print("no decision records found — did the live run write to this path?")
    sys.exit(2)

wf = [r for r in rows if r.get("would_fill")]
prof = lambda r: r.get("expected_profit_usd") or 0.0

print(f"decisions: {len(rows)}    would_fill: {len(wf)}\n")

print("reason distribution:")
for reason, c in collections.Counter(r.get("reason", "?") for r in rows).most_common():
    print(f"  {c:6d}  {reason}")

print("\nconfidence distribution  (0.2 = sanity ceiling fired; 0.4 = cross-class; 0.9 = same-class fill):")
for conf, c in sorted(collections.Counter(round(r.get("confidence", -1) or -1, 2) for r in rows).items()):
    print(f"  {c:6d}  conf={conf}")

print("\ngas_source distribution  (want etherscan/feehistory; static = stale fallback):")
for gs, c in collections.Counter(r.get("gas_source", "(none)") for r in rows).most_common():
    print(f"  {c:6d}  {gs}")

print("\ntop would_fill by |profit|  (should all be SMALL — cents to low single/double $):")
for r in sorted(wf, key=lambda r: abs(prof(r)), reverse=True)[:10]:
    print(f"  ${prof(r):>13.4f}   conf={r.get('confidence')}   {r.get('route')}")

# --- The decisive checks ---------------------------------------------------
big_wf = [r for r in wf if abs(prof(r)) > BIG]
demoted = [r for r in rows if not r.get("would_fill") and abs(prof(r)) > BIG]
unk = sum(1 for r in rows if r.get("reason") == "unknown_decimals")
ceiling = sum(1 for r in rows if round(r.get("confidence", -1) or -1, 2) == 0.20)

print("\n" + "=" * 60)
print(f"[RED FLAG] would_fill with |profit|>${BIG:.0f}  (MUST be 0): {len(big_wf)}")
for r in big_wf:
    print(f"    !! ${prof(r):.2f}  {r.get('route')}  {r.get('verify_url')}")

print(f"\n[GOOD] demoted-to-suspect with |profit|>${BIG:.0f}  (ceiling/cross-class catching garbage): {len(demoted)}")
for r in sorted(demoted, key=lambda r: abs(prof(r)), reverse=True)[:8]:
    print(f"    ${prof(r):>12.2f}  conf={r.get('confidence')}  reason={r.get('reason')}  {r.get('route')}")

print(f"\n[INFO] unknown_decimals skips (active decimals guard): {unk}")
print(f"[INFO] confidence==0.20 rows (sanity ceiling fired):    {ceiling}")
print("       (0 for either just means no such token/route flowed in this window —")
print("        the unit tests are the definitive proof the logic is wired.)")
print("=" * 60)

if big_wf:
    print("\nVERDICT: RED — a would_fill still shows artifact-scale profit. Send these rows.")
    sys.exit(1)
print("\nVERDICT: GREEN — no artifact-scale would_fill. Fix holds on this window.")
sys.exit(0)
