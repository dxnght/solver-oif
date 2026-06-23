//! Live USD price feed (CoinGecko), behind the `--live-prices` flag.
//!
//! The Phase-0 [`StaticPriceFeed`] only knows a hand-pinned set (stables → $1,
//! ETH/BTC reference, native per chain); every other ERC-20 returns `None`, so
//! the Router skips the intent with `price_unknown`. On the real cross-chain
//! stream that is *most* of the long-tail flow. [`LivePriceFeed`] resolves a
//! token by **contract address + chain** via CoinGecko, which is exactly what
//! covers that long tail — the token gets a real price instead of a skip.
//!
//! Honesty rules baked in (CLAUDE.md #2):
//!   - A **network/parse error** never produces `price_unknown` where the
//!     static feed already has an honest answer: we fall back to the static
//!     value and `warn!`. We never invent a number.
//!   - A token CoinGecko genuinely does not price (and the static feed doesn't
//!     either) stays `None` — an honest `price_unknown`, not a fabricated price.
//!   - Platform ids and native coin ids are **verified** against CoinGecko's
//!     own `/asset_platforms` (see [`coingecko_platform`]); an unverified chain
//!     is left unmapped rather than guessed.
//!
//! The [`PriceFeed`] trait is synchronous (and fixed — we do not change it), so
//! a live HTTP GET is bridged to async via a resolver. In production the
//! resolver runs reqwest under `tokio::task::block_in_place`; tests inject a
//! resolver returning canned bodies, so the parse/fallback logic is exercised
//! with **no network and no runtime** (mirrors the doctor Across-probe pattern).

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use solver_types::{ChainId, TokenRef};
use tracing::warn;

use crate::prices::{PriceFeed, StaticPriceFeed};

/// Per-token cache TTL. CoinGecko's free tier is rate-limited; a 60s memo keeps
/// a busy live stream (or a backtest re-pricing the same few tokens) well under
/// the cap, and bounds how stale a quote's price can be. We cache the *resolved*
/// value (live or fallback) so a network outage doesn't re-hammer the API.
const CACHE_TTL: Duration = Duration::from_secs(60);

/// CoinGecko public REST base.
const COINGECKO_BASE: &str = "https://api.coingecko.com/api/v3";

/// Synchronous HTTP-GET resolver: URL → response body, or a network/parse
/// error. Injected so tests drive the feed with canned bodies (no network), the
/// same dependency-injection the doctor Across probe uses.
pub type PriceResolver = Box<dyn Fn(&str) -> anyhow::Result<String> + Send + Sync>;

/// CoinGecko asset-platform id for `chain` — the `{platform}` path segment of
/// `/simple/token_price/{platform}`. `None` for any chain not verified against
/// CoinGecko `/asset_platforms`, so we never query a guessed platform id
/// (rule #2); the static fallback still answers for those.
///
/// Verified 2026-06-23 by fetching `/asset_platforms` and matching each id's
/// `chain_identifier`: ethereum=1, polygon-pos=137, arbitrum-one=42161,
/// base=8453, optimistic-ethereum=10.
fn coingecko_platform(chain: ChainId) -> Option<&'static str> {
    Some(match chain {
        ChainId::ETHEREUM => "ethereum",
        ChainId::POLYGON => "polygon-pos",
        ChainId::ARBITRUM => "arbitrum-one",
        ChainId::BASE => "base",
        ChainId::OPTIMISM => "optimistic-ethereum",
        _ => return None,
    })
}

/// CoinGecko coin id of `chain`'s **native** gas token, for
/// `/simple/price?ids={id}`. Only the ETH-native chains are mapped (`ethereum`
/// verified live → real USD on 2026-06-23). Polygon's native is deliberately
/// left `None`: the `matic-network` id returned an empty body in verification,
/// so we defer to the static MATIC value rather than ship a mapping we watched
/// fail. The live-shadow chains (base/arbitrum/optimism) are all ETH-native.
fn coingecko_native_id(chain: ChainId) -> Option<&'static str> {
    Some(match chain {
        ChainId::ETHEREUM
        | ChainId::OPTIMISM
        | ChainId::ARBITRUM
        | ChainId::BASE
        | ChainId::UNICHAIN => "ethereum",
        _ => return None,
    })
}

/// Outcome of one live lookup, kept distinct so the fallback policy is explicit:
/// a genuine "not priced" (Unknown) defers quietly to static, while a network
/// failure (Error) defers *and* warns — the two must not be conflated.
enum Live {
    Price(f64),
    Unknown,
    Error(String),
}

/// Live CoinGecko price feed with an honest static fallback.
pub struct LivePriceFeed {
    /// Static feed used as the fallback (and the source of the env price
    /// overrides). Never lets a network failure downgrade a known token to
    /// `price_unknown`.
    fallback: StaticPriceFeed,
    resolver: PriceResolver,
    cache: Mutex<HashMap<TokenRef, (Instant, Option<f64>)>>,
}

