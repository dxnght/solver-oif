use chrono::Duration;
use serde::{Deserialize, Serialize};

/// How a given intent gets validated and how the filler claims their input.
///
/// This is **the** axis of financial risk for the filler (spec §6) — it
/// determines challenge windows, attestation dependencies, and lock semantics.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SettlementType {
    /// Across-style: a proposer posts a root, an optimistic oracle gates the
    /// release. The filler's capital is exposed during the challenge window.
    OptimisticOracle {
        #[serde(with = "duration_secs")]
        challenge_window: Duration,
    },
    /// UniswapX-style: off-chain attestor signs the fill; trust is in the
    /// attestor infrastructure rather than in on-chain proofs.
    OffchainAttestation,
    /// OIF resource-lock / escrow: input is locked at origin, atomically
    /// releaseable on proof of fill at destination. Lowest filler risk.
    ResourceLock,
}

impl SettlementType {
    pub fn label(&self) -> &'static str {
        match self {
            SettlementType::OptimisticOracle { .. } => "optimistic",
            SettlementType::OffchainAttestation => "attestation",
            SettlementType::ResourceLock => "resource_lock",
        }
    }
}

mod duration_secs {
    use chrono::Duration;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(d: &Duration, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_i64(d.num_seconds())
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Duration, D::Error> {
        let secs = i64::deserialize(d)?;
        Ok(Duration::seconds(secs))
    }
}
