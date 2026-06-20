//! UniswapX orders API client.
//!
//! The API is documented at <https://docs.uniswap.org/contracts/uniswapx/api>.
//! We treat field names defensively (many `#[serde(default)]`) because the
//! API has historically shifted field shapes across versions; we'd rather
//! ingest a partial record than fail the whole pull.

use std::path::PathBuf;
use std::str::FromStr;
use std::time::Duration as StdDuration;

use alloy_primitives::{Address, U256};
use anyhow::{Context, Result};
use chrono::{DateTime, Duration, Utc};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use solver_types::{
    known_decimals, AssetAmount, ChainId, DutchDecay, ExclusivityWindow, IntentId, NormalizedIntent,
    SettlementType, Source, TokenRef, TxRef,
};
use tracing::{debug, info, warn};

use crate::store::JsonlSink;

const UX_API_BASE: &str = "https://api.uniswap.org/v2";

/// Chains where UniswapX has meaningful volume. Add more as it expands.
pub const UNISWAPX_CHAINS: &[ChainId] = &[
    ChainId::ETHEREUM,
    ChainId::ARBITRUM,
    ChainId::BASE,
    ChainId::POLYGON,
    ChainId::OPTIMISM,
    ChainId::UNICHAIN,
];

#[derive(Debug, Clone)]
pub struct UniswapXIngestConfig {
    pub days_back: i64,
    pub out_dir: PathBuf,
    /// Per-request limit (API max is typically 50).
    pub page_size: u32,
    /// Optional override.
    pub chains: Option<Vec<ChainId>>,
    /// Override base URL (escape hatch if Uniswap shifts the endpoint).
    pub base_url: String,
}

impl UniswapXIngestConfig {
    pub fn defaults(out_dir: PathBuf) -> Self {
        Self {
            days_back: 30,
            out_dir,
            page_size: 50,
            chains: None,
            base_url: UX_API_BASE.to_string(),
        }
    }
}

pub struct UniswapXIngest {
    cfg: UniswapXIngestConfig,
    http: Client,
}

impl UniswapXIngest {
    pub fn new(cfg: UniswapXIngestConfig) -> Result<Self> {
        let http = Client::builder()
            .timeout(StdDuration::from_secs(30))
            .user_agent("solver-ingest/0.0.1")
            .build()
            .context("building reqwest client")?;
        Ok(Self { cfg, http })
    }

    pub async fn run(&self) -> Result<usize> {
        let until = Utc::now();
        let since = until - Duration::days(self.cfg.days_back);
        info!(?since, ?until, "starting UniswapX backfill");

        let chains: Vec<ChainId> = self.cfg.chains.clone()
            .unwrap_or_else(|| UNISWAPX_CHAINS.to_vec());

        let mut total = 0;
        for chain in chains {
            match self.scan_chain(chain, since).await {
                Ok(n) => total += n,
                Err(e) => warn!(chain = %chain, error = %e, "uniswapx chain scan failed; continuing"),
            }
        }
        Ok(total)
    }

    async fn scan_chain(&self, chain: ChainId, since: DateTime<Utc>) -> Result<usize> {
        let out_path = self.cfg.out_dir
            .join(format!("uniswapx-{}-{}.jsonl", chain.name(), Utc::now().format("%Y%m%d")));
        let mut sink = JsonlSink::open(&out_path).await?;

        let mut cursor: Option<String> = None;
        let mut written = 0usize;
        let since_unix = since.timestamp() as u64;

        loop {
            let page = self.fetch_page(chain, cursor.as_deref(), since_unix).await?;
            if page.orders.is_empty() { break; }

            for order in &page.orders {
                let ts = order.created_at_unix();
                if ts < since_unix { continue; }
                match Self::normalize(chain, order) {
                    Ok(intent) => { sink.write(&intent).await?; written += 1; }
                    Err(e) => debug!(error = %e, "skipping uniswapx order"),
                }
            }

            cursor = page.cursor;
            if cursor.is_none() { break; }
        }

        sink.flush().await?;
        info!(chain = %chain, written, "uniswapx chain done");
        Ok(written)
    }

