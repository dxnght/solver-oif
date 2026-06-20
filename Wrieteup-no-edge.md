# No edge in zero-inventory cross-chain intent filling

*An honest backtest over 11,818 intents — and the structural reason the edge lives where a zero-capital solo can't reach.*

by **dxnght** · research note · source: `[https://github.com/dxnght/solver-oif]`

> Draft. The exact figures below are from a 14-day run; pin them to the final
> `metrics.json` before publishing. The qualitative conclusion is robust across
> several independent lenses and does not hinge on any single percentage.

---

## TL;DR

I built a cross-chain intent solver to fill long-tail L2↔L2 routes with zero
inventory, then tried to **falsify my own thesis** with a backtest validated
against realized on-chain outcomes. The result: **no edge.** Of the intents my
model flagged as profitable to fill, only ~12% would have cleared at my own
profit threshold on the terms that actually settled. Across **1,675 routes, only
2 ever produced a fill signal at all** — and on one of them the real winning
filler was *underwater on price* for all 31 of its fills.

The deeper finding is the useful one: **the edge in intent filling is not in
routing quality.** It lives in inventory, MEV, and order-flow access — none of
which a zero-capital solo has. The backtest didn't just say "no profit"; it
showed *why* there is none for someone in my position. I'm open-sourcing the
measurement methodology, because an honest negative result plus a reusable
harness is the part that lasts.

---

## The thesis I set out to test

Intents are now a settled DeFi market structure (UniswapX, CoW, Across — billions
in monthly volume), and the rails are open: **ERC-7683** gives one order format
and one settler interface, and the **Open Intents Framework** ships an
output-input separation model where the filler delivers the output first and is
reimbursed from the user's input second — in principle, fill with minimal
inventory.

The bet was specific: large solvers dominate dense single-chain spot via
inventory and latency, but **the long tail of L2↔L2 routes is served lazily**
(wide spreads). So the edge would be **routing quality** on those under-served
routes — out-quote the lazy incumbent, capture the spread, hold no inventory.

Plausible on paper. The job was to falsify it before spending a cent.

## How I measured it (and why the number is trustworthy)

The whole value of a backtest is whether you can believe it, so the discipline is
the point:

- **One definition of "fill."** A single `FillPolicy`
  (`confidence ≥ 0.5 AND expected_profit > $0.50`) is the *only* place a fill
  decision is made — identical in backtest and in the live shadow path. The
  answer never depends on which code path you're in.
- **Validated against realized outcomes, not just modeled.** For every intent I
  also compute the **realized** gross margin the *actual winning filler*
  accepted, from on-chain settlement data, and compare it to the model.
- **Two precisions, not one.** "Did this fill have *any* margin?" (realized > 0)
  is the lenient lens; "could *I* have profited at *my* threshold?"
  (realized net ≥ my margin) is the honest one. Reporting both is what exposes
  the over-optimism.
- **Net vs net, gross vs gross.** Modeled profit is net of my gas; the filler's
  price spread is gross. I subtract my own gas estimate from the realized side so
  the comparison is apples-to-apples — otherwise the gap looks smaller than it is.
- **Every number is verifiable.** Each fill row carries a click-through
  block-explorer link to the underlying transaction. No figure is asserted that a
  reader can't check.
- **No dishonest metrics.** `profitable_quote_rate` is never called a "win rate."
  There are no annualized projections. Margins below the gas-model error bar are
  treated as noise, not signal.

## Results

| | |
|---|---|
| Intents ingested (14 days) | **11,818** — Ethereum 8,195 · Base 2,599 · Arbitrum 1,023 · everything else ≈ 0 |
| Quoted by the router | 4,424 |
| Flagged `would_fill` by the model | 60 |
| Precision — *any* realized margin (> 0) | **48.3%** |
| Precision — *my* threshold (realized net ≥ margin) | **11.7%** |
| Modeled profit (sum over would_fill) | ≈ $17,904 *(modeled)* |
| Realized, same set | ≈ $1,709 — a ~10× overstatement |
| Worst single row | modeled **+$5,095** → realized **−$582** |
| Routes that *ever* produced a fill signal | **2 of 1,675** |

