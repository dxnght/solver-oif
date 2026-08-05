use serde::{Deserialize, Serialize};
use std::fmt;

/// EVM chain identifier. Stored as raw `u64` rather than an enum because
/// the long-tail of L2s expands faster than we want to recompile.
///
/// Helpers ([`Self::name`], [`Self::is_l2`]) are best-effort lookups.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ChainId(pub u64);

impl ChainId {
    pub const ETHEREUM: ChainId = ChainId(1);
    pub const OPTIMISM: ChainId = ChainId(10);
    pub const BSC: ChainId = ChainId(56);
    pub const POLYGON: ChainId = ChainId(137);
    pub const ZKSYNC: ChainId = ChainId(324);
    pub const WORLDCHAIN: ChainId = ChainId(480);
    pub const REDSTONE: ChainId = ChainId(690);
    pub const BOBA: ChainId = ChainId(288);
    pub const LISK: ChainId = ChainId(1135);
    pub const POLYGON_ZKEVM: ChainId = ChainId(1101);
    pub const MODE: ChainId = ChainId(34443);
    pub const MANTLE: ChainId = ChainId(5000);
    pub const BASE: ChainId = ChainId(8453);
    pub const ARBITRUM: ChainId = ChainId(42161);
    pub const ZORA: ChainId = ChainId(7777777);
    pub const SCROLL: ChainId = ChainId(534352);
    pub const LINEA: ChainId = ChainId(59144);
    pub const BLAST: ChainId = ChainId(81457);
    pub const INK: ChainId = ChainId(57073);
    pub const LENS: ChainId = ChainId(232);
    pub const SONEIUM: ChainId = ChainId(1868);
    pub const UNICHAIN: ChainId = ChainId(130);
    // Newer Across-supported chains (IDs cross-checked against
    // docs.across.to/reference/supported-chains, 2026-06). Stablecoin-heavy
    // flow — the regime where RouterV0's same-class inventory path is most
    // accurate.
    pub const HYPEREVM: ChainId = ChainId(999);
    pub const MONAD: ChainId = ChainId(143);
    pub const MEGAETH: ChainId = ChainId(4326);
    pub const PLASMA: ChainId = ChainId(9745);

    pub fn name(self) -> &'static str {
        match self.0 {
            1 => "ethereum",
            10 => "optimism",
            56 => "bsc",
            130 => "unichain",
            137 => "polygon",
            143 => "monad",
            232 => "lens",
            288 => "boba",
            324 => "zksync",
            480 => "worldchain",
            690 => "redstone",
            999 => "hyperevm",
            1101 => "polygon-zkevm",
            1135 => "lisk",
            1868 => "soneium",
            4326 => "megaeth",
            5000 => "mantle",
            8453 => "base",
            9745 => "plasma",
            34443 => "mode",
            42161 => "arbitrum",
            57073 => "ink",
            59144 => "linea",
            81457 => "blast",
            534352 => "scroll",
            7777777 => "zora",
            _ => "unknown",
        }
    }

    /// Inverse of [`Self::name`]: resolve a (case-insensitive) chain name to its
    /// id, or `None` for an unknown name. Used to parse the `solver live
    /// --chains base,arbitrum,…` list; callers surface a loud error on `None`
    /// rather than silently dropping a chain (legibility). Kept in lockstep with
    /// `name()` by [`tests::from_name_roundtrips_named_chains`].
    pub fn from_name(name: &str) -> Option<ChainId> {
        let c = match name.trim().to_ascii_lowercase().as_str() {
            "ethereum" => Self::ETHEREUM,
            "optimism" => Self::OPTIMISM,
            "bsc" => Self::BSC,
            "unichain" => Self::UNICHAIN,
            "polygon" => Self::POLYGON,
            "monad" => Self::MONAD,
            "lens" => Self::LENS,
            "boba" => Self::BOBA,
            "zksync" => Self::ZKSYNC,
            "worldchain" => Self::WORLDCHAIN,
            "redstone" => Self::REDSTONE,
            "hyperevm" => Self::HYPEREVM,
            "polygon-zkevm" => Self::POLYGON_ZKEVM,
            "lisk" => Self::LISK,
            "soneium" => Self::SONEIUM,
            "megaeth" => Self::MEGAETH,
            "mantle" => Self::MANTLE,
            "base" => Self::BASE,
            "plasma" => Self::PLASMA,
            "mode" => Self::MODE,
            "arbitrum" => Self::ARBITRUM,
            "ink" => Self::INK,
            "linea" => Self::LINEA,
            "blast" => Self::BLAST,
            "scroll" => Self::SCROLL,
            "zora" => Self::ZORA,
            _ => return None,
        };
        Some(c)
    }

    /// Best-effort: is this chain an L2/L3 (i.e. not Ethereum mainnet)?
    pub fn is_l2(self) -> bool {
        self != Self::ETHEREUM
    }

    /// Block-explorer URL for `tx_hash` on this chain, or `None` when no
    /// verified explorer is recorded.
    ///
    /// Verified majors only — Etherscan / its sister sites are the publishers
    /// of these domains and the hostnames are stable. For everything else we
    /// return `None` rather than guess (LIV-4 discipline: a broken link is
    /// worse than no link). Long-tail explorers can be added later from
    /// `https://chainid.network/chains.json` with the source URL in the
    /// arm's comment, but only after a per-chain manual cross-check; we do
    /// NOT bulk-import that file at runtime because its long tail includes
    /// dead and squatted domains.
    ///
    /// `tx_hash` is normalised to `0x…` lowercase before formatting. Inputs
    /// that are empty, non-hex, or otherwise unusable return `None` rather
    /// than producing a syntactically broken URL.
    pub fn explorer_tx_url(self, tx_hash: &str) -> Option<String> {
        let base = match self.0 {
            // Source: each explorer's own homepage / docs. Stable since
            // Etherscan launched the multichain sister sites.
            1 => "https://etherscan.io",
            10 => "https://optimistic.etherscan.io",
            137 => "https://polygonscan.com",
            8453 => "https://basescan.org",
            42161 => "https://arbiscan.io",
            _ => return None,
        };
        let trimmed = tx_hash.trim();
        let body = trimmed
            .strip_prefix("0x")
            .or_else(|| trimmed.strip_prefix("0X"))
            .unwrap_or(trimmed);
        if body.is_empty() || !body.chars().all(|c| c.is_ascii_hexdigit()) {
            return None;
        }
        Some(format!("{}/tx/0x{}", base, body.to_ascii_lowercase()))
    }
}

