# CLAUDE.md — Solver Engine

Канон для агента (Claude Code) і людини в цьому репо. **Тримати коротким, свіжим,
правдивим.** Цей файл описує ФАКТИЧНЕ дерево коду й незмінні правила. Якщо згадка не
відповідає коду — це баг документації, виправляти **негайно** (інакше дрейф, який
уже одного разу нас покусав).

> Це чистий рестарт. Архітектура взята з перевіреної (вона добра), а правила нижче
> запікають **уроки попередньої реалізації** — щоб ті самі помилки не повторились.

## Що це

Крос-чейн інтент-движок на **ERC-7683 / OIF**. Канонічна ідея — `solver-engine-spec.md`.

**Чесний статус (не амбіція, а виміряний факт):** початкова комерційна гіпотеза —
заробляти філлінгом на long-tail L2↔L2 з $0 inventory — **сфальсифікована**: чесний
крос-чейн edge ≈ 0 (бар'єри: аванс інвентаря, латентність, конкуренція об'ємом). Тому
проєкт — це **вимірювальна/observability-інфраструктура + дослідницький артефакт +
портфоліо + потенційний грантовий тулінг**, НЕ джерело доходу з $0.

Скоуп: **Фаза 0** (історичний backtest) + **Фаза 1** (live shadow, нуль транзакцій).
Фаза 2 (реальний філлінг) — **припаркована**, не починати без капіталу/інфраструктури.

Головна цінність репо — **чесність вимірювання**. Вона ж — головний deliverable.

## Архітектура (цільова, чиста)

Rust workspace, 6 крейтів + live-підсистема + дашборд. Кожен крейт — одна
відповідальність:

| Крейт | Відповідальність |
|---|---|
| `types` | `NormalizedIntent`, `Quote`, `AssetAmount`, `ChainId`, `FillPolicy`, `SettlementType`, `TxRef` |
| `ingest` | historical (`across/`, `uniswapx/`) + live (`live/` — `LiveSource` trait, `poll` transport (checkpointed `eth_getLogs`), shared; `across/`'s and `oif/`'s own live adapters) + `oif/` (`deployments`, `event`, `live`) → нормалізація в один формат |
| `pricing` | газ (`gas`/`fee_history_gas`), сорсинг (`odos`), offline |
| `router` | **серце edge** (`v0`): asset-class routing, decay, confidence, `QuoteOutcome`/`SkipReason`; ціни (`prices`/`live_prices`) |
| `backtest` | `load` (dedup+hygiene), `metrics` (+ would_fill-калібрування), `report`, `dest_summary` |
| `cli` | бінар `solver`: модулі `main`, `live`, `dashboard` (+ `dashboard.html`, `about.html`, `digest.html`), `aggregates`, `doctor`. Сабкоманди: `ingest`, `backtest`, `live`, `doctor` |

Потік: discovery → eligibility → router(quote) → **FillPolicy** (bid-рішення) →
[Фаза 1: shadow-лог] / [Фаза 2: executor — припарковано] → settlement-watcher.

## `would_fill` — визначення (одне на весь репо)

`would_fill ≡ confidence ≥ FillPolicy.min_confidence AND
expected_profit_usd > FillPolicy.min_margin_usd`, де `expected_profit_usd = revenue −
(sourcing + gas)` на static- або live-цінах. **`FillPolicy` — ЄДИНЕ джерело цього
рішення; live і backtest зобов'язані давати однакову відповідь.**

`would_fill` — це **модельна віра**. Вона стає **довірою** лише через валідацію проти
realized (`would_fill_precision`, margin-drift). Ніколи не подавати `would_fill` як
факт прибутку.

## Команди

```bash
export PATH="$HOME/.cargo/bin:$PATH"

# /verify — після КОЖНОГО коміту (toolchain: edition 2021, rust-version 1.90):
cargo build --workspace && cargo clippy --workspace -- -D warnings && cargo test --workspace

# Легібельність ПЕРЕД будь-яким прогоном — що реально дасть дані:
./target/release/solver doctor

# No-key золотий шлях (лише мережа) — перший перевірюваний успіх:
./target/release/solver ingest uniswapx --days 14
./target/release/solver backtest --out-dir reports/<run>

# Live shadow (потрібен ACROSS_RPC_*; без нього джерело idle — і каже про це):
./target/release/solver live --chains base,arbitrum,optimism --sources across,oif \
    --dashboard-port 8088 --duration 600 --out reports/live
```