impl LivePriceFeed {
    /// Production feed: fallback seeded from the optional `PRICE_ETH_USD` /
    /// `PRICE_BTC_USD` overrides (else the same reference values the static feed
    /// uses, so behaviour is unchanged when they are unset), and a reqwest GET
    /// resolver bridged to the current tokio runtime.
    pub fn from_env() -> Self {
        let eth = env_f64("PRICE_ETH_USD").unwrap_or(3000.0);
        let btc = env_f64("PRICE_BTC_USD").unwrap_or(100_000.0);
        let fallback = StaticPriceFeed::new(eth, btc, 0.50);
        let client = reqwest::Client::new();
        let resolver: PriceResolver = Box::new(move |url: &str| http_get_blocking(&client, url));
        Self::with_resolver(fallback, resolver)
    }

    /// Construct with an explicit fallback + resolver — the test seam (inject a
    /// canned-body or always-error resolver to exercise parse/fallback offline).
    pub fn with_resolver(fallback: StaticPriceFeed, resolver: PriceResolver) -> Self {
        Self { fallback, resolver, cache: Mutex::new(HashMap::new()) }
    }

    fn cached(&self, token: TokenRef) -> Option<Option<f64>> {
        let cache = self.cache.lock().expect("price cache poisoned");
        cache
            .get(&token)
            .and_then(|(t, v)| (t.elapsed() < CACHE_TTL).then_some(*v))
    }

    fn store(&self, token: TokenRef, value: Option<f64>) {
        self.cache
            .lock()
            .expect("price cache poisoned")
            .insert(token, (Instant::now(), value));
    }

    /// One live lookup: build the URL (native vs ERC-20), call the resolver,
    /// pull `usd` out of the response under the right key. Any unmapped chain or
    /// missing price is `Unknown`; a resolver error is `Error`.
    fn fetch_live(&self, token: TokenRef) -> Live {
        let (url, key) = if token.is_native() {
            let id = match coingecko_native_id(token.chain) {
                Some(id) => id,
                None => return Live::Unknown,
            };
            (
                format!("{COINGECKO_BASE}/simple/price?ids={id}&vs_currencies=usd"),
                id.to_string(),
            )
        } else {
            let platform = match coingecko_platform(token.chain) {
                Some(p) => p,
                None => return Live::Unknown,
            };
            // CoinGecko keys the response by the lowercased contract address;
            // `{:#x}` gives `0x…` lowercase, matching both the query and the key.
            let addr = format!("{:#x}", token.address);
            (
                format!("{COINGECKO_BASE}/simple/token_price/{platform}?contract_addresses={addr}&vs_currencies=usd"),
                addr,
            )
        };
        let body = match (self.resolver)(&url) {
            Ok(b) => b,
            Err(e) => return Live::Error(e.to_string()),
        };
        match parse_usd(&body, &key) {
            Some(p) if p > 0.0 => Live::Price(p),
            _ => Live::Unknown,
        }
    }
}

/// Runtime selection between the static and live price feeds. Lets the Router
/// (and the backtest/live loops) hold **one** concrete `PriceFeed` type whether
/// or not `--live-prices` is set — an enum dispatch, no trait objects and no
/// 2×N monomorphization of the decision loop.
pub enum PriceSource {
    Static(StaticPriceFeed),
    Live(LivePriceFeed),
}

impl PriceSource {
    /// Short label for the `prices=…` summary line.
    pub fn label(&self) -> &'static str {
        match self {
            PriceSource::Static(_) => "static",
            PriceSource::Live(_) => "live-coingecko",
        }
    }
}

impl PriceFeed for PriceSource {
    fn usd_price(&self, token: TokenRef) -> Option<f64> {
        match self {
            PriceSource::Static(f) => f.usd_price(token),
            PriceSource::Live(f) => f.usd_price(token),
        }
    }
}

impl PriceFeed for LivePriceFeed {
    fn usd_price(&self, token: TokenRef) -> Option<f64> {
        if let Some(v) = self.cached(token) {
            return v;
        }
        let resolved = match self.fetch_live(token) {
            Live::Price(p) => Some(p),
            // CoinGecko has no price for this token: the static feed may still
            // know it (a stable → $1); otherwise an honest `None`.
            Live::Unknown => self.fallback.usd_price(token),
            // Network/parse failure: never `price_unknown` where static knows.
            Live::Error(e) => {
                warn!(token = %token, error = %e,
                    "live price fetch failed — falling back to static feed");
                self.fallback.usd_price(token)
            }
        };
        self.store(token, resolved);
        resolved
    }
}

/// Pull `body[key].usd` out of a CoinGecko simple-price response. Pure (no
/// network) so it is unit-tested against the **real** response shapes captured
/// on 2026-06-23. `None` when the key or `usd` is absent (token not priced).
fn parse_usd(body: &str, key: &str) -> Option<f64> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    v.get(key)?.get("usd")?.as_f64()
}

/// Parse a float env var, treating blank/garbage as absent.
fn env_f64(name: &str) -> Option<f64> {
    std::env::var(name).ok()?.trim().parse().ok()
}

