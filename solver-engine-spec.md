# solver-engine-spec — канонічна ідея (§0–§10)

> Канонічний опис **ідеї** руху. Консолідує вже зафіксований у `PROJECT-STATUS.md`
> тезис і архітектуру з `CLAUDE.md §Архітектура` в один файл, на який посилається код
> (`solver-engine-spec.md §N`). **Правда понад амбіцію:** секції описують або
> **реалізоване** (з посиланням на крейт), або явно позначене `PENDING`. Якщо код і
> спека розійшлися — це баг документації, виправляти негайно (CLAUDE.md правило #4).
>
> Деталі стану — `PROJECT-STATUS.md`; плани з ID — `ROADMAP.md`; незмінні правила —
> `CLAUDE.md`.

## §0 — Тезис і чесний статус

Крос-чейн інтент-движок на **ERC-7683 / OIF**. Початкова комерційна гіпотеза —
заробляти філлінгом на long-tail L2↔L2 з $0 inventory — **сфальсифікована**: чесний
крос-чейн edge ≈ 0 (бар'єри: аванс інвентаря, латентність, конкуренція об'ємом).

Тому це **вимірювальна/observability-інфраструктура + дослідницький артефакт**, НЕ
джерело доходу. Головний deliverable — **чесність вимірювання**. Перший наскрізний
прогін це підтвердив емпірично: на static-цінах would_fill на волатильних маршрутах
давав фантомний edge ($1.18M «прибутку», 94% «precision») — артефакт static-цін;
після прибирання фальшивої впевненості залишились лише same-class маршрути з
тверезим would_fill_precision ≈ 43% (модель оптимістична проти realized).

## §1 — Скоуп і фази

- **Фаза 0** (реалізовано): історичний backtest. `ingest uniswapx → backtest → звіт`.
- **Фаза 1** (`PENDING`): live shadow — нуль транзакцій, лише спостереження/лог.
- **Фаза 2** (припарковано): реальний філлінг. Не починати без капіталу/інфраструктури.

Дисципліна: **verified-core-first, консолідація перед шириною**. Нове джерело/чейн/фіча
— по одному, з exit-критеріями, після того як попереднє працює наскрізно й перевірювано.

## §2 — Типи й нормалізація (крейт `types`)

Один словник для гетерогенних джерел. Ключові типи (всі реалізовано):

- `NormalizedIntent` — крос-джерельний вигляд інтенту: `input`/`output_min`,
  `output_decay`, `deadline`, `observed_at`, `winning_filler`, `realized_output`,
  `exclusivity`, `tx_ref`, `raw`. Після нормалізації решта движка не знає про джерело.
- `AssetAmount` / `TokenRef` — повний `U256` + decimals; `to_f64()` для статистики.
- `asset_class()` → `AssetClass {Stable, Eth, Btc, Unknown}` — за адресними таблицями
  (стейбли/WETH/WBTC); основа для static-цін і confidence.
- `ChainId` — `u64` + best-effort `name()`/`explorer_tx_url()` (verified majors).
- `Quote` — вихід роутера; `expected_profit_usd` — **єдине** поле для USD-агрегації.
- `FillPolicy` / `FillReason` — див. §7.
- `SettlementType` — див. §6.
- `TxRef` — on-chain tx, що підпирає інтент; per-source мапінг chain. Див. §9.

## §3 — Інгест (крейт `ingest`)

Кожне джерело: (1) тягне upstream API, (2) мапить у `NormalizedIntent`, (3) стрімить
JSONL (`store::JsonlSink`, append-only) для backtest.

- **UniswapX historical** (реалізовано): `GET /v2/orders?orderStatus=filled`,
  Dutch V2/V3, пагінація курсором, defensive serde. `deadline` у payload часто `null`
  → береться з `decayEndTime` (cosigner/top-level), інакше інтент виглядав би
  простроченим. Single-chain домінує; cross-chain поле `destinationChainId`.
- **Across historical / live, OIF, інші** (`PENDING`): додаються по одному (B2+), з
  сигнатурами подій з канонічного ABI, не з пам'яті. OIF лишається idle, доки
  settler-адресу не звірено on-chain (PROJECT-STATUS).

## §4 — Прайсинг і економіка маршруту (крейт `pricing`, `router::prices`)

Питання роутера: *скільки коштує дістати `output_min` токена Y на чейні C і скільки
газу на fill-tx?*

- `GasModel` / `StaticGasModel` (реалізовано): per-chain оцінка wei для ~300k-газового
  fill. Live `eth_feeHistory` — `PENDING` (B3, за прапорцем, дефолт off).
- `Pricer` / `OfflineIdentityPricer` (реалізовано): buy == sell, нуль газу —
  детермінований backtest. Live odos — `PENDING`.
- `PriceFeed` / `StaticPriceFeed` (реалізовано): стейбли = $1 (реально near-peg),
  ETH/BTC = **позначені MODELED reference-припущення**, native — per-chain; невідоме →
  `None` (скіп, не вигадувати ціну). Live-фід — `PENDING` (B3).

**Економіка філлера (USD):** `expected_profit_usd = input·price_in − output_min·price_out
− gas_usd`. На static-цінах надійна лише для **same-class** (стейбл↔стейбл); крос-клас
несе немодельований DEX-slippage + ризик неправильної static-ціни волатильного активу.

### §4.1 — Across-exclusivity та прийнятність маршруту

`ExclusivityWindow`: поки `now < deadline` і ми не призначений relayer — інтент не
вільно-фільмований. RouterV0 у такому разі скіпає (`SkipReason::ExclusiveToOther`), щоб
не ловити on-chain revert. Для backtest оцінка йде **на момент відкриття аукціону**
(коли exclusivity завершується), бо ми міряємо прибутковість потоку, а не winnability.

## §5 — Роутер: серце edge (крейт `router`)

**RouterV0** (реалізовано): `NormalizedIntent → QuoteOutcome` (`Quoted(Quote)` або
`Skipped(SkipReason)`).

- Liveness-гейти: `Expired`, `ExclusiveToOther`, `DustAmount` (див. §4.1).
- Ціни обох сторін + dest-газ у USD; будь-яке невідоме → `PriceUnknown` (скіп).
- `confidence` — наскільки віримо `expected_profit_usd`: same-class `0.90`; крос-клас
  на static-цінах `0.40` — **навмисно нижче** `FillPolicy.min_confidence`, тож такі
  маршрути ніколи не would_fill, доки нема live-фіда (явне прибирання фальшивої
  впевненості). Крос-клас усе одно квотується (міряти економіку — робота роутера).
- `effective_output_at` — лінійна Dutch-decay інтерполяція (integer, clamped).
- Рішення про fill роутер **не** ухвалює — це `FillPolicy` (§7), один на live і backtest.

## §6 — Backtest, ризик сетлменту, калібрування (крейт `backtest`)

Потік: `load` → `metrics` → `report`.

- `load` (реалізовано): рекурсивний `*.jsonl`, дедуп за `IntentId`, дроп
  unknown-chain; кожен рядок облікований (kept+dup+malformed+dropped). Чистити вхід
  ДО виміру (CLAUDE.md правило #9).
- `metrics` (реалізовано): RouterV0 + `FillPolicy` над інтентами; лічильники рішень,
  `profitable_quote_rate` (**не** win rate), і **would_fill-калібрування** проти
  realized — `would_fill_precision = confirmed / would_fill_with_realized`, де confirmed
  = realized gross-margin > 0. `None`, коли нема чого звіряти (не вигадувати число).
- `report` (реалізовано): `summary.md` (всі USD позначені MODELED; would_fill поряд з
  realized; таблиця fills з клікабельними verify-links — `—`, не вигаданий URL) +
  `metrics.json`. Порожній прогін — гучний, з наступною командою.
- `dest_summary` (`PENDING`).

**Ризик сетлменту** (`SettlementType`) — головна вісь фінансового ризику філлера:
`OptimisticOracle` (challenge-вікно, капітал під ризиком), `OffchainAttestation`
(довіра атестору), `ResourceLock` (escrow, найнижчий ризик). Впливає на реалістичність
backtest і на майбутній sizing.

## §7 — `would_fill` (єдине визначення)

`would_fill ≡ confidence ≥ FillPolicy.min_confidence AND expected_profit_usd >
FillPolicy.min_margin_usd`, де `expected_profit_usd = revenue − (sourcing + gas)`.
**`FillPolicy` — ЄДИНЕ джерело цього рішення; live і backtest дають однакову відповідь.**

`would_fill` — **модельна віра**, стає довірою лише через валідацію проти realized
(§6). Ніколи не подавати як факт прибутку.

## §8 — Легібельність (`cli doctor`)

`solver doctor` (реалізовано): read-only, без мережі/мутацій — каже ДО прогону, які
джерела/чейни реально дадуть дані. uniswapx=DATA-OK (без ключа), across=idle (адаптер
B2), oif=idle (адреса UNVERIFIED, не пробуємо), etherscan-v2=unused до B2. Гучний
верхній рядок; `--suggest` друкує мінімальну робочу команду. Тиха відмова заборонена
(CLAUDE.md правило #7). Live V2-проба — `PENDING` (B2, з Across-шляхом, що її споживає).

## §9 — Дисципліна verify-links

`TxRef → explorer_tx_url` лише для verified-majors доменів; невідомий чейн або
непридатний хеш → `None`, **ніколи** вигаданий URL. Кожен fill у звіті несе клікабельне
посилання (клік → block-explorer → звірити цифри) або `—`. Зразок чесної роботи.

## §10 — Роадмеп і відкриті питання

Порядок: **B0** (verified core — цей наскрізний цикл) → **B1** (наповнити would_fill-
калібрування даними) → **B2** (Across поточні події) → **B3** (live gas/prices, тоді
крос-клас confidence стає feed-aware). Тільки потім — нова ширина (FEED/PATH/OIF).
Деталі та application-ідеї (observability long-tail L2 тощо) — `ROADMAP.md`.

Відкриті питання: життєздатність observability-ніші (звірити Dune); робочість
хардкоднутої OIF-адреси (звірити on-chain); перелік чейнів з реальним потоком і
безкоштовними даними (`doctor`).
