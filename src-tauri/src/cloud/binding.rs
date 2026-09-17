//! Per-capability configuration. Exactly two exist, configured fully
//! independently: different credentials, models, policies and windows. A
//! binding references credentials by id and never copies them.

use crate::cloud::Capability;
use serde::{Deserialize, Serialize};
use specta::Type;
use std::time::Duration;

pub const DEFAULT_COOLDOWN_SECS: u64 = 6 * 60 * 60;
pub const MAX_COOLDOWN_SECS: u64 = 7 * 24 * 60 * 60;
pub const DEFAULT_STRIKE_THRESHOLD: u32 = 3;
/// Past this a "threshold" stops meaning anything: the key would be retried
/// dozens of times per hour before ever being benched.
pub const MAX_STRIKE_THRESHOLD: u32 = 20;
/// Three failures inside an hour mean a broken key; three across a month are
/// noise, so strikes older than this are discarded when counting.
pub const DEFAULT_STRIKE_WINDOW_SECS: u64 = 60 * 60;

/// How the pool picks among credentials already known to be eligible.
/// Policies never see ineligible ones: filtering happens first, in one place.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Type, Default)]
#[serde(rename_all = "snake_case")]
pub enum RotationPolicy {
    #[default]
    RoundRobin,
    /// Oldest last-used first. Matches round robin in steady state but
    /// self-corrects after restarts, cooldown expiry, or a key added
    /// mid-session, where a raw cursor would point somewhere stale.
    LeastRecentlyUsed,
}

/// One participant in a rotation.
///
/// The model lives here rather than on the binding because two providers never
/// share a model id: an OpenAI key and a Groq key in one rotation need
/// different ones, and a single shared value would fail every request on
/// whichever provider does not own it.
#[derive(Serialize, Deserialize, Debug, Clone, Type)]
pub struct RotationEntry {
    pub credential_id: String,
    /// For Apple Intelligence this is a token limit, matching upstream.
    #[serde(default)]
    pub model: String,
    /// Post-processing only. References `AppSettings::post_process_prompts`.
    /// `None` falls back to the binding's default template.
    #[serde(default)]
    pub prompt_id: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Type)]
pub struct CapabilityBinding {
    /// Speech only. Post-processing has no switch of its own: upstream's
    /// `post_process_enabled` is the on/off, and an empty rotation is what says
    /// "use upstream's single key instead of the pool".
    #[serde(default)]
    pub enabled: bool,
    /// Order is the round-robin sequence. A credential may appear more than once
    /// so long as each appearance names a different model: providers meter per
    /// model, so those are independent quotas.
    #[serde(default)]
    pub entries: Vec<RotationEntry>,
    /// Speech only: vocabulary bias, shared by every entry. Cleanup
    /// instructions are per entry instead, via [`RotationEntry::prompt_id`].
    #[serde(default)]
    pub prompt: String,
    #[serde(default)]
    pub policy: RotationPolicy,
    #[serde(default = "default_cooldown_secs")]
    pub cooldown_secs: u64,
    #[serde(default = "default_strike_threshold")]
    pub strike_threshold: u32,
    #[serde(default = "default_strike_window_secs")]
    pub strike_window_secs: u64,
    /// On for speech means "run the local model" when every key is
    /// unavailable; off means the failure surfaces instead.
    #[serde(default = "default_true")]
    pub fallback_enabled: bool,
    /// Speech only. Empty means auto-detect.
    #[serde(default)]
    pub language: String,
}

fn default_cooldown_secs() -> u64 {
    DEFAULT_COOLDOWN_SECS
}

fn default_strike_threshold() -> u32 {
    DEFAULT_STRIKE_THRESHOLD
}

fn default_strike_window_secs() -> u64 {
    DEFAULT_STRIKE_WINDOW_SECS
}

fn default_true() -> bool {
    true
}

impl Default for CapabilityBinding {
    fn default() -> Self {
        Self {
            enabled: false,
            entries: Vec::new(),
            prompt: String::new(),
            policy: RotationPolicy::default(),
            cooldown_secs: DEFAULT_COOLDOWN_SECS,
            strike_threshold: DEFAULT_STRIKE_THRESHOLD,
            strike_window_secs: DEFAULT_STRIKE_WINDOW_SECS,
            fallback_enabled: true,
            language: String::new(),
        }
    }
}

impl CapabilityBinding {
    /// Bounds are applied on read as well as on write, so a hand-edited
    /// settings file cannot bench a credential for a year or bench it on the
    /// first use.
    pub fn cooldown(&self) -> Duration {
        Duration::from_secs(self.cooldown_secs.clamp(0, MAX_COOLDOWN_SECS))
    }

