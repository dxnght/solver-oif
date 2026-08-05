use alloy_primitives::U256;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::fmt;

use crate::amount::{checked_decimals, AssetAmount, TokenRef};
use crate::chain::ChainId;
use crate::settlement::SettlementType;

/// Stable identifier for a normalized intent. Format is `{source}:{native_id}`
/// so we can dedup across sources and trace back to raw data.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct IntentId(pub String);

impl fmt::Display for IntentId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Origin protocol of an intent. Discriminator only — protocol-specific
/// details live in [`NormalizedIntent::raw`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    Across,
    UniswapX,
    Oif,
    Velora,
}

impl Source {
    pub fn as_str(self) -> &'static str {
        match self {
            Source::Across => "across",
            Source::UniswapX => "uniswapx",
            Source::Oif => "oif",
            Source::Velora => "velora",
        }
    }
}

/// Across exclusivity window: only the designated relayer may fill until `deadline`.
///
/// When this field is `Some` and `Utc::now() < deadline`, the intent is not
/// freely fillable. RouterV0 skips it to avoid on-chain reverts (§4.1 Across V3).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExclusivityWindow {
    pub exclusive_relayer: alloy_primitives::Address,
    pub deadline: DateTime<Utc>,
}

/// Reference to the on-chain transaction this intent was observed in.
///
/// `chain` is the chain where the transaction was mined — NOT necessarily
/// origin or dest of the intent. The mapping depends on what the ingest
/// event actually represents:
///
/// - Across `FilledV3Relay`: fill emitted on the **destination** chain.
/// - Across `V3FundsDeposited`: deposit emitted on the **origin** chain.
/// - UniswapX (filled order): settlement tx on the order's `chainId`.
///
/// Captured at ingest, where the upstream code already knows the chain — so
/// the verify-link helper does not have to guess by parsing raw fields
/// downstream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TxRef {
    pub chain: ChainId,
    pub tx_hash: String,
}

/// Parameters of a Dutch auction's price decay over time. UniswapX-shaped.
///
/// Effective output at time `t` is linearly interpolated between
/// `start_output` (at `start_time`) and `end_output` (at `end_time`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DutchDecay {
    pub start_time: DateTime<Utc>,
    pub end_time: DateTime<Utc>,
    pub start_output: U256,
    pub end_output: U256,
}

/// Cross-source unified view of an intent. Once an intent is in this shape,
/// the rest of the engine doesn't need to know where it came from.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NormalizedIntent {
    pub id: IntentId,
    pub source: Source,
    pub origin_chain: ChainId,
    pub dest_chain: ChainId,
    /// What the user supplies on `origin_chain`.
    pub input: AssetAmount,
    /// Minimum the filler must deliver on `dest_chain`.
    pub output_min: AssetAmount,
    /// Present only for Dutch-auction sources (UniswapX, OIF Dutch).
    pub output_decay: Option<DutchDecay>,
    /// When this intent stops being fillable.
    pub deadline: DateTime<Utc>,
    pub settlement: SettlementType,
    /// When the intent was observed (created at origin / posted to mempool).
    pub observed_at: DateTime<Utc>,
    /// Filler that actually won the auction in historical data (Phase 0 only).
    /// `None` for live intents in Phase 1+.
    pub winning_filler: Option<alloy_primitives::Address>,
    /// Output amount that was actually delivered, if known. For Phase 0 this
    /// is what we benchmark our quote against.
    pub realized_output: Option<AssetAmount>,
    /// Across exclusivity: if set, only the designated relayer may fill
    /// until the deadline. Missing from historical data (defaults to None).
    #[serde(default)]
    pub exclusivity: Option<ExclusivityWindow>,
    /// VL-1: on-chain tx that backs this intent (deposit on origin, fill on
    /// dest, or filled order tx — see [`TxRef`] for the per-source mapping).
    /// `#[serde(default)]` so old JSONL rows written before VL landed still
    /// parse into the post-VL struct without any data migration step.
    #[serde(default)]
    pub tx_ref: Option<TxRef>,
    /// Raw source-specific JSON. Kept so the [`crate::Quote`] step can
    /// reconstruct fill calldata without re-fetching.
    pub raw: serde_json::Value,
}