Env — у `.env.example` (скопіюй у `.env`, **`.env` ніколи не комітити**).

## Незмінні правила / уроки (НЕ регресувати — кожен оплачений болем)

1. **Чесність вимірювання — deliverable.** Не заявляти edge без даних. Research-
   backtest, не торгова порада. Жодних annualized-проєкцій.
2. **НІКОЛИ не фабрикувати факти.** Адреси контрактів, ABI, signature-hash подій,
   explorer-URL — **тільки** з верифіковного живого джерела, з **реальною**
   перевіркою (on-chain або fetch під час білда). Неверифіковане → `return None` +
   `PENDING`, **ніколи** плейсхолдер-hex, **ніколи** впевнений коментар.
   *(Урок: вигадана OIF-адреса з фальшивим посиланням на неіснуючий `DEPLOYMENTS.md`.)*
3. **Жодних тавтологічних тестів.** Тест мусить звіряти з **незалежною** істиною
   (on-chain, фікстура знята з реальності, property). Порівняння константи зі стрічкою
   тієї ж константи — 0 гарантії й маскує ризик. Якщо коментар каже «verified via X» —
   X має бути **реально** перевірено.
4. **Один канонічний доку, нуль дрейфу.** Доки живуть у репо, єдине джерело істини,
   оновлюються з кожною зміною. Doc ≠ code → баг, виправити зараз. **Не плодити
   сесійні рунбуки** як постійні артефакти (вони → `.claude/` тимчасово, не в канон).
5. **Консолідація перед шириною. Verified-core-first.** Одна річ працює наскрізно
   й **перевірювано** перш ніж додавати наступне джерело/чейн/фічу. Small agile
   buckets, по одному, з exit-критеріями. **Заборонено** горизонтальний ріст на
   неперевіреному фундаменті. *(Урок: FEED/PATH/OIF додані замість doctor/AE.)*
6. **`would_fill` — зміряний, не вірований.** «Fill» визначається один раз
   (`FillPolicy`); валідується проти realized.
7. **Легібельність — first-class.** Система мусить казати, що сконфігуровано і що
   реально дасть дані, **до** прогону (`doctor`). Тиха відмова **заборонена** —
   idle/error гучні. *(Урок: «subscription confirmed, але тиша» / «feels broken».)*
8. **Компілятор — істина, не «на папері».** `/verify` після кожного коміту.
9. **Чистити вхід ПЕРЕД виміром.** Дедуп за `IntentId` + chain-гігієна на load.
   `realized_output`/`winning_filler` — ground-truth. USD vs token-units не плутати
   (агрегація читає ТІЛЬКИ `expected_profit_usd`).
10. **Робочий процес:** атомарні коміти `area(scope): summary (ID)`; ID стабільні, не
    перенумеровувати; гілка `audit/<topic>`. **Не пушити — людина пушить.** Ніколи не
    чіпати `.env`/секрети/`.claude/` без явної інструкції. **Один змінений файл — один
    у відповіді**, не zip. `profitable_quote_rate` ≠ «win rate» (зарезервовано).

## Структура даних і артефактів
Вхід: `data/<source>/*.jsonl` (append-only — дедуп на load). Звіти: `reports/<run>/`.
Live: `reports/live/{intents,decisions}-<src>-<day>.jsonl`. JSONL-схема — durable:
нові поля **додавати**, не перейменовувати мовчки.

## Доки (канон, не сесійні)
- `solver-engine-spec.md` — канонічна ідея (§0–§10).
- `PROJECT-STATUS.md` — поточний стан, звідки починати (для нового чату/контриб'ютора).
- `ROADMAP.md` — плани з ID (buckets, ідеї застосування, відкриті питання).
- `README.md` — публічний опис (English).
- Сесійні рунбуки/аудити → `.claude/` як **тимчасові**; не цитувати з коду як канон.
