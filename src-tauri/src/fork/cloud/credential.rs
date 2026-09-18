//! The credential entity: one API key, stored once no matter how many
//! capabilities use it. Duplicating per capability would mean duplicate
//! rotation, duplicate revocation, and copies that drift.

use serde::{Deserialize, Serialize};
use specta::Type;

/// Key-level health, as shown in the credentials list.
///
/// Not per-capability: a key can cool down for chat while serving speech, so a
/// single badge would be a lie. Per-capability health lives in the capability
/// tabs.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Type, Default)]
#[serde(rename_all = "snake_case")]
pub enum CredentialValidity {
    #[default]
    Untested,
    Valid,
    /// The provider answered 401/403.
    Invalid,
}

/// The secret is not here: it lives in `AppSettings::cloud_credential_secrets`
/// keyed by [`Credential::id`], so this can be logged and rendered freely.
#[derive(Serialize, Deserialize, Debug, Clone, Type)]
pub struct Credential {
    /// Bindings reference this, never the label, so renaming cannot detach a
    /// credential from its rotation state.
    pub id: String,
    pub label: String,
    pub provider_id: String,
    #[serde(default)]
    pub validity: CredentialValidity,
    /// Whether this account meters speech and cleanup from one allowance.
    ///
    /// Off by default, which keeps every (capability, model) bucket independent.
    /// That is right for Groq, whose speech and chat quotas are separate, and
    /// being wrong in that direction costs one request against a key that turns
    /// out to be exhausted. Being wrong the other way benches a feature that
    /// still had allowance left, which is worse, so this is opt-in rather than
    /// guessed from the provider.
    #[serde(default)]
    pub shared_quota: bool,
}

impl Credential {
    pub fn new(
        id: impl Into<String>,
        label: impl Into<String>,
        provider_id: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
            provider_id: provider_id.into(),
            validity: CredentialValidity::Untested,
            shared_quota: false,
        }
    }
}

/// Mint an id unique within this user's credential list. Deliberately not a
/// UUID dependency: a fork pays for every added crate at rebase time.
pub fn new_credential_id(existing: &[Credential]) -> String {
    use std::time::{SystemTime, UNIX_EPOCH};

    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);

    let mut candidate = format!("cred_{millis}");
    let mut suffix = 1_u32;
    while existing.iter().any(|c| c.id == candidate) {
        candidate = format!("cred_{millis}_{suffix}");
        suffix += 1;
    }
    candidate
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn minted_ids_stay_unique_within_the_same_millisecond() {
        let mut all: Vec<Credential> = Vec::new();
        for _ in 0..8 {
            let id = new_credential_id(&all);
            assert!(!all.iter().any(|c| c.id == id), "collided");
            all.push(Credential::new(id, "k", "groq"));
        }
    }
}