impl NormalizedIntent {
    /// Composite route key (`origin_chain → dest_chain × input_token → output_token`).
    /// This is what Phase 0 metrics aggregate over.
    pub fn route_key(&self) -> String {
        format!(
            "{}->{}/{}->{}",
            self.origin_chain,
            self.dest_chain,
            short_addr(self.input.token.address.0.as_slice()),
            short_addr(self.output_min.token.address.0.as_slice()),
        )
    }

    /// VL-1: clickable block-explorer URL for the tx that backs this intent
    /// (deposit on origin, fill on dest, or filled-order tx). Returns
    /// `None` when no tx hash was captured (e.g. unsettled UniswapX open
    /// order) OR when the chain's explorer is not in our verified-majors
    /// list — we deliberately refuse to invent a URL for an unfamiliar host
    /// (LIV-4 discipline). Callers should treat `None` as "no link to show",
    /// not as an error.
    pub fn verify_url(&self) -> Option<String> {
        self.tx_ref
            .as_ref()
            .and_then(|r| r.chain.explorer_tx_url(&r.tx_hash))
    }

    /// Price-only realized margin: `input_usd − realized_output_usd`. The
    /// terms the winning filler actually accepted, ignoring their gas. This
    /// is the right reference for "could an independent filler have profited
    /// here?" — compare against [`crate::FillPolicy::min_margin_usd`]
    /// (would_fill calibration, WFC-2). Returns `None` when there is no
    /// realized output, when either token's price is unknown, OR when either
    /// token's decimals are unknown.
    ///
    /// The decimals guard applies the same discipline as the Router's modeled
    /// leg ([`checked_decimals`]): valuation must not guess a scale. The stored
    /// `AssetAmount.decimals` is an ingest convenience that falls back to the
    /// ERC-20 default of 18, so reading a 6-decimal realized amount as 18-dp
    /// would underflow it to ~0 and fabricate a full-notional "realized margin"
    /// — garbage drift in the backtest metric. Re-derive from recognized tokens
    /// and return `None` rather than invent a number (CLAUDE.md rule #2). Today
    /// the consumed drift path only reaches here for router-quoted intents
    /// (whose legs are already recognized), but this method is public and also
    /// backs the `with_realized` count over *every* intent — so it must be
    /// honest on its own, not by relying on an upstream skip.
    pub fn realized_gross_margin_usd<F>(&self, price: F) -> Option<f64>
    where
        F: Fn(TokenRef) -> Option<f64>,
    {
        let realized = self.realized_output.as_ref()?;
        let p_in = price(self.input.token)?;
        let p_out = price(realized.token)?;
        let in_dec = checked_decimals(self.input.token)?;
        let out_dec = checked_decimals(realized.token)?;
        let input_usd = AssetAmount::new(self.input.token, self.input.amount, in_dec).to_f64() * p_in;
        let realized_usd =
            AssetAmount::new(realized.token, realized.amount, out_dec).to_f64() * p_out;
        Some(input_usd - realized_usd)
    }
}