    pub fn strike_window(&self) -> Duration {
        Duration::from_secs(self.strike_window_secs.clamp(1, MAX_COOLDOWN_SECS))
    }

    pub fn effective_strike_threshold(&self) -> u32 {
        self.strike_threshold.clamp(1, MAX_STRIKE_THRESHOLD)
    }

    /// Normalise the rotation on the way into settings. Must de-duplicate on
    /// the same key as [`crate::cloud::plan`], or entries are dropped on save
    /// that the pool would have accepted.
    pub fn prune_entries(
        &mut self,
        known_credential: impl Fn(&str) -> bool,
        known_prompt: impl Fn(&str) -> bool,
    ) {
        let mut seen = std::collections::HashSet::new();
        self.entries.retain_mut(|entry| {
            // A dangling template fails the request instead of falling back.
            if entry.prompt_id.as_ref().is_some_and(|id| !known_prompt(id)) {
                entry.prompt_id = None;
            }
            known_credential(&entry.credential_id)
                && seen.insert((entry.credential_id.clone(), entry.model.trim().to_string()))
        });
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, Default, Type)]
pub struct CapabilityBindings {
    #[serde(default)]
    pub stt: CapabilityBinding,
    #[serde(default)]
    pub post_process: CapabilityBinding,
}

impl CapabilityBindings {
    pub fn get(&self, capability: Capability) -> &CapabilityBinding {
        match capability {
            Capability::Stt => &self.stt,
            Capability::PostProcess => &self.post_process,
        }
    }

    pub fn get_mut(&mut self, capability: Capability) -> &mut CapabilityBinding {
        match capability {
            Capability::Stt => &mut self.stt,
            Capability::PostProcess => &mut self.post_process,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_off_with_fallback_on() {
        let bindings = CapabilityBindings::default();
        assert!(!bindings.stt.enabled);
        assert!(!bindings.post_process.enabled);
        // Losing a dictation is worse than degrading it.
        assert!(bindings.stt.fallback_enabled);
    }

    #[test]
    fn out_of_range_stored_values_are_bounded_on_read() {
        let binding = CapabilityBinding {
            cooldown_secs: MAX_COOLDOWN_SECS * 10,
            strike_threshold: 0,
            strike_window_secs: 0,
            ..Default::default()
        };
        assert_eq!(binding.cooldown(), Duration::from_secs(MAX_COOLDOWN_SECS));
        assert_eq!(binding.effective_strike_threshold(), 1);
        assert_eq!(binding.strike_window(), Duration::from_secs(1));
    }

    fn entry(credential_id: &str, model: &str) -> RotationEntry {
        RotationEntry {
            credential_id: credential_id.to_string(),
            model: model.to_string(),
            prompt_id: None,
        }
    }

    #[test]
    fn one_key_on_two_models_survives_a_save() {
        let mut binding = CapabilityBinding {
            entries: vec![
                entry("cred_a", "llama-3.3-70b"),
                entry("cred_a", "llama-3.1-8b"),
                // Same key and same model: one attempt, not two.
                entry("cred_a", "llama-3.3-70b"),
                entry("gone", "whatever"),
            ],
            ..Default::default()
        };

        binding.prune_entries(|id| id != "gone", |_| true);

        assert_eq!(
            binding
                .entries
                .iter()
                .map(|e| (e.credential_id.as_str(), e.model.as_str()))
                .collect::<Vec<_>>(),
            vec![("cred_a", "llama-3.3-70b"), ("cred_a", "llama-3.1-8b")]
        );
    }

    #[test]
    fn an_entry_naming_a_deleted_template_falls_back_instead_of_failing() {
        let mut binding = CapabilityBinding {
            entries: vec![RotationEntry {
                prompt_id: Some("deleted".to_string()),
                ..entry("cred_a", "gpt-4o-mini")
            }],
            ..Default::default()
        };

        binding.prune_entries(|_| true, |id| id == "kept");

        assert_eq!(binding.entries[0].prompt_id, None);
    }

    #[test]
    fn a_partial_stored_binding_fills_in_defaults() {
        let binding: CapabilityBinding = serde_json::from_str(
            r#"{"enabled":true,"entries":[{"credential_id":"c1","model":"whisper-large-v3"}]}"#,
        )
        .unwrap();
        assert!(binding.enabled);
        assert_eq!(binding.entries[0].prompt_id, None);
        assert_eq!(binding.cooldown_secs, DEFAULT_COOLDOWN_SECS);
        assert!(binding.fallback_enabled);
    }
}