    async fn fetch_page(&self, chain: ChainId, cursor: Option<&str>, since_unix: u64) -> Result<OrdersPage> {
        // UniswapX API quirks (learned at runtime, 2026):
        //   - Default order is not strictly time-sorted; pagination is by
        //     stable cursor across (chainId, orderStatus, orderHash).
        //   - To restrict to a time window we use the range-filter syntax:
        //     `sortKey=createdAt&sort=gte(<unix>)`. Without this we'd page
        //     through years of historical data.
        //   - orderTypes=Dutch_V1_V2,Dutch_V3 is required to get V3 orders
        //     which dominate volume but are excluded from the default filter.
        let mut url = format!(
            "{base}/orders?chainId={cid}&orderStatus=filled\
             &orderTypes=Dutch_V1_V2,Dutch_V3\
             &limit={lim}&sortKey=createdAt&sort=gte({since})",
            base  = self.cfg.base_url,
            cid   = chain.0,
            lim   = self.cfg.page_size,
            since = since_unix,
        );
        if let Some(c) = cursor {
            url.push_str("&cursor=");
            url.push_str(&urlencoding(c));
        }
        let resp = self.http.get(&url).send().await
            .with_context(|| format!("GET {url}"))?;
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            anyhow::bail!("uniswapx orders API {} -> {}: {}", url, status, body);
        }
        let page: OrdersPage = serde_json::from_str(&body)
            .with_context(|| format!("parsing uniswapx page body: {body}"))?;
        Ok(page)
    }

    fn normalize(chain: ChainId, order: &Order) -> Result<NormalizedIntent> {
        // UniswapX surfaces two shapes:
        //   - `Dutch` orders use `startAmount`/`endAmount` on input + outputs.
        //   - `Priority` orders (newer, on Base/Unichain) use a single `amount`
        //     plus priority-fee bidding.
        // The Order DTO accepts both; we coalesce here.
        let input_token = parse_addr_opt(&order.input.token).unwrap_or(Address::ZERO);
        let input_amount = order.input.effective_amount();
        let first_output = order.outputs.first().ok_or_else(|| anyhow::anyhow!("no outputs"))?;
        let output_token = parse_addr_opt(&first_output.token).unwrap_or(Address::ZERO);
        // UniswapX orders typically carry a swapper output PLUS an
        // interface-fee output in the same token — the filler owes the sum,
        // not just outputs[0]. Outputs in a different token (rare) are not
        // modeled and would understate cost; they stay out of this sum.
        let (output_start, output_end) = sum_same_token_outputs(&order.outputs, &first_output.token);

        let dest_chain = order.dest_chain_id
            .map(ChainId)
            .unwrap_or(chain); // single-chain UniswapX: dest = origin

        let in_tok  = TokenRef::new(chain, input_token);
        let out_tok = TokenRef::new(dest_chain, output_token);
        let in_dec  = known_decimals(in_tok);
        let out_dec = known_decimals(out_tok);

        let observed = DateTime::<Utc>::from_timestamp(order.created_at_unix() as i64, 0)
            .unwrap_or_else(Utc::now);
        let deadline = DateTime::<Utc>::from_timestamp(order.deadline.unwrap_or(0) as i64, 0)
            .unwrap_or(observed + Duration::minutes(5));

        // Dutch V3: decay times live in cosignerData when absent at top level (RUN-7).
        let (decay_start, decay_end) = match (order.decay_start_time, order.decay_end_time) {
            (Some(s), Some(e)) => (Some(s), Some(e)),
            _ => order
                .cosigner_data
                .as_ref()
                .map(|cd| (cd.decay_start_time, cd.decay_end_time))
                .unwrap_or((None, None)),
        };

        let decay = match (decay_start, decay_end) {
            (Some(s), Some(e)) => Some(DutchDecay {
                start_time: DateTime::<Utc>::from_timestamp(s as i64, 0).unwrap_or(observed),
                end_time:   DateTime::<Utc>::from_timestamp(e as i64, 0).unwrap_or(deadline),
                start_output: output_start,
                end_output: output_end,
            }),
            _ => None,
        };

        // Dutch V3: exclusivity from cosignerData.exclusiveFiller (RUN-7).
        let exclusivity = order.cosigner_data.as_ref().and_then(|cd| {
            if cd.exclusive_filler.is_empty() {
                return None;
            }
            let addr = parse_addr_opt(&cd.exclusive_filler).filter(|a| !a.is_zero())?;
            let excl_deadline = decay_start
                .and_then(|s| DateTime::<Utc>::from_timestamp(s as i64, 0))
                .unwrap_or(deadline);
            Some(ExclusivityWindow {
                exclusive_relayer: addr,
                deadline: excl_deadline,
            })
        });

        let realized = order.settled_amounts.first().and_then(|s|
            parse_u256_opt(&s.amount_out).map(|amt|
                AssetAmount::new(out_tok, amt, out_dec)
            )
        );

        let id = IntentId(format!("uniswapx:{}", order.order_hash.as_deref().unwrap_or("?")));
        let raw = serde_json::to_value(order).unwrap_or(serde_json::Value::Null);

        Ok(NormalizedIntent {
            id,
            source: Source::UniswapX,
            origin_chain: chain,
            dest_chain,
            input: AssetAmount::new(in_tok, input_amount, in_dec),
            output_min: AssetAmount::new(out_tok, output_end, out_dec),
            output_decay: decay,
            deadline,
            settlement: SettlementType::OffchainAttestation,
            observed_at: observed,
            exclusivity,
            winning_filler: order.filler.as_deref().and_then(parse_addr_opt),
            realized_output: realized,
            // VL-2: UniswapX `txHash` on a filled order is the settlement tx
            // on the order's `chainId` — which is `chain`, the chain we
            // queried for. The orders API only sets this for filled orders;
            // for anything else it's absent / empty and `tx_ref` stays None.
            tx_ref: order
                .tx_hash
                .as_deref()
                .filter(|s| !s.is_empty())
                .map(|h| TxRef {
                    chain,
                    tx_hash: h.to_string(),
                }),
            raw,
        })
    }
}