/// Bridge the synchronous `PriceFeed::usd_price` to async reqwest. The trait is
/// fixed (CLAUDE.md forbids changing it), so we run the GET on the current
/// runtime via `block_in_place`. Requires the multi-threaded runtime the CLI
/// uses (`#[tokio::main]`); tests never reach here (they inject a resolver).
fn http_get_blocking(client: &reqwest::Client, url: &str) -> anyhow::Result<String> {
    use anyhow::Context;
    tokio::task::block_in_place(|| {
        tokio::runtime::Handle::current().block_on(async {
            let resp = client.get(url).send().await.context("coingecko GET")?;
            let text = resp.text().await.context("coingecko body")?;
            Ok(text)
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{address, Address};

    const BASE_USDC: Address = address!("833589fCD6eDb6E08f4c7C32D4f71b54bdA02913");

    fn tok(chain: ChainId, a: Address) -> TokenRef {
        TokenRef::new(chain, a)
    }

    /// `parse_usd` reads the **real** CoinGecko bodies captured on 2026-06-23:
    /// a token_price (lowercased-address key) and a native simple/price (coin-id
    /// key). Independent truth (the live payloads), not a constant vs itself.
    #[test]
    fn parse_usd_reads_real_coingecko_shapes() {
        let token_body = r#"{"0x833589fcd6edb6e08f4c7c32d4f71b54bda02913":{"usd":0.999674}}"#;
        assert_eq!(
            parse_usd(token_body, "0x833589fcd6edb6e08f4c7c32d4f71b54bda02913"),
            Some(0.999674)
        );
        let native_body = r#"{"ethereum":{"usd":1711.39},"matic-network":{}}"#;
        assert_eq!(parse_usd(native_body, "ethereum"), Some(1711.39));
        // A queried-but-unpriced id (empty object) and a missing key → None.
        assert_eq!(parse_usd(native_body, "matic-network"), None);
        assert_eq!(parse_usd(r#"{}"#, "ethereum"), None);
    }

    /// A long-tail ERC-20 the static feed does NOT know gets a live price by
    /// contract address — the whole point of `--live-prices` (fixes the
    /// blanket `price_unknown`). The resolver returns the captured token_price
    /// body keyed by the lowercased address.
    #[test]
    fn long_tail_token_resolved_by_contract_address() {
        let resolver: PriceResolver = Box::new(|url: &str| {
            assert!(url.contains("/simple/token_price/base"), "url: {url}");
            assert!(url.contains("contract_addresses=0x833589"), "url: {url}");
            Ok(r#"{"0x833589fcd6edb6e08f4c7c32d4f71b54bda02913":{"usd":0.999674}}"#.to_string())
        });
        let feed = LivePriceFeed::with_resolver(StaticPriceFeed::reference(), resolver);
        // (Base USDC *is* a known stable, but here we only assert the live path
        // is taken and the live number — not the $1 static value — is returned.)
        assert_eq!(feed.usd_price(tok(ChainId::BASE, BASE_USDC)), Some(0.999674));
    }

    /// On a network error the feed falls back to the static value (never
    /// `price_unknown` where static knows) — and the static value reflects the
    /// `PRICE_ETH_USD` override baked into the fallback. Drives the no-network
    /// CI path: the resolver always errors, so no real request is made.
    #[test]
    fn network_error_falls_back_to_static_with_override() {
        let resolver: PriceResolver = Box::new(|_url| anyhow::bail!("offline"));
        // Fallback built as if PRICE_ETH_USD=2222 was set (env not touched here).
        let fallback = StaticPriceFeed::new(2222.0, 100_000.0, 0.5);
        let feed = LivePriceFeed::with_resolver(fallback, resolver);
        // WETH on Optimism → static eth value (the override), not None.
        let weth = tok(ChainId::OPTIMISM, address!("4200000000000000000000000000000000000006"));
        assert_eq!(feed.usd_price(weth), Some(2222.0));
        // A known stable still resolves to $1 via the fallback.
        assert_eq!(feed.usd_price(tok(ChainId::BASE, BASE_USDC)), Some(1.0));
    }

    /// A token unknown to BOTH CoinGecko (empty body) and the static feed stays
    /// `None` — an honest `price_unknown`, never a fabricated price (rule #2).
    #[test]
    fn unknown_to_both_is_none() {
        let resolver: PriceResolver = Box::new(|_url| Ok("{}".to_string()));
        let feed = LivePriceFeed::with_resolver(StaticPriceFeed::reference(), resolver);
        let mystery = tok(ChainId::BASE, Address::repeat_byte(0x42));
        assert_eq!(feed.usd_price(mystery), None);
    }

    /// An unmapped chain (no verified CoinGecko platform) never makes a request
    /// — the resolver panics if called — and defers to the static feed, which
    /// for an unknown token is `None`. Guards against querying a guessed
    /// platform id (rule #2).
    #[test]
    fn unmapped_chain_skips_network_and_defers_to_static() {
        let resolver: PriceResolver = Box::new(|_url| panic!("must not query an unmapped chain"));
        let feed = LivePriceFeed::with_resolver(StaticPriceFeed::reference(), resolver);
        // Scroll has no coingecko_platform mapping; unknown token → None, no call.
        let t = tok(ChainId::SCROLL, Address::repeat_byte(0x11));
        assert_eq!(feed.usd_price(t), None);
    }
}