fn short_addr(bytes: &[u8]) -> String {
    if bytes.iter().all(|b| *b == 0) {
        return "native".into();
    }
    let hex = hex::encode(bytes);
    format!("0x{}…{}", &hex[..4], &hex[hex.len() - 4..])
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{address, Address};

    /// Real Arbitrum-native USDC (6 decimals, in `STABLE_ADDRS`), so
    /// `checked_decimals` resolves it — a recognized token is exactly what the
    /// realized valuation now requires. Using the true address keeps the
    /// margin fixtures honest (independent truth, rule #3), not a placeholder.
    fn usdc() -> TokenRef {
        TokenRef::new(ChainId::ARBITRUM, address!("af88d065e77c8cC2239327C5EDb3A432268e5831"))
    }

    fn make_intent(realized_units: Option<u64>) -> NormalizedIntent {
        NormalizedIntent {
            id: IntentId("test:1".into()),
            source: Source::Across,
            origin_chain: ChainId::BASE,
            dest_chain: ChainId::ARBITRUM,
            input: AssetAmount::new(usdc(), U256::from(1_000_000u64), 6),
            output_min: AssetAmount::new(usdc(), U256::from(999_000u64), 6),
            output_decay: None,
            deadline: Utc::now(),
            settlement: SettlementType::OffchainAttestation,
            observed_at: Utc::now(),
            winning_filler: None,
            realized_output: realized_units
                .map(|u| AssetAmount::new(usdc(), U256::from(u), 6)),
            exclusivity: None,
            tx_ref: None,
            raw: serde_json::Value::Null,
        }
    }

    #[test]
    fn realized_gross_margin_usdc_pair() {
        // 1.000 USDC in, 0.999 USDC realized → +$0.001 margin.
        let intent = make_intent(Some(999_000));
        let m = intent.realized_gross_margin_usd(|_| Some(1.0)).unwrap();
        assert!((m - 0.001).abs() < 1e-9, "got {m}");
    }

    #[test]
    fn realized_gross_margin_returns_none_without_realized() {
        let intent = make_intent(None);
        assert!(intent.realized_gross_margin_usd(|_| Some(1.0)).is_none());
    }

    #[test]
    fn realized_gross_margin_returns_none_when_price_missing() {
        let intent = make_intent(Some(999_000));
        assert!(intent.realized_gross_margin_usd(|_| None).is_none());
    }

    /// The realized valuation obeys the same checked-decimals discipline as the
    /// Router's modeled leg: an unrecognized token — whose decimals we cannot
    /// vouch for — yields `None`, never a fabricated margin, **even when a price
    /// is available** (as a live feed can supply one by contract address).
    /// Reading its raw amount with the stored 18-dp ingest fallback would
    /// underflow a 6-dp realized amount to ~0 and print the full input as
    /// "realized margin" (garbage drift); refusing to guess keeps that out of
    /// the backtest metric (CLAUDE.md rule #2). Independent truth: a real
    /// recognized input vs an unlisted realized token, priced but unscalable.
    #[test]
    fn realized_gross_margin_none_for_unknown_decimals_realized_token() {
        let mystery = TokenRef::new(ChainId::ARBITRUM, Address::repeat_byte(0x42));
        let mut intent = make_intent(Some(999_000));
        // A 6-dp amount stored with the ingest fallback of 18 — the exact shape
        // that would underflow to ~0 and fabricate a full-notional margin.
        intent.realized_output =
            Some(AssetAmount::new(mystery, U256::from(999_000u64), 18));
        // Every token is priced at $1 — a price is not enough; we also need to
        // trust the scale, and for this token we do not.
        assert!(
            intent.realized_gross_margin_usd(|_| Some(1.0)).is_none(),
            "priced-but-unrecognized realized token must not fabricate a margin"
        );
    }

    /// VL-1: without a `tx_ref`, there's nothing to link to. Must be `None`,
    /// not a partial URL — downstream renderers branch on `Option` and
    /// hiding the "no link" case as e.g. an empty string would muddle the
    /// table render.
    #[test]
    fn verify_url_is_none_without_tx_ref() {
        let intent = make_intent(None);
        assert!(intent.verify_url().is_none());
    }

    /// VL-1: with a `tx_ref` on a verified-major chain, the URL composes via
    /// the explorer map. Exercises the same chain pair an Across fill on
    /// Arbitrum would carry — the most common shape in the JSONL.
    #[test]
    fn verify_url_with_tx_ref_on_major_chain() {
        let mut intent = make_intent(None);
        intent.tx_ref = Some(TxRef {
            chain: ChainId::ARBITRUM,
            tx_hash: "0xABCDEF0123456789abcdef0123456789abcdef0123456789abcdef0123456789".into(),
        });
        assert_eq!(
            intent.verify_url(),
            Some(
                "https://arbiscan.io/tx/0xabcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789"
                    .to_string()
            )
        );
    }

    /// VL-1: a `tx_ref` on an unverified chain returns `None` rather than a
    /// fabricated URL — the LIV-4 anti-footgun. If we later add a chain to
    /// `explorer_tx_url`, this test will need to pick a different unverified
    /// chain id to keep its meaning.
    #[test]
    fn verify_url_returns_none_for_unverified_chain() {
        let mut intent = make_intent(None);
        intent.tx_ref = Some(TxRef {
            chain: ChainId::SCROLL,
            tx_hash: "0xdeadbeef00000000000000000000000000000000000000000000000000000001".into(),
        });
        assert!(intent.verify_url().is_none());
    }
}
