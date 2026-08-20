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
(Газ-дефолт згодом оновлено в **GAS**: `live` бере драбину за замовчуванням, `--live-gas`
лишається опт-ін для backtest.)
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

**B6 — Розширення price-feed coverage. ✅ ЗРОБЛЕНО (COV-1..3).** Вузький, observability-only —
повнота дашборда, не пошук edge. CoinGecko platform-мапа доповнена `bsc`→binance-smart-chain,
`worldchain`→world-chain, `ink`→ink (кожен platform-id звірено з живим `/asset_platforms`
2026-06-23; `BSC`=56 — новий `ChainId`). Новий additive skip-reason `unsupported_chain`:
відсутню ціну тепер класифіковано за покриттям чейна — поза покриттям (нема verified-платформи:
не-EVM або ще не змаплений L2) → `unsupported_chain` («поза досяжністю»), покритий чейн із
незнайденим токеном → `price_unknown` («у досяжності, ціни нема»). Дашборд розрізняє їх (різні
кольори), % класифікованого потоку зростає. Не чіпали не-EVM і native-gas-мапу (поза скоупом).

**GAS — Real gas by default + source transparency. ✅ ЗРОБЛЕНО (GAS-1..5).** Вузький,
correctness+legibility. Газ тепер **драбина** etherscan `gasoracle` → `eth_feeHistory` →
static ([`LadderGasModel`]): `live` бере її **за замовчуванням** (лагодить «забув `--live-gas`»),
`--static-gas` форсить static; `backtest` лишається static-дефолт (`--live-gas` вмикає драбину),
`--reproducible` → static (детермінізм). Кожен `GasEstimate` несе `origin`
(`etherscan`/`feehistory`/`static`) → нове additive поле `gas_source` у `Quote` і
`DecisionRecord` (видно, якому числу вірити). Etherscan V2 `gasoracle` (той самий хост, що
ingest): `status!="1"` (чейн без покриття плану) / нема ключа / помилка → `None`, драбина падає
далі — ніякої вигаданої ціни. Звірено наживо 2026-06-24: ethereum/polygon/arbitrum → `status:1`,
optimism/base → «Free API access not supported» (парсер тестовано на реальних тілах). Polygon-газ
цінується в POL, не ETH (вже коректно; додано end-to-end-тест). `doctor` показує драбину per-chain.
Юніти (≈300k) свідомо **не** чіпали (правило #2); `$27`-проблема Ethereum — від статичної *ціни*,
драбина дає реальну (~sub-$1.5 за поточних gwei). Принагідно виправлено pre-existing fallback,
який недооцінював газ невідомих чейнів (9e12 < усі L2 → 6e13).

**HOV — Margin transparency. ✅ ЗРОБЛЕНО (HOV-1..3).** Дашборд-розширення поверх GAS: на комірці
profit — hover-тултип `Gross − Gas = Net` (gross = `expected_profit_usd + gas_usd`, газ позначено
джерелом), щоб модельне число було самопояснювальним; `gas_source == "static"` (статична підлога,
нема live RPC/oracle) позначено жовтим `⚠ static` з підказкою «set ACROSS_RPC_<chain>» — stale-газ
видно там, де він стається. HOV-1 (`gas_source` у `DecisionRecord`) уже зроблено в GAS-1.
Нема JS-харнеса → ідентичність `gross = net + gas` застрахована Rust-тестом роутера; JS звірено
`node --check`. Skipped-інтенти (null profit) — без тултипа.

**DEC — Decimals-чесність у valuation (`checked_decimals`). ✅ ЗРОБЛЕНО (DEC-1..2).** Вузький,
correctness. Валюація ноги **не** вгадує scale: збережений `AssetAmount.decimals` — ingest-
зручність із fallback 18, тож 6-dp токен, прочитаний як 18-dp, underflow-иться до ~0 і роздуває
профіт до повного notional (баг за абсурдними would_fill-рядками). **DEC-1** (роутер, shipped
`3381ca3`): `checked_decimals(token) → Option` (Some лише для впізнаних: native/stable/WETH/WBTC),
новий additive skip `unknown_decimals` — модельна нога зі невідомими decimals **скіпається**, не
квотиться. **DEC-2** (realized-нога, ця гілка): `NormalizedIntent::realized_gross_margin_usd` теж
через `checked_decimals` → `None` при невпізнаному токені (навіть якщо ціна є — live-фід дає її за
адресою). Сьогодні спожиток drift досягає цієї ноги лише для квотованих інтентів (ноги вже
впізнані), але метод публічний і бекає лічильник `with_realized` над **усіма** інтентами — тож
чесний сам по собі, не через upstream-скіп. Тести: real-адреси USDC (незалежна істина, не
плейсхолдер) + guard на priced-but-unrecognized realized-токен. Числа для впізнаних токенів
незмінні (stored == checked).

**BETA-1 — Ядро бети (публічно + надійно + чесно). ✅ ЗРОБЛЕНО й ПІДТВЕРДЖЕНО
НАЖИВО (2026-08-05).** П'ять атомарних під-buckets, щоб `solver live
--dashboard-port` стало реальним публічним сайтом, не лише localhost-демо.
Живий прогін виконано на VPS Zomro (1 vCPU / 2.5GB / 15GB, Ubuntu 24.04) —
менший бокс, ніж `deploy/README.md` початково мав на увазі (Hetzner), і саме
тому корисний тест: усе піднялось у цих межах, зі swap і `--jobs 1` на білді.
Публічна адреса: `https://solver.45-131-182-254.sslip.io/` (sslip.io —
безкоштовний, без реєстрації домену; заміниться на справжній домен для BETA-3).
`/healthz`, `/about`, `/aggregates` звірено ззовні; агрегати показали реальний
потік і перший живий `reason=fill` (не лише в бектесті).
- **BNET-1** — `deploy/Caddyfile`: єдиний публічний listener, авто-HTTPS
  (Let's Encrypt) → reverse-proxy на `127.0.0.1:8088` (застосунок ніколи не
  слухає назовні напряму); `/events` (SSE) не буферизується/не стискається
  (`flush_interval -1`), решта — так. Новий `GET /healthz` у `dashboard.rs`
  (bare `200 ok`, не залежить від наявності живих рішень — idle-потік не є
  «нездоровим»). Exit: HTTPS-доступ ззовні — **виконано, підтверджено наживо**:
  Caddy на Zomro-VPS видав Let's Encrypt-сертифікат автоматично, `/healthz`
  відповідає з реального домену (sslip.io), не лише з loopback.
