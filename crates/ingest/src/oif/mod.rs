//! OIF (Open Intents Framework / ERC-7683): the verified contract address
//! ([`deployments`], OIF-1), the `Open` event shape ([`event`]), and the live
//! poll adapter ([`live`], OIF-2) — `solver live --sources oif` polls `Open`
//! the same way `--sources across` polls `FundsDeposited`.

pub mod deployments;
pub mod event;
pub mod live;

pub use deployments::{input_settler_escrow, INPUT_SETTLER_ESCROW, OIF_CHAINS};
pub use event::{MandateOutput, Open, StandardOrder};
pub use live::{normalize_open, OifLive};