fn urlencoding(s: &str) -> String {
    // Minimal percent-encoding for cursor values.
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
            _ => format!("%{:02X}", b),
        })
        .collect()
}

/// Saturating sum of (start, end) amounts over outputs denominated in
/// `token` (case-insensitive hex compare). Saturating per invariant 6: one
/// malformed order must not abort the whole ingest.
fn sum_same_token_outputs(outputs: &[OrderOutput], token: &str) -> (U256, U256) {
    let mut start = U256::ZERO;
    let mut end = U256::ZERO;
    for o in outputs {
        if o.token.eq_ignore_ascii_case(token) {
            start = start.saturating_add(o.start_amount_u256());
            end = end.saturating_add(o.end_amount_u256());
        }
    }
    (start, end)
}

fn parse_addr_opt(s: &str) -> Option<Address> {
    Address::from_str(s.trim()).ok()
}

fn parse_u256_opt(s: &str) -> Option<U256> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    if let Some(hex) = s.strip_prefix("0x") {
        if hex.is_empty() { return None; }
        return U256::from_str_radix(hex, 16).ok();
    }
    U256::from_str_radix(s, 10).ok()
}

// -------- API DTOs (defensive) --------

#[derive(Debug, Clone, Deserialize, Serialize)]
struct OrdersPage {
    #[serde(default)]
    orders: Vec<Order>,
    #[serde(default)]
    cursor: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct Order {
    #[serde(rename = "orderHash", default)]
    order_hash: Option<String>,
    #[serde(rename = "type", default)]
    order_type: Option<String>,
    #[serde(default)]
    swapper: Option<String>,
    #[serde(default)]
    filler: Option<String>,
    #[serde(default)]
    input: OrderInput,
    #[serde(default)]
    outputs: Vec<OrderOutput>,
    #[serde(rename = "settledAmounts", default)]
    settled_amounts: Vec<SettledAmount>,
    #[serde(rename = "createdAt", default)]
    created_at: Option<u64>,
    #[serde(rename = "decayStartTime", default)]
    decay_start_time: Option<u64>,
    #[serde(rename = "decayEndTime", default)]
    decay_end_time: Option<u64>,
    #[serde(default)]
    deadline: Option<u64>,
    #[serde(rename = "destinationChainId", default)]
    dest_chain_id: Option<u64>,
    #[serde(rename = "txHash", default)]
    tx_hash: Option<String>,
    /// Dutch V3: decay and exclusivity live here when absent at top level (RUN-7).
    #[serde(rename = "cosignerData", default)]
    cosigner_data: Option<CosignerData>,
}

/// Cosigner payload for Dutch V3 orders (backtest path).
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
struct CosignerData {
    #[serde(rename = "decayStartTime", default)]
    decay_start_time: Option<u64>,
    #[serde(rename = "decayEndTime", default)]
    decay_end_time: Option<u64>,
    #[serde(rename = "exclusiveFiller", default)]
    exclusive_filler: String,
}

impl Order {
    fn created_at_unix(&self) -> u64 { self.created_at.unwrap_or(0) }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
struct OrderInput {
    #[serde(default)]
    token: String,
    #[serde(rename = "startAmount", default)]
    start_amount: String,
    #[serde(rename = "endAmount", default)]
    end_amount: String,
    /// Priority-order shape: single `amount` instead of start/end.
    #[serde(default)]
    amount: String,
}

impl OrderInput {
    fn effective_amount(&self) -> U256 {
        parse_u256_opt(&self.start_amount)
            .or_else(|| parse_u256_opt(&self.amount))
            .unwrap_or(U256::ZERO)
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
struct OrderOutput {
    #[serde(default)]
    token: String,
    #[serde(rename = "startAmount", default)]
    start_amount: String,
    #[serde(rename = "endAmount", default)]
    end_amount: String,
    #[serde(default)]
    amount: String,
    #[serde(default)]
    recipient: String,
}

impl OrderOutput {
    fn start_amount_u256(&self) -> U256 {
        parse_u256_opt(&self.start_amount)
            .or_else(|| parse_u256_opt(&self.amount))
            .unwrap_or(U256::ZERO)
    }
    fn end_amount_u256(&self) -> U256 {
        parse_u256_opt(&self.end_amount)
            .or_else(|| parse_u256_opt(&self.amount))
            .unwrap_or_else(|| self.start_amount_u256())
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
struct SettledAmount {
    #[serde(rename = "amountOut", default)]
    amount_out: String,
    #[serde(rename = "tokenOut", default)]
    token_out: String,
    #[serde(rename = "amountIn", default)]
    amount_in: String,
    #[serde(rename = "tokenIn", default)]
    token_in: String,
}

#[cfg(test)]
mod tests {
    use super::{OrdersPage, UniswapXIngest};
    use alloy_primitives::{Address, U256};
    use solver_types::{ChainId, Source};
    use std::str::FromStr;

    // A single-chain Base order in the documented UniswapX `/orders` shape:
    // a Dutch_V3 swapper output PLUS an interface-fee output in the SAME token,
    // decay + exclusivity carried in `cosignerData` (the V3 layout), a filled
    // `settledAmounts`, and a settlement `txHash`. This exercises the transform
    // (field renames, start/end coalescing, same-token output summation, the
    // cosigner decay/exclusivity fallback, realized extraction, tx_ref gating)
    // against a concrete input — not a constant compared with itself.
    const FILLED_V3_PAGE: &str = r#"{
      "orders": [{
        "orderHash": "0xabc0000000000000000000000000000000000000000000000000000000000001",
        "type": "Dutch_V3",
        "swapper": "0x1111111111111111111111111111111111111111",
        "filler": "0x2222222222222222222222222222222222222222",
        "input": { "token": "0x4200000000000000000000000000000000000006",
                   "startAmount": "1000000000000000000", "endAmount": "1000000000000000000" },
        "outputs": [
          { "token": "0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913",
            "startAmount": "3100000000", "endAmount": "3000000000",
            "recipient": "0x1111111111111111111111111111111111111111" },
          { "token": "0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913",
            "startAmount": "10000000", "endAmount": "9000000",
            "recipient": "0x4444444444444444444444444444444444444444" }
        ],
        "settledAmounts": [{ "amountOut": "3009000000",
                             "tokenOut": "0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913" }],
        "createdAt": 1750000000,
        "deadline": 1750000300,
        "txHash": "0xfeed000000000000000000000000000000000000000000000000000000000002",
        "cosignerData": { "decayStartTime": 1750000000, "decayEndTime": 1750000060,
                          "exclusiveFiller": "0x3333333333333333333333333333333333333333" }
      }],
      "cursor": null
    }"#;

    fn addr(s: &str) -> Address {
        Address::from_str(s).unwrap()
    }

    #[test]
    fn normalize_filled_v3_order() {
        let page: OrdersPage = serde_json::from_str(FILLED_V3_PAGE).unwrap();
        assert_eq!(page.orders.len(), 1);
        let intent = UniswapXIngest::normalize(ChainId::BASE, &page.orders[0]).unwrap();

        assert_eq!(
            intent.id.0,
            "uniswapx:0xabc0000000000000000000000000000000000000000000000000000000000001"
        );
        assert_eq!(intent.source, Source::UniswapX);
        // No `destinationChainId` → single-chain: dest == origin.
        assert_eq!(intent.origin_chain, ChainId::BASE);
        assert_eq!(intent.dest_chain, ChainId::BASE);

        // Input uses `startAmount`.
        assert_eq!(intent.input.amount, U256::from(1_000_000_000_000_000_000u64));
        // output_min is the END amount summed across the two same-token
        // outputs (swapper 3.0e9 + interface fee 9.0e6): the filler owes both.
        assert_eq!(intent.output_min.amount, U256::from(3_009_000_000u64));

        let decay = intent.output_decay.expect("V3 cosigner decay present");
        assert_eq!(decay.start_output, U256::from(3_110_000_000u64)); // 3.1e9 + 1.0e7
        assert_eq!(decay.end_output, U256::from(3_009_000_000u64));

        let excl = intent.exclusivity.expect("exclusiveFiller present");
        assert_eq!(
            excl.exclusive_relayer,
            addr("0x3333333333333333333333333333333333333333")
        );

        assert_eq!(
            intent.winning_filler,
            Some(addr("0x2222222222222222222222222222222222222222"))
        );
        let realized = intent.realized_output.expect("settledAmounts present");
        assert_eq!(realized.amount, U256::from(3_009_000_000u64));

        let tx = intent.tx_ref.expect("txHash present on filled order");
        assert_eq!(tx.chain, ChainId::BASE);
        assert_eq!(
            tx.tx_hash,
            "0xfeed000000000000000000000000000000000000000000000000000000000002"
        );
    }

    #[test]
    fn normalize_rejects_order_with_no_outputs() {
        // An order with no outputs can't have an `output_min` — normalize must
        // error so the caller skips it, rather than fabricate a zero output.
        let page: OrdersPage = serde_json::from_str(
            r#"{ "orders": [{ "orderHash": "0xdead", "outputs": [] }] }"#,
        )
        .unwrap();
        assert!(UniswapXIngest::normalize(ChainId::BASE, &page.orders[0]).is_err());
    }

    #[test]
    fn normalize_open_order_without_txhash_has_no_tx_ref() {
        // Unsettled / open order: no `txHash`, no fill. tx_ref must stay None
        // (verify-links refuse to invent a URL — LIV-4 discipline).
        let page: OrdersPage = serde_json::from_str(
            r#"{ "orders": [{ "orderHash": "0xbeef",
                 "outputs": [{ "token": "0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913",
                               "startAmount": "1000000", "endAmount": "1000000" }] }] }"#,
        )
        .unwrap();
        let intent = UniswapXIngest::normalize(ChainId::BASE, &page.orders[0]).unwrap();
        assert!(intent.tx_ref.is_none());
        assert!(intent.realized_output.is_none());
    }
}
