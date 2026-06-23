# ROADMAP — Solver Engine

Плани з ID. Принцип: **verified-core-first**, bucket-driven, **консолідація перед
шириною**. Кожен напрям продає **видимість і дисципліну** (чого надлишок), а не
швидкість/капітал (чого бракує). Контекст — `PROJECT-STATUS.md`, правила — `CLAUDE.md`.

## Як ми працюємо (нова дисципліна)

**Small agile buckets, по одному.** Bucket = вузька, закінчувана робота з: метою,
точними file-цілями, exit-критеріями, `/verify` після кожного коміту, scope-guard
(«чого НЕ робити»). Не починати наступний, поки поточний не зелений і не
перевірюваний. **Без горизонтального росту на неперевіреному фундаменті.**

## Зараз / найближчі buckets (за пріоритетом)

**B0 — Verified core (фундамент перед усім).**
Один source + кілька чейнів наскрізно й перевірювано:
- `solver doctor` — preflight: що сконфігуровано, який чейн дасть дані (Etherscan V2 /
  WS), що idle. Гучно, до прогону. (Лікує «feels broken».)
- No-key цикл зелений: `ingest uniswapx → backtest` дає непорожній звіт із
  verify-links (клікнути → block-explorer → звірити цифри).
- `cargo build/clippy/test` зелені (компілятор — істина).
Exit: `doctor` показує правду; no-key цикл дає перевірюваний звіт; CI зелений.

**B1 — would_fill calibration (серце довіри).**
Тримати й наповнювати даними: `would_fill_precision` + margin-drift проти realized.
Exit: на реальному backtest видно число «з N сказаних fill, K ринок підтвердив».

**B2 — Across historical ingest (крос-чейновий observability-шар). ✅ ЗРОБЛЕНО (AX-1..7).**
Поточне покоління `FilledRelay` (bytes32) з канонічного ABI (`@across-protocol/contracts`),
**не з памʼяті**; topic0 звірено з on-chain-логом; SpokePool-адреси з канонічних
deployments (мейджори `Some`, решта `None`+PENDING). `ingest across` через Etherscan-V2
`getLogs` пише інтенти з realized/filler/tx_ref; `doctor` дає реальну per-chain V2-готовність.
Exit (виконано): backtest включає Across, would_fill-калібрування рахується крос-чейн.
Свідомо **поза** скоупом (окремі buckets): deposits-скан + JOIN depozit↔fill (PATH);
старе покоління `FilledV3Relay`; live-WS.

**B3 — Live gas/prices. ✅ ЗРОБЛЕНО (LP-1..6).**
`eth_feeHistory`-газ по чейну (`ACROSS_RPC_<CHAIN>`) + CoinGecko-ціни за контракт-адресою
(long-tail fix), **за прапорцем** `--live-prices`/`--live-gas`, fallback static + WARN,
дефолт off (детермінізм CI); `--reproducible` перекриває на static. `doctor` показує
live-pricing-готовність (CoinGecko ping + per-chain RPC). Наживо: `--live-prices` зняв 171
long-tail-інтент із `price_unknown` (quoted 6597→6768); `--live-gas` здешевив L2-газ
(would_fill 100→105). Платформ-id CoinGecko та topic0/RPC-формати звірено з живими джерелами.
Крос-клас confidence лишається консервативно нижче порога (feed-aware тюнінг — окремо).

**B4 — Live Across shadow. ✅ ЗРОБЛЕНО (LIV-1..6).**
WS `eth_subscribe` на `FundsDeposited` (origin-депозити, **не** `FilledRelay` — це open-side
рестрім), спільний `decide()` (один `FillPolicy` у live і backtest), decisions JSONL із
verify_url на origin-tx, нуль транзакцій; `doctor` показує WS-готовність. topic0 звірено
з реальним on-chain-логом.