impl fmt::Display for ChainId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.name() {
            "unknown" => write!(f, "chain:{}", self.0),
            name => write!(f, "{}", name),
        }
    }
}

impl From<u64> for ChainId {
    fn from(id: u64) -> Self {
        Self(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every named constant must round-trip through `name()` (no constant
    /// left without a match arm, no typo'd id). Guards against adding a
    /// `ChainId::FOO` const but forgetting the `name()` arm — which would
    /// make it "unknown" and get it dropped by load-time chain hygiene.
    #[test]
    fn named_chains_resolve() {
        let named = [
            ChainId::ETHEREUM, ChainId::OPTIMISM, ChainId::BSC, ChainId::POLYGON, ChainId::ZKSYNC,
            ChainId::WORLDCHAIN, ChainId::REDSTONE, ChainId::BOBA, ChainId::LISK,
            ChainId::POLYGON_ZKEVM, ChainId::MODE, ChainId::MANTLE, ChainId::BASE,
            ChainId::ARBITRUM, ChainId::ZORA, ChainId::SCROLL, ChainId::LINEA,
            ChainId::BLAST, ChainId::INK, ChainId::LENS, ChainId::SONEIUM,
            ChainId::UNICHAIN, ChainId::HYPEREVM, ChainId::MONAD, ChainId::MEGAETH,
            ChainId::PLASMA,
        ];
        for c in named {
            assert_ne!(c.name(), "unknown", "chain id {} has no name() arm", c.0);
        }
    }

    /// `from_name` is the exact inverse of `name()` for every named chain, is
    /// case-insensitive, and returns `None` for an unknown name (so the CLI
    /// can error loudly instead of silently dropping it). Reuses the same
    /// named set as [`named_chains_resolve`] — one drifts, both fail.
    #[test]
    fn from_name_roundtrips_named_chains() {
        let named = [
            ChainId::ETHEREUM, ChainId::OPTIMISM, ChainId::BSC, ChainId::POLYGON, ChainId::ZKSYNC,
            ChainId::WORLDCHAIN, ChainId::REDSTONE, ChainId::BOBA, ChainId::LISK,
            ChainId::POLYGON_ZKEVM, ChainId::MODE, ChainId::MANTLE, ChainId::BASE,
            ChainId::ARBITRUM, ChainId::ZORA, ChainId::SCROLL, ChainId::LINEA,
            ChainId::BLAST, ChainId::INK, ChainId::LENS, ChainId::SONEIUM,
            ChainId::UNICHAIN, ChainId::HYPEREVM, ChainId::MONAD, ChainId::MEGAETH,
            ChainId::PLASMA,
        ];
        for c in named {
            assert_eq!(ChainId::from_name(c.name()), Some(c), "round-trip {}", c.0);
        }
        // Case-insensitive and whitespace-trimmed (CLI input is human).
        assert_eq!(ChainId::from_name("  Arbitrum "), Some(ChainId::ARBITRUM));
        assert_eq!(ChainId::from_name("BASE"), Some(ChainId::BASE));
        // Unknown → None (caller errors loudly, never a silent drop).
        assert_eq!(ChainId::from_name("not-a-chain"), None);
    }

    #[test]
    fn new_chain_ids_are_correct() {
        // Cross-checked against docs.across.to/reference/supported-chains.
        assert_eq!(ChainId::HYPEREVM.0, 999);
        assert_eq!(ChainId::MONAD.0, 143);
        assert_eq!(ChainId::MEGAETH.0, 4326);
        assert_eq!(ChainId::PLASMA.0, 9745);
    }

    /// VL-1: verified majors get a real URL on the correct host. Catches a
    /// typo'd domain (e.g. `etherescan.io`) or a wrong host assigned to a
    /// chain id during a sloppy refactor — both would leave the user
    /// clicking a 404 thinking the ingest hash itself is bad.
    #[test]
    fn explorer_tx_url_majors() {
        let hash = "0xABCDEF0123456789abcdef0123456789abcdef0123456789abcdef0123456789";
        assert_eq!(
            ChainId::ETHEREUM.explorer_tx_url(hash),
            Some("https://etherscan.io/tx/0xabcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789".to_string())
        );
        assert_eq!(
            ChainId::OPTIMISM.explorer_tx_url(hash),
            Some("https://optimistic.etherscan.io/tx/0xabcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789".to_string())
        );
        assert_eq!(
            ChainId::POLYGON.explorer_tx_url(hash),
            Some("https://polygonscan.com/tx/0xabcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789".to_string())
        );
        assert_eq!(
            ChainId::BASE.explorer_tx_url(hash),
            Some("https://basescan.org/tx/0xabcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789".to_string())
        );
        assert_eq!(
            ChainId::ARBITRUM.explorer_tx_url(hash),
            Some("https://arbiscan.io/tx/0xabcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789".to_string())
        );
    }

    /// VL-1: long-tail / unknown chains MUST return None. Inventing a host
    /// from memory (or returning a "guess" derived from `chainid.network`
    /// without a manual cross-check) is the LIV-4 antipattern — a fabricated
    /// URL is worse than the absence of a URL.
    #[test]
    fn explorer_tx_url_unknown_chain_returns_none() {
        let hash = "0xdeadbeef00000000000000000000000000000000000000000000000000000000";
        // ChainId::SCROLL (534352): a real chain id, but not in the verified
        // majors list — must return None until we add it deliberately.
        assert!(ChainId::SCROLL.explorer_tx_url(hash).is_none());
        // A totally fabricated chain id — also None.
        assert!(ChainId(99_999_999).explorer_tx_url(hash).is_none());
    }

    /// VL-1: hash normalisation — uppercase or bare-hex input normalises to
    /// `0x…` lowercase. Locks the formatting so report links are byte-equal
    /// across runs (diff-able snapshot tests) and matches what every
    /// Etherscan-family explorer accepts in its URL.
    #[test]
    fn explorer_tx_url_normalises_hash_to_lower_with_0x() {
        let lower = ChainId::BASE
            .explorer_tx_url("0xAABBccdd00000000000000000000000000000000000000000000000000000001")
            .unwrap();
        assert_eq!(
            lower,
            "https://basescan.org/tx/0xaabbccdd00000000000000000000000000000000000000000000000000000001"
        );
        // Bare-hex (no 0x) is tolerated — some upstream APIs deliver hashes
        // without the prefix; we add it.
        let bare = ChainId::BASE
            .explorer_tx_url("aabbccdd00000000000000000000000000000000000000000000000000000001")
            .unwrap();
        assert_eq!(bare, lower);
    }

    /// VL-1: refuse to build a URL for inputs that can't be a tx hash —
    /// empty string, non-hex characters, etc. Returning `Some` here would
    /// emit a syntactically broken URL ("…/tx/0x") which is exactly the
    /// LIV-4 footgun the helper exists to prevent.
    #[test]
    fn explorer_tx_url_rejects_unusable_input() {
        assert!(ChainId::BASE.explorer_tx_url("").is_none());
        assert!(ChainId::BASE.explorer_tx_url("0x").is_none());
        assert!(ChainId::BASE.explorer_tx_url("0xzzzz").is_none());
        assert!(ChainId::BASE.explorer_tx_url("not-a-hash").is_none());
    }
}
