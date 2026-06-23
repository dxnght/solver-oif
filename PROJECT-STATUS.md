# PROJECT-STATUS — Solver Engine

Знімок стану для нового контексту/чату/контриб'ютора. **Почни звідси**, потім
`CLAUDE.md` (канон коду + правила) і `ROADMAP.md` (плани).

> Це **чистий рестарт**, інформований аудитом попередньої реалізації. Архітектура
> переноситься (вона добра); сесійний дрейф і фабриковані факти — ні.

## Одним абзацом

Крос-чейн інтент-движок (ERC-7683/OIF, Rust, 6 крейтів + live + дашборд). Початкова
комерційна гіпотеза — філлінг на long-tail L2↔L2 з $0 inventory — **сфальсифікована**
(edge ≈ 0). Проєкт свідомо у режимі **observability / дослідження / портфоліо /
грантовий тулінг**. Скоуп: Фаза 0 (backtest) + Фаза 1 (live shadow, нуль транзакцій).
Головна цінність — **вимірювальна дисципліна**.

## Чому не комерціалізуємо філлінг (зафіксоване рішення)
JIT ≠ $0 капіталу (філлер авансує output, рефанд з лагом); UniswapX L2 permissionless,
але бар'єр **економічний** (інвентар/латентність/конкуренція); CoW під DAO-bonding pool
— лише KYC, але новачок заробляє копійки; головні маршрути — маржа 2–8 bps, з'їдена
об'ємом. Висновок: цінність — **демонстрація дисципліни + грантовий артефакт**, не дохід.

## Що працює (B0 verified core — реалізовано й перевірено наскрізно)
Чистий рестарт пройшов наскрізний no-key цикл `ingest uniswapx → backtest → звіт`:
- **6-крейтова основа зелена**: `build + clippy -D warnings + test` (61 тест,
  поведінкові/property, не тавтологічні). Компілятор — істина.
- **RouterV0** — `NormalizedIntent → Quote/Skip`: економіка філлера в USD,
  asset-class confidence, Dutch-decay, `QuoteOutcome`/`SkipReason`.
- **Єдиний `FillPolicy`** — «fill» означає одне в live і backtest (один код-шлях).
- **would_fill-калібрування — наповнене реальними даними**: на ~1090 живих UniswapX-
  інтентах would_fill_precision ≈ **43%** (модель оптимістична проти realized).
- **Калібрування поглиблене (B1, WFC-5..8)**: margin-drift рахується **net↔net**
  (realized_net = realized − наш газ) із розподілом (перцентилі), **дві** precision
  (`>0` = чи була маржа взагалі; `≥ our min_margin` = чи могли б ми бути в плюсі),
  per-route розбивка (відповідь спеці §3) і top-10 worst-drift із verify-links —
  усе в `summary.md` + `metrics.json`. Над-оптимізм тепер інспектований, не лише
  усереднений.
- **Verify-links** — `TxRef` → block-explorer; лише верифіковані домени, `None`/`—` для
  невідомих. У звіті кожен fill несе клікабельний tx-лінк.
- **`solver doctor`** — read-only preflight: статика без мережі + **реальна Etherscan-V2
  per-chain проба** для Across (DATA-OK / NO-V2 / NEEDS-KEY), каже, що дасть дані до прогону.
- **Across historical ingest (B2)** — крос-чейновий observability-шар: поточне покоління
  `FilledRelay` (bytes32) з канонічного `@across-protocol/contracts`-ABI; topic0 звірено
  з реальним on-chain-логом; SpokePool-адреси з канонічних deployments (5 мейджорів `Some`,
  решта `None`+PENDING). `ingest across` пише інтенти з наповненими
  `realized_output`/`winning_filler`/`tx_ref`; backtest тепер рахує **would_fill-калібрування
  крос-чейн** (drift ≈ похибка static-входів — очікувано, бо маржа фіксована депозитом, без
  Dutch-аукціону). Free Etherscan-ключ покриває не всі чейни — `doctor` каже це гучно.

**Емпіричний урок (записано в spec §0):** на static-цінах would_fill на **волатильних**
маршрутах давав фантомний edge ($1.18M «прибутку», 94% «precision») — артефакт
static-ціни (та ще й тавтологічної: модель і realized-звірка ділять ту саму хибну ціну).
Прибрано: крос-клас confidence опущено нижче порога fill → would_fill лише на same-class,
де $1-peg реальний. Результат тверезий (precision ≈43%), що й підтверджує edge≈0.

