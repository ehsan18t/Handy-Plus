//! Fork-owned cloud provider layer: credentials, rotation, and cloud STT.
//!
//! Upstream files reach in at a few named call sites; nothing here reaches back
//! out except through `crate::settings`. That direction is what keeps a rebase
//! legible.

pub mod binding;
pub mod capability;
pub mod commands;
pub mod credential;
pub mod error;
pub mod pool;
pub mod post_process;
pub mod providers;
pub mod runtime;
pub mod state;
pub mod stt;

pub use binding::{CapabilityBinding, CapabilityBindings, RotationPolicy};
pub use capability::Capability;
pub use credential::{new_credential_id, Credential, CredentialValidity};
pub use error::{parse_retry_after, ApiError, FailureClass};
pub use pool::{plan, Attempt, CredentialPool, PoolError, PoolPlan, PoolRun};
pub use providers::ProviderInfo;
pub use state::{PairState, RotationStateStore};

use std::time::{SystemTime, UNIX_EPOCH};

/// Milliseconds since the epoch, saturating to 0 if the clock predates it.
/// Shared so the state store, the pool and the status command cannot disagree
/// about what "now" is.
pub(crate) fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}