- **BAGG-1** — персистентні 24h/7d агрегати. Новий модуль `cli/src/aggregates.rs`:
  повний перерахунок (не інкрементальний — вузький масштаб, правило #5) з усіх
  `decisions-*.jsonl` під `--out`, при старті `live` (до цикла) і на кожен
  heartbeat (60с). Новий `GET /aggregates`; дашборд полить його раз на 30с за
  перемикачем 24h/7d, з оптимістичним клієнтським bump між пулами (дзеркалить
  серверний `WindowAgg::bump`). Exit: рестарт не втрачає історію — **звірено
  наживо**: засіяно `decisions-*.jsonl` записами на «зараз» / «30г тому» /
  «10д тому», прогнано `solver live --dashboard-port`, `/aggregates` ДО
  першого нового рішення коректно дав 24h=1, 7d=2 (10-денний запис виключено).
- **BRESIL-1** — `deploy/solver-live.service`: unbounded (без `--duration`),
  `Restart=on-failure` + start-limit (5/60с, не молотить на зламаному ключі);
  WS-реконект (backoff) уже є в `live/ws.rs` — підтверджено тим самим прогоном
  (WARN → reconnect 1с→2с→4с). Hardening: `ProtectSystem=strict`,
  `ProtectHome`, лише `reports/`+`data/` writable. Exit: сервіс переживає
  падіння й обрив вузла — **виконано**.
- **BFRAME-1** — чесний банер (уже був) + новий `GET /about` (embedded, як
  dashboard.html — що це / чого це НЕ / як перевірити число / джерело+ліцензія)
  + `gas_source` тепер **завжди видимий** інлайн-тег (`es`/`fh`/`static`) на
  кожному quoted рядку, не лише в hover-тултипі (тільки `static` мав видимий
  прапорець раніше). Exit: рев'юер за 30с розуміє межі — **виконано**
  (звірено standalone node-assertion скриптом для `profitCell`, не лише
  `node --check`).
- **BDEPLOY-1** — `deploy/update.sh` (pull→build→`/verify`→install→restart) +
  `just deploy-remote host=...`. Виправлено реальний розрив у чернетці: адмін
  проксіювався через `sudo -u solver` (той — `nologin`, без власного sudo) →
  вкладені `sudo install`/`sudo systemctl` впали б без пароля/TTY. Модель
  виправлено: `solver` лише **запускає** сервіс (unprivileged, nologin, без
  sudo); білдить і деплоїть **адмінський** акаунт своїм реальним sudo. Також
  виправлено дрейф назви репо (`solver-engine` → `solver-oif`, звірено з
  `git remote -v`). Exit: `git push` → живий сайт — **виконано, реальний
  прогін на VPS 2026-08-05** (вручну, крок за кроком — не чистим одним
  `just deploy-remote`, бо перший прогін на свіжому образі вивів на поверхню
  прогалини, яких `update.sh`/README ще не покривали: відсутній
  `rustup component add clippy` і системний `build-essential`(`cc`) на
  мінімальному Ubuntu; `EnvironmentFile` у systemd не приймає `export VAR=...`
  (лише `VAR=...`, без shell-синтаксису); `deploy/Caddyfile` писав лог у файл
  і падав на `permission denied` при reload (виправлено → `output stdout`,
  закомічено). На 1-vCPU боксі варто `CARGO_BUILD_JOBS=1` і 2GB swap — інакше
  ризик OOM через `codegen-units=1`+`lto=thin` у релізному профілі. Ці фікси
  вже в репо (Caddyfile, deploy/README.md); `update.sh` сам ще не покриває
  build-essential/clippy-крок — окремий дрібний follow-up, не блокер.

**BETA-2 — Поліш і диференціація (у роботі, спланована 2026-08-05).** Ітерація
поверх живої BETA-1-бети. Третій пункт з початкового чернетки цієї фази
(«realized-drift фікс, `intent.rs:163`») виявився вже зробленим — це DEC-1/DEC-2
вище, чернетка застаріла в цьому місці. Стан трьох під-buckets: BPOLISH-1
запарковано як ідея, BPOLISH-2 відкритий/незакритий (рішення Власника —
нижче), BPOLISH-3 зроблено:

- **BPOLISH-1 — Exclusivity-тренд графік. Запарковано як ідея, не в
  main.** Прототип зроблений і перевірений на окремій гілці
  (`audit/bucket-bpolish-1`, 3 коміти, 8 тестів, живий ручний прогін) — периодичний
  снепшот у `reports/live/trend.jsonl`, `GET /trend`, inline SVG-спарклайн. Не
  змержено: цінність графіка для одного оператора під питанням, тому свідомо
  лишили як ідею, а не зробили частиною канону. Гілка живе локально,
  непушнута — підняти пізніше, якщо реально знадобиться.
- **BPOLISH-2 — Methodology-сторінка. НЕ ЗАКРИТО — рішення Власника: залишити
  як є, не чіпати цифри зараз.** `Wrieteup-no-edge.md` уже існує (позначено
  «Draft», TODO — «pin the exact figures to the final `metrics.json`»); `/about`
  зараз лише лінкує на нього в GitHub, не показує inline. Бакет: вбудована
  `GET /methodology` (той самий патерн, що `/about`/`about.html`) + допінути
  числа під фінальний BETA-0/BETA-1 живий прогін замість чернетки. Exit:
  методологія в один клік з дашборда, не «читай репо».

  **Блокер, знайдений при огляді:** headline-цифри в чернетці невірні —
  «Modeled profit (sum over would_fill) ≈ $17,904 → Realized ≈ $1,709»
  насправді є **одним найбільшим рядком** (`summary.md`, top-25 by modeled
  profit), а не сумою. Перерахунок з поточним кодом (`--data-dir` нижче) дав
  реальні цифри: сума modeled over would_fill — **$74,947.53**; реальна сума
  net-of-gas за той самий набір (`realized_net_usd_sum`) — **−$619.65**, тобто
  на net-основі набір **збитковий загалом**, а не просто «у 10 разів
  переоцінений» — сильніше твердження, ніж зараз у чернетці.

  **Рішення Власника (2026-08-06):** would_fill-похідні цифри легко переплутати
  (net vs gross, сума vs один рядок — саме на цьому спіткнулись зараз), тому
  свідомо **не вносимо** цю правку в `Wrieteup-no-edge.md` цим циклом. Пункт
  лишається відкритим/незакритим — не «зроблено», не «відхилено». Коли
  повернемось: `crates/backtest` тепер має `backtest --data-dir data/uniswapx
  --out-dir reports/b0-days14 --reproducible` для відтворення того самого
  14-денного набору (`data/across/*` з'явився пізніше й раніше туди не
  потрапляв) з повними calibration-полями поточного коду
  (`realized_margin_usd_sum`/`realized_net_usd_sum`) — цифри вище звідти,
  перевірити самостійно перед публікацією, не довіряти цьому підсумку наосліп.
- **BPOLISH-3 — Дайджест-сторінка. ✅ ЗРОБЛЕНО.** `GET /digest?window=24h|7d` —
  текстовий summary (топ-5 маршрутів, would_fill rate, exclusivity + зсув проти
  іншого вікна) з тих самих `WindowAgg`/`Aggregates`, що `aggregates.rs` уже
  рахує (нового джерела даних не додано). Нова `WindowAgg::exclusivity_pct()` —
  та сама формула, що клієнтський `renderAggregates` у `dashboard.html`
  (`exclusive_to_other / total * 100`), тепер спільна для дашборда й дайджеста.
  «Зсув» — 24h vs 7d (вікна, які вже є), не day-over-day: персистентної
  щоденної історії в репо нема (це BPOLISH-1, запарковано), тому чесніше
  порівняти наявні вікна й явно підписати їх, ніж імітувати тренд, якого дані
  не підтверджують. `dashboard.rs`: `DashboardHandle.aggregates` тепер тримає
  структурований `Aggregates` (не pre-serialized JSON) — `/aggregates` і
  `/digest` рендерять з одного снепшота без другого JSON round-trip;
  `digest.html` — той самий `include_str!`-шаблон, що `about.html`, з
  `{{BODY}}`/`{{ASOF}}`-підстановкою на кожен запит. Nav-лінки `/` ↔ `/digest`
  ↔ `/about`. Перевірено: 6 нових тестів (exclusivity_pct на fixture,
  digest-body на fixture з ручно порахованими відсотками, empty-state,
  HTTP round-trip до/після `set_aggregates`, `?window=7d`) + живий ручний
  прогін (`live --dashboard-port`, 353 реальних рядків з `beta1-confirm`):
  `/digest`, `/digest?window=7d`, `/aggregates` звірено через `curl` —
  49.6% exclusivity (175/353 exclusive_to_other) і 0.8% would_fill (3/353)
  зійшлись вручну; жодних `NaN`/незамінених `{{...}}`-плейсхолдерів.

**POLL-1 — Batch-poll live ingest замість WS push. ✅ ЗРОБЛЕНО (2026-08-18).**
Не нова ширина, а виправлення операційної вартості вже зробленого (B4/OIF-2):
живий VPS-сервіс (BRESIL-1, `solver-live.service`, 24/7, 3 чейни) генерував
`eth_subscribe`-нотифікації в темпі реального Across-депозитного трафіку —
некерований обсяг (~9200 інтентів/добу на самому лише Across), достатній,
щоб щоночі з'їдати істотну частку безкоштовної Infura-квоти. Причина
знайдена через живий `/aggregates` дашборда, не здогадка.

Оскільки `would_fill` зважується виключно постфактум (правило #6 — модель,
не факт до валідації) і система нічого не робить у момент транзакції,
секундна свіжість push-підписки не давала жодної практичної переваги перед
контрольованим polling. Новий `crates/ingest/src/live/poll.rs`: `eth_blockNumber`
(тік) → `eth_getLogs` за `[checkpoint+1, min(tip, checkpoint+2000)]` →
checkpoint на диску (`reports/live/checkpoint-<source>-<chain>.json`,
restart-safe, без реплею історії — той самий «тільки що станеться далі», що
й WS). `--poll-interval-secs` (дефолт 60с, новий прапорець `solver live`) —
фіксована стеля `2 × чейни × (86400/interval)` запитів на добу, незалежна
від реального ончейн-трафіку. Кожен RPC-фейл — гучний `WARN` + ретрай на
наступний тік (нема fatal-шляху, на відміну від WS `eth_subscribe rejected`
— усі HTTP-помилки тут за визначенням транзієнтні).

`AcrossLive`/`OifLive` тепер резолвлять **той самий** `ACROSS_RPC_<CHAIN>`,
що вже existed для `eth_feeHistory`-рунги газової драбини (LP-1..6/GAS) —
не окремий `ACROSS_WS_<CHAIN>`/`OIF_WS_<CHAIN>`: один сконфігурований
ендпоінт на чейн тепер живить три речі одразу (Across-поллінг, OIF-поллінг,
живий газ), і сервер, де вже стояв `ACROSS_RPC_*` для газу, **не потребує
нового секрету** для редеплою. `solver doctor` перероблено відповідно
(`across_rpc_env`/`oif_rpc_env`, «RPC-SET/RPC-MISSING», «READY on N/M chains
(poll)») — той самий legibility-контракт (правило #7), нові дані.

Перевірено: 39 нових/оновлених тестів у `solver-ingest` (wire-shape
`eth_getLogs`/`eth_blockNumber`, `parse_get_logs`/`parse_block_number` на
**реальних** committed-фікстурах Across/OIF (той самий підхід, що WS-ери
тести — не тавтологія, правило #3), checkpoint round-trip через диск,
`next_range`-межі), `solver doctor` тести переписані під новий env-контракт
— увесь workspace `/verify` зелений. Живий ручний прогін проти реального
Base RPC (`ACROSS_RPC_BASE` з `.env`): коректний baseline на tip, і на
другому тіку — реальний Base-депозит знайдено через `eth_getLogs`,
нормалізовано, прогнано крізь спільний `decide()`
(`across:base:chain:4663:6024439`, `reason=unsupported_chain`, чесний скіп),
записано в JSONL; checkpoint коректно просунувся. **Не задеплоєно на VPS** —
код і `deploy/`-доки готові (`solver-live.service`/`solver.env.example`
оновлені, старі `*_WS_*`-змінні документовано як більше не читані), рестарт
сервісу лишається за людиною.

**RETAIN-1 — Автоматична ротація `decisions-*.jsonl`. ✅ ЗРОБЛЕНО (2026-08-18).**
POLL-1 закрив некерований RPC-кошт 24/7-сервісу; цей bucket закриває
дзеркальний ризик для диска — той самий VPS (1 vCPU/2.5GB/15GB) без чистки
накопичує `decisions-*.jsonl` нескінченно. Новий `crates/cli/src/retention.rs`:
`prune_old_decisions(dir, today, retention_days)` видаляє цілі
день-ротовані файли старші за поріг (без часткового обрізання — не треба,
раз ротація вже щоденна); `checkpoint-<src>-<chain>.json` (POLL-1) ніколи не
матчиться (інший префікс/розширення) і переживає будь-яку чистку. Новий
прапорець `solver live --retention-days` (дефолт **9**), перевіряється на
кожному heartbeat (та сама 60с-каденція, що й BAGG-1-агрегати — дешево на
цьому масштабі, rule #5). `0` = зберігати назавжди (явний opt-out, не
випадковість).

**Чому 9, не рівно 7:** дашбордове вікно `/aggregates`'s 7d читає
`observed_at` **всередині** файлів, не назву файлу — тож видалення файлу за
день D безпечне, лише якщо жоден рядок у ньому вже не потрапляє у 7д-вікно.
Оскільки файл — це цілий UTC-день, needed retention = 7 (вікно) + 1
(сьогоднішній частковий день) = **8** мінімум, щоб вікно ніколи не
обрізалось мовчки; 9 — +1 запас. `MIN_SAFE_RETENTION_DAYS = 8` експортовано;
якщо оператор виставить нижче — гучний `WARN` на старті («7d window may be
truncated»), не тиха розбіжність цифр (rule #7).

Перевірено: 5 нових тестів (`file_day` на реальних іменах файлів включно з
checkpoint-колізією, prune на реальній файловій системі — старий видалено,
свіжий і checkpoint лишились, `retention_days<=0` нічого не чіпає, відсутня
директорія — no-op) + живий ручний прогін (`solver live --retention-days 5`
проти реального `ACROSS_RPC_BASE`): попереджувальний WARN про поріг
з'явився одразу, застарілий файл (дата 2020-01-01) видалено на першому ж
heartbeat, checkpoint лишився недоторканим — увесь workspace `/verify`
зелений (35 тестів у `solver-cli`, було 30).

**Тільки після B0–B6 — повертатись до нової ширини** (PATH-JOIN/FEED/OIF/нові чейни).

## Беклог (заморожено до verified core)

- **OIF-1 — `InputSettlerEscrow`-адреса. ✅ ЗРОБЛЕНО (2026-08-06).** Нове
  `crates/ingest/src/oif/deployments.rs` (той самий патерн, що
  `across/spoke_pool.rs`): `INPUT_SETTLER_ESCROW =
  0x1CC9260E285C2C8AC8D2E7102F3978056Ec1d0a8`, звірено **трьома незалежними
  джерелами** — (1) офіційний `openintentsframework/oif-contracts`
  `DEPLOYMENTS.md`, (2) `eth_getCode` через публічні RPC на
  ethereum/base/arbitrum/optimism (непорожній байткод, ідентичний між чейнами
  крім ~1 immutable-слова — узгоджено з CREATE2-деплоєм одного контракту, не
  чотирма різними), (3) [Sourcify](https://sourcify.dev) (незалежний
  bytecode↔source верифікатор) — `runtimeMatch`/`creationMatch` обидва
  `exact_match` → `InputSettlerEscrow.sol:InputSettlerEscrow`. `OIF_CHAINS`
  містить лише ці 4 самостійно перевірені чейни; `DEPLOYMENTS.md` заявляє
  більше (Polygon/BSC/Linea/Scroll/zkSync/Katana), але цитата доку — не
  верифікація (правило #2), тож решта лишається `None`/PENDING, доки не
  перевіримо самі.
- **OIF-2 — живий адаптер. ✅ ЗРОБЛЕНО (2026-08-06).** `crates/ingest/src/oif/`
  тепер має `event.rs` (типи + `Open` topic0) і `live.rs` (`OifLive` — той
  самий `LiveSource`/`ws::stream_logs`-транспорт, що `AcrossLive`, лише
  адреса/topic0/decode інші). `solver live --sources across,oif` мерджить
  обидва джерела в один потік (`crates/cli/src/live.rs`: `Vec<Box<dyn
  LiveSource>>`, кожне переслане у спільний `mpsc`-канал); ім'я вихідного
  файлу — з реально відкритих джерел (`decisions-across+oif-<day>.jsonl`).
  `solver doctor` показує per-chain OIF WS-готовність тим самим блоком, що
  Across (`OIF_WS_<CHAIN>` + `input_settler_escrow`-звірка).

  **`Open`-topic0 — чотири незалежні звірки** (більше, ніж у Across
  `FilledRelay`, бо `StandardOrder` вкладає два динамічні масиви): (1)
  `sol!`-похідний `SIGNATURE_HASH` проти `cast keccak` канонічного підпису
  вручну з `InputSettlerEscrow.sol`/`StandardOrderType.sol`/
  `MandateOutputType.sol`; (2) той самий hash проти `topics[0]` **реального**
  `Open`-логу з Base (tx `0x504e78d9…`, блок 47245610), знятого
  `eth_getTransactionReceipt` з публічного RPC — не Etherscan; (3) `cast
  abi-decode` цього ж логу тим самим tuple-типом дав осмислені реальні
  значення ще до жодного Rust-коду: input-токен `0x833589fc…` = Base USDC,
  output settler `0x52602D7c…` (за назвою — ймовірно `OutputSettlerSimple`,
  але **не** верифіковано незалежно: OIF-1 перевірив лише
  `InputSettlerEscrow`, деталі в `deployments.rs`), output-токен
  `0x3c499c54…` = Polygon USDC; (4) Blockscout
  незалежно задекодував той самий лог і дав ідентичну tuple-форму. Фікстура
  (`testdata/open_base_usdc_to_polygon.json`) і всі чотири звірки — тести в
  `oif::event`.

  **Скоуп першого заходу:** `StandardOrder` дозволяє кілька input/output
  ніг; `NormalizedIntent` моделює рівно одну з кожного боку. Замість
  вгадувати, яка нога «рахується» в multi-leg ордері, `normalize_open`
  нормалізує лише single-input/single-output (форма, в якій був реальний
  захоплений ордер) і **скіпає** (голосний `WARN`, не тиха втрата даних) усе
  інше — чесний PENDING-стан (правило #2), не здогадка. Також перевіряє
  `order.originChainId` проти підписаного чейна (ловить помилково
  налаштований `OIF_WS_<CHAIN>` на не той нетворк). `settlement:
  ResourceLock` (поле в `solver_types::SettlementType` уже мало коментар
  «OIF resource-lock» — співпало без змін). `exclusivity: None` — на відміну
  від Across немає верифікованого exclusive-relayer-поля в `StandardOrder`
  (можливо кодується в `MandateOutput.context`, але ця семантика не
  перевірена, тож чесно `None`, не здогадка).

  Перевірено: 9 нових тестів у `crates/ingest/src/oif/` (topic0 × 2 звірки,
  повний decode реального логу з ручно звіреними полями, `normalize_open` на
  тому самому реальному ордері з усіма полями `NormalizedIntent`, mismatch-
  guard) + `doctor`-тести (symmetric Across/OIF WS-readiness) +
  живий ручний прогін `solver live --sources across,oif --chains base` з
  фіктивними `*_WS_BASE` (обидва джерела коректно `open`, обидва намагаються
  конектитись, обидва бекоф'зять незалежно; окремо перевірено `--sources
  oif` самостійно, `--sources bogus` (гучний WARN), і дефолт без `--sources`
  (лишається тільки across, як і було)) — увесь workspace `/verify` зелений,
  139 тестів.
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