**B5 — Live observability dashboard. ✅ ЗРОБЛЕНО (DASH-1..4).**
Zero-dep tokio HTTP/SSE-сервер (без web-фреймворку/білд-кроку): `GET /` віддає вбудований
(`include_str!`) `dashboard.html`; `GET /events` стрімить кожен `DecisionRecord` як SSE-подію
через `broadcast`-канал + replay-буфер (новий клієнт бачить свіжий потік, не порожній екран).
За прапорцем `--dashboard-port` у `solver live` — публікує **той самий** decision-потік у
broadcast **на додачу** до JSONL (без дублювання `decide`-логіки); без прапорця B4 незмінна.
Наратив — exclusivity-capture (`exclusive_to_other` % від усіх) і склад потоку (топ
origin→dest-маршрути / dest-чейни / reason-breakdown / live-feed із клікабельними verify-лінками),
**не** would_fill (edge≈0 — чесно підписано). Інтеграційні тести на ефемерному порту (без
браузера): `GET /` віддає HTML; `GET /events` — `text/event-stream` і прокидає опублікований
`DecisionRecord` (replay + live-broadcast). Реалізує application-ідею #1 (публічний intent-flow
дашборд). Ще не прогнано проти живого WS-фіда (потрібні `ACROSS_WS_<CHAIN>`).

**Тільки після B0–B5 — повертатись до нової ширини** (PATH-JOIN/FEED/OIF/нові чейни).

## Беклог (заморожено до verified core)

- OIF live (Open-подія): **спершу підтвердити адресу InputSettler on-chain** —
  поточна хардкоднута адреса неверифікована (див. PROJECT-STATUS). Не довіряти, поки
  не звірено. Адаптер тримати `Idle`, доки нема підтвердження.
- FEED (UniswapX real-time): webhook / filled-poll (open-poll емпірично мертвий).
- PATH (крос-чейн кореляція Across): детермінований JOIN на `(originChainId, depositId)`.
- Активація PENDING-чейнів: SpokePool/settler-адреси **лише з живих docs**, по одному,
  з верифікацією; без адреси → `return None` + `PENDING`.

## Ідеї застосування (монетизація/гранти) — ранжовано за ROI/зусилля

Усі переспрямовують наявні активи: нормалізований intent-flow, decision-движок,
whitebox-edge-математику, **вимірювальну дисципліну**.

**#1 — Intent-flow observability на long-tail L2 (НАЙВИЩИЙ ROI).** Публічний дашборд:
потік/маршрути/exclusivity-частка по чейнах, які великим аналітикам нецікаві. Покупець
конкретний: ecosystem-команди малих L2 (хочуть видимості свого потоку) + Uniswap
research / Across. **Перед ставкою — звірити Dune на конкурентів** (Across має власний
Dune-дашборд; диференціація — крос-протокольна нормалізація long-tail).

**#2 — Honest-measurement методологія (репутаційний public good, $0).** Відкритий
backtest-harness + writeup «як ми методично рознесли власний headline до edge≈0».
Унікальний контент (solver-робота закрита, backtest-и часто нечесні) → репутація →
гранти/контракти. Вже наполовину написано в audit-доках.

**#3 — Тренувальний симулятор солвера (освітній).** Пісочниця проти історичних даних;
whitebox-математика рідкісна. Uniswap Foundation фінансує onboarding/освіту.

**#4 — Exclusivity / value-leakage монітор.** Агрегація частки потоку, замкненого
інкумбентами. Grant-bait, дешево, вузько.

## Гранти (non-dilutive, $0-сумісно; усі вимагають KYC — західні фонди)
- **Uniswap Foundation** — найкращий фіт (cross-chain routing + data research + tooling).
- **Base Builder Grants** — ретроактивні (спершу відвантаж, тоді помітять).
- **EF ESP** — security/infra/research ≤ $30k.
- **Optimism Retro Funding / Superchain missions** — public goods + ERC-7683 місії.
- Across/Risk Labs — перевірити прямо (формальної програми не підтверджено).

## Відкриті питання
- Чи існує життєздатна observability-ніша на long-tail → перевірити Dune + поговорити
  з 1–2 малими L2.
- Чи робоча хардкоднута OIF InputSettler-адреса → звірити on-chain (B-беклог).
- Поточний перелік чейнів з реальним потоком і доступними безкоштовними даними → `doctor`.

## Анти-цілі (чого НЕ робити — оплачено помилками)
- НЕ комерціалізувати філлінг без капіталу (перевірено, edge≈0).
- НЕ вигадувати адреси/ABI/факти; неверифіковане → `None` + `PENDING`.
- НЕ писати тавтологічні тести й фальшиві «verified via …» коментарі.
- НЕ рости горизонтально на неперевіреному фундаменті.
- НЕ плодити постійні сесійні доки (дрейф); канон — лише файли з `CLAUDE.md §Доки`.
- НЕ робити annualized-проєкцій; would_fill (модель) ≠ win (realized).