## Що під підозрою / зламано (з аудиту)
- 🔴 **OIF InputSettler-адреса хардкоднута й НЕВЕРИФІКОВАНА** (`live/oif/deployments.rs`):
  коментар посилається на `DEPLOYMENTS.md`, якого в репо OIF немає; «тест» —
  тавтологія. **Не довіряти, поки не звірено on-chain.** Тримати OIF idle.
- ✅ **Across-події (історичні) на поточному поколінні** — закрито в B2: ingest читає
  `FilledRelay` (bytes32) з канонічного ABI. ✅ **live-WS-адаптер — закрито в B4**
  (`eth_subscribe` на `FundsDeposited`-депозити). Лишається: JOIN depozit↔fill
  (PATH, окремий bucket).
- 🟡 **Дрейф ширини**: додано FEED/PATH/OIF замість консолідації (doctor/B2). Заморозити.
- **Компіляція не підтверджена незалежним аудитом** (потрібні build/test-логи).

## Найближче (порядок із ROADMAP)
1. ✅ **B0 — verified core**: `doctor` + no-key цикл із verify-links + зелений CI — **зроблено**.
2. ✅ **B1 — would_fill-калібрування поглиблене**: net↔net margin-drift + розподіл,
   дві precision, per-route summary, top-10 worst-drift — **зроблено** (WFC-5..8).
   (`dest_summary` — окремо, за потреби.)
3. ✅ **B2 — Across historical ingest + Etherscan-V2 проба в `doctor`**: поточне покоління
   `FilledRelay`, калібрування побігло крос-чейн — **зроблено** (AX-1..7).
4. ✅ **B4 — live Across shadow**: WS `eth_subscribe` на `FundsDeposited` (origin-депозити),
   спільний `decide()` (один `FillPolicy` у live і backtest), decisions JSONL із verify_url,
   нуль транзакцій; `doctor` показує WS-готовність — **зроблено** (LIV-1..6).
5. ✅ **B3 — live gas/prices**: CoinGecko-ціни за контракт-адресою (long-tail) +
   `eth_feeHistory`-газ (`ACROSS_RPC_<CHAIN>`), за прапорцем `--live-prices`/`--live-gas`,
   fallback static, дефолт off (детермінізм CI), `--reproducible` перекриває — **зроблено**
   (LP-1..6). Наживо: `--live-prices` зняв 171 long-tail-інтент із `price_unknown`
   (quoted 6597→6768); `--live-gas` здешевив L2-газ (would_fill 100→105). Крос-клас
   confidence лишається консервативно нижче порога (feed-aware тюнінг — можливий наступний крок).
6. ✅ **B5 — live observability dashboard**: zero-dep tokio HTTP/SSE-сервер за прапорцем
   `--dashboard-port` у `solver live` — стрімить **той самий** `DecisionRecord`-потік
   (broadcast + replay-буфер) у вбудований vanilla-JS дашборд; наратив — exclusivity-capture
   і склад потоку (маршрути/dest-чейни/reason/feed із verify-лінками), не would_fill;
   без прапорця B4 незмінна — **зроблено** (DASH-1..4). Реалізує application-ідею #1.
   Ще не прогнано проти живого WS-фіда (`ACROSS_WS_<CHAIN>`).
7. ✅ **B6 — розширення price-feed coverage** (вузький, observability-only): CoinGecko
   platform-мапа +bsc/world-chain/ink (platform-id звірено з живим `/asset_platforms`;
   `ChainId::BSC`=56 додано), новий additive skip-reason `unsupported_chain` — відсутню ціну
   класифіковано за покриттям чейна (поза покриттям → `unsupported_chain`, покритий чейн без
   ціни токена → `price_unknown`) — **зроблено** (COV-1..3). Не чіпали не-EVM / native-gas.
8. Тільки тоді — нова ширина (PATH-JOIN, FEED, OIF після on-chain-підтвердження адреси).

## Чого НЕ робити (застереження)
- Не вигадувати адреси/ABI/факти; неверифіковане → `None` + `PENDING`.
- Не писати тавтологічні тести / фальшиві «verified via …» коментарі.
- Не комерціалізувати філлінг без капіталу (перевірено).
- Не рости горизонтально на неперевіреному фундаменті.
- Не плодити постійні сесійні доки (дрейф); `/verify` після кожного коміту; не пушити.

## Стек і середовище
Rust 1.90 (edition 2021, resolver 2). `export PATH="$HOME/.cargo/bin:$PATH"`. Мережа в
sandbox обмежена (crates.io/github/npm дозволені). Білд — на машині розробника або
через Claude Code; **компілятор — джерело істини**. Env — `.env.example` → `.env`
(не комітити).