Two things jump out.

**The long tail I targeted had almost no flow.** Live intent volume concentrates
on Ethereum, Base, and Arbitrum; Optimism, Polygon, and the smaller L2s returned
essentially nothing over the window — not a bug, just real scarcity. So the
thesis fails on *both* sides at once: the **dense** routes have flow but no edge
(competition eats the spread), and the **long-tail** routes I was counting on
have no flow to capture.

**Even where the model fired, it was wrong ~7 times out of 8.** The 11.7%
threshold-precision is the real story: my model said "fill this profitably," but
only about one in eight of those would actually have cleared at my own bar. And
the two routes that produced *any* signal were Ethereum USDC↔USDT — the single
most competitive stablecoin pair on the most competitive chain — where on one
direction the winning filler was **underwater on price across all 31 fills**.

## The edge isn't in the routing

That last fact is the most important one in the whole study.

I measure **price** margin only (what the user paid in, minus what was delivered
out). If the *winning* filler consistently delivered more value than it received
— underwater on price, 31 times out of 31 — then a rational actor was filling at
a price loss on purpose. They weren't losing money; they were capturing value
**outside the price spread**: inventory they wanted anyway, MEV bundled with the
fill, rebates, or internalized flow.

Two honest reads follow, and both are useful:

1. **There is no price/routing edge on these routes — not even for the winners.**
   The premise I started from — "edge = routing quality on under-served routes" —
   is pointing at the wrong thing. Routing quality is not where the money is made.
2. **My price-only margin is a *lower bound* on the winner's real economics.**
   "Underwater on price" does not mean "lost money." It means the game is played
   on axes I can't see in price data, and can't replicate without capital.

Put together: **intent filling is won on inventory, MEV, and order-flow access —
not on routing.** A solo with zero inventory, no MEV infrastructure, and no flow
relationships has no edge *by construction*. That's a stronger conclusion than
"I couldn't find a profit." It's an understanding of **why** the profit isn't
reachable from where I'm standing — and it would not change with a better router.

## Limitations (because honesty is the whole point)

- **Small N.** Only 60 intents cleared the fill policy, so the 11.7% and 48.3%
  figures have wide confidence intervals — read them as "roughly one in eight" and
  "roughly half." The conclusion is robust because three *independent* lenses
  agree (threshold-precision, the ~10× modeled-vs-realized gap, and 2-of-1,675
  routes), not because any one percentage is precise.
- **UniswapX-dominated, Ethereum-heavy, mostly single-chain.** This run measures
  the flow that actually exists, which is concentrated and competitive. The
  cross-chain Across leg of the study is next; I expect it to confirm the same
  structural conclusion, but it isn't in this number yet.
- **Price-only realized margin, static reference prices.** Live gas and price
  feeds would tighten the model at the margin; they would not move a 7-in-8
  miss rate to a profit.

## Why a negative result is the deliverable

A clean falsification is rarer and more useful than another optimistic backtest:

- **The methodology is reusable.** A backtest that defines "fill" once, validates
  against realized outcomes, separates gross from net, and links every figure to
  on-chain proof is a harness anyone studying solver economics can run — in a
  space where backtests are often quietly dishonest and production solvers are
  closed.
- **The observability layer has standalone value.** Normalizing long-tail
  cross-chain intent flow into one model — routes, exclusivity capture,
  concentration — is something neither the big analytics dashboards nor the
  protocol-specific tools do for the long tail.
- **It's a public good.** "Here is exactly how I measured my own edge to ≈0, and
  here is the structural reason it's ≈0" is the kind of honest infrastructure work
  that ecosystem and intent-framework grant programs exist to fund.

The thesis is dead. The instrument that killed it — and the discipline behind it
— is the thing worth keeping.

---

*Built in Rust on ERC-7683 / OIF. Methodology, code, and the full per-route
breakdown (all 1,675 routes) are open at `[https://github.com/dxnght/solver-oif]`. Not financial advice;
this is research tooling.*
