//! The credential pool: the one place that decides which key serves a request.
//!
//! [`plan`] resolves settings into eligible candidates and is Tauri-free, so
//! the rules are unit-testable. [`CredentialPool::execute`] drives the attempt
//! loop, taking the call itself as a closure: callers say *what* to do, the
//! pool owns *who* does it, in what order, and what a failure means. Adding a
//! policy therefore cannot touch a call site.

use crate::fork::cloud::binding::RotationEntry;
use crate::fork::cloud::now_ms;
use crate::fork::cloud::{
    ApiError, Capability, CapabilityBinding, CredentialValidity, FailureClass, RotationPolicy,
    RotationStateStore,
};
use crate::settings::{AppSettings, PostProcessProvider};
use log::{debug, info, warn};
use std::collections::HashSet;
use std::fmt;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Everything one call needs, handed to the caller's closure.
///
/// Carries the whole entry so the caller can resolve capability-specific
/// extras (which instruction template, for speech the vocabulary hints) without
/// the pool knowing what any of them mean.
#[derive(Clone)]
pub struct Attempt {
    pub entry: RotationEntry,
    pub provider: PostProcessProvider,
    pub credential_label: String,
    pub secret: String,
    /// Speech only. Empty means auto-detect.
    pub language: String,
    /// Speech only: shared vocabulary bias.
    pub vocabulary: String,
}

impl Attempt {
    pub fn credential_id(&self) -> &str {
        &self.entry.credential_id
    }

    pub fn model(&self) -> &str {
        &self.entry.model
    }
}

/// Hand-written so a stray `{:?}` cannot print the key.
impl fmt::Debug for Attempt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Attempt")
            .field("credential_label", &self.credential_label)
            .field("provider", &self.provider.id)
            .field("model", &self.entry.model)
            .field("secret", &"[REDACTED]")
            .finish()
    }
}

#[derive(Debug)]
pub enum PoolError {
    NotConfigured(&'static str),
    /// Keys are configured and usable, but every one is benched right now, so
    /// nothing was sent. Distinct from `Exhausted` on purpose: telling someone
    /// every key failed when none was tried sends them looking at the wrong key,
    /// which is exactly the wrong place.
    AllCoolingDown {
        retry_in: Option<Duration>,
    },
    Exhausted {
        attempted: usize,
        last_error: Option<ApiError>,
    },
}

impl fmt::Display for PoolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PoolError::NotConfigured(what) => write!(f, "not configured: {what}"),
            PoolError::AllCoolingDown { retry_in } => match retry_in {
                Some(wait) => write!(
                    f,
                    "every configured credential is cooling down; the first is free in {}s",
                    wait.as_secs()
                ),
                None => write!(f, "every configured credential is cooling down"),
            },
            PoolError::Exhausted {
                attempted,
                last_error,
            } => match last_error {
                Some(error) => write!(
                    f,
                    "all {attempted} credential(s) failed; last error: {error}"
                ),
                None => write!(f, "no credential was available to try"),
            },
        }
    }
}

pub struct PoolPlan {
    pub binding: CapabilityBinding,
    pub candidates: Vec<Attempt>,
}

/// Resolve settings into a candidate list.
///
/// Eligibility filtering happens here, before any policy runs, so a new policy
/// cannot reimplement it incorrectly. Cooldown is the one rule that cannot live
/// here because it needs the state store; [`CredentialPool::execute`] applies
/// it immediately before ordering.
///
/// Whether the capability is switched on at all is the caller's check: speech
/// owns its toggle, post-processing rides on upstream's `post_process_enabled`.
pub fn plan(settings: &AppSettings, capability: Capability) -> Result<PoolPlan, PoolError> {
    let binding = settings.cloud_bindings.get(capability).clone();
    if binding.entries.is_empty() {
        return Err(PoolError::NotConfigured("no credentials selected"));
    }

    let mut candidates: Vec<Attempt> = Vec::new();
    for entry in &binding.entries {
        let credential_id = &entry.credential_id;
        // Stale config is skipped rather than failing the whole request.
        let Some(credential) = settings
            .cloud_credentials
            .iter()
            .find(|c| &c.id == credential_id)
        else {
            debug!("Binding references unknown credential '{credential_id}'; skipping");
            continue;
        };

        if entry.model.trim().is_empty() {
            debug!(
                "Skipping '{}': no model set for this entry",
                credential.label
            );
            continue;
        }

        if credential.validity == CredentialValidity::Invalid {
            debug!(
                "Skipping '{}': marked invalid by a 401/403",
                credential.label
            );
            continue;
        }

        let Some(provider) = settings.post_process_provider(&credential.provider_id) else {
            debug!(
                "Skipping '{}': provider '{}' no longer exists",
                credential.label, credential.provider_id
            );
            continue;
        };

        if !crate::fork::cloud::providers::supports(provider, capability) {
            debug!(
                "Skipping '{}': provider '{}' does not serve {capability}",
                credential.label, provider.id
            );
            continue;
        }

        let secret = settings
            .cloud_credential_secrets
            .get(&credential.id)
            .cloned()
            .unwrap_or_default();

        if crate::fork::cloud::providers::requires_credential(provider) && secret.trim().is_empty()
        {
            debug!("Skipping '{}': no key stored", credential.label);
            continue;
        }

        // Same key with a different model is legitimate: providers meter per
        // model, so those are separate quotas. Same key AND same model is not,
        // and would break the retry budget and double-strike one bucket.
        if candidates.iter().any(|existing| {
            existing.entry.credential_id == credential.id && existing.entry.model == entry.model
        }) {
            debug!(
                "Skipping duplicate entry for {} on model {}",
                credential.label, entry.model
            );
            continue;
        }

        candidates.push(Attempt {
            entry: entry.clone(),
            credential_label: credential.label.clone(),
            provider: provider.clone(),
            secret,
            language: binding.language.clone(),
            vocabulary: binding.prompt.clone(),
        });
    }

    if candidates.is_empty() {
        return Err(PoolError::NotConfigured(
            "no selected credential can serve this capability",
        ));
    }

    Ok(PoolPlan {
        binding,
        candidates,
    })
}

/// Validity changes are returned rather than written, keeping this module free
/// of Tauri and of settings persistence.
pub struct PoolRun<T> {
    pub result: Result<T, PoolError>,
    pub validity_updates: Vec<(String, CredentialValidity)>,
}

pub struct CredentialPool {
    state: Arc<RotationStateStore>,
}

impl CredentialPool {
    pub fn new(state: Arc<RotationStateStore>) -> Self {
        Self { state }
    }

    pub fn state(&self) -> &RotationStateStore {
        &self.state
    }

    /// Indices into `plan.candidates`, in the order they should be tried.
    fn ordered(&self, capability: Capability, plan: &PoolPlan) -> Vec<usize> {
        let window = plan.binding.strike_window();
        let now = now_ms();
        let states = self.state.states_for(capability, window);

        let mut eligible: Vec<(usize, Option<i64>)> = Vec::new();
        for (index, candidate) in plan.candidates.iter().enumerate() {
            let state = states
                .get(&(
                    candidate.entry.credential_id.clone(),
                    candidate.entry.model.clone(),
                ))
                .cloned()
                .unwrap_or_default();
            if state.is_cooling_down(now) {
                debug!(
                    "Skipping '{}' for {capability}: cooling down",
                    candidate.credential_label
                );
                continue;
            }
            eligible.push((index, state.last_used_ms));
        }

        if eligible.is_empty() {
            return Vec::new();
        }

        match plan.binding.policy {
            RotationPolicy::RoundRobin => {
                // The cursor indexes the whole list, not the eligible subset, or
                // benching a key shifts every later starting point.
                let total = plan.candidates.len();
                let start = (self.state.cursor(capability) as usize) % total;
                let pivot = eligible
                    .iter()
                    .position(|(index, _)| *index >= start)
                    .unwrap_or(0);
                eligible.rotate_left(pivot);
                // Past the entry actually served, not by one slot: a benched
                // slot would burn a turn and repeat the next key.
                let served = eligible[0].0;
                self.state
                    .set_cursor(capability, ((served + 1) % total) as u64);
            }
            RotationPolicy::LeastRecentlyUsed => {
                // Never-used sorts first, so a key added mid-session is picked
                // up immediately instead of waiting for the cursor.
                eligible.sort_by_key(|(_, last_used)| last_used.unwrap_or(i64::MIN));
            }
        }

        eligible.into_iter().map(|(index, _)| index).collect()
    }

    /// How long until the first benched candidate is usable again.
    ///
    /// Only read when nothing is eligible, so the extra state read costs nothing
    /// on the path that matters.
    fn soonest_cooldown(&self, capability: Capability, plan: &PoolPlan) -> Option<Duration> {
        let now = now_ms();
        let states = self
            .state
            .states_for(capability, plan.binding.strike_window());
        plan.candidates
            .iter()
            .filter_map(|candidate| {
                states.get(&(
                    candidate.entry.credential_id.clone(),
                    candidate.entry.model.clone(),
                ))
            })
            .filter_map(|state| state.cooldown_remaining(now))
            .min()
    }

    /// Try eligible credentials until one succeeds, each at most once. Without
    /// that cap a provider-wide outage becomes an infinite retry loop. The
    /// chosen credential serves the entire call including any retries the
    /// operation performs internally; switching mid-flight would make failures
    /// unattributable.
    ///
    /// `deadline` bounds the whole rotation. It is checked between attempts
    /// rather than imposed from outside, because cancelling this future loses
    /// its `PoolRun`: a key marked invalid by a 401 earlier in the loop would
    /// never reach settings, and would be tried and rejected again on the next
    /// dictation.
    pub async fn execute<T, F, Fut>(
        &self,
        capability: Capability,
        plan: &PoolPlan,
        deadline: Option<Instant>,
        operation: F,
    ) -> PoolRun<T>
    where
        F: Fn(Attempt) -> Fut,
        Fut: std::future::Future<Output = Result<T, ApiError>>,
    {
        let mut validity_updates = Vec::new();
        let order = self.ordered(capability, plan);

        if order.is_empty() {
            // Candidates existed (`plan` errors otherwise) and none is eligible,
            // so every one is benched. Nothing was sent to any provider.
            return PoolRun {
                result: Err(PoolError::AllCoolingDown {
                    retry_in: self.soonest_cooldown(capability, plan),
                }),
                validity_updates,
            };
        }

        let threshold = plan.binding.effective_strike_threshold();
        let window = plan.binding.strike_window();
        let cooldown = plan.binding.cooldown();
        let mut last_error = None;
        let mut attempted = 0usize;
        // A rejected key is rejected for every model it serves, unlike a rate
        // limit.
        let mut rejected: HashSet<&str> = HashSet::new();

        for index in order {
            if deadline.is_some_and(|limit| Instant::now() >= limit) {
                warn!(
                    "Rotation budget for {capability} spent after {attempted} attempt(s); not trying the rest"
                );
                break;
            }
            let candidate = &plan.candidates[index];
            let credential_id = candidate.entry.credential_id.as_str();
            if rejected.contains(credential_id) {
                debug!(
                    "Skipping '{}': already rejected in this request",
                    candidate.credential_label
                );
                continue;
            }

            attempted += 1;
            let model = candidate.entry.model.as_str();
            self.state.mark_used(credential_id, capability, model);

            match operation(candidate.clone()).await {
                Ok(value) => {
                    self.state.clear_strikes(credential_id, capability, model);
                    validity_updates.push((credential_id.to_string(), CredentialValidity::Valid));
                    return PoolRun {
                        result: Ok(value),
                        validity_updates,
                    };
                }
                Err(error) => {
                    self.record_failure(capability, candidate, &error, threshold, window, cooldown);
                    if error.is_auth_failure() {
                        rejected.insert(credential_id);
                        validity_updates
                            .push((credential_id.to_string(), CredentialValidity::Invalid));
                    }
                    last_error = Some(error);
                }
            }
        }

        PoolRun {
            result: Err(PoolError::Exhausted {
                attempted,
                last_error,
            }),
            validity_updates,
        }
    }

    fn record_failure(
        &self,
        capability: Capability,
        candidate: &Attempt,
        error: &ApiError,
        threshold: u32,
        window: Duration,
        cooldown: Duration,
    ) {
        match error.class() {
            FailureClass::Unreachable => {
                // No strike: the failure says nothing about the key.
                debug!(
                    "Credential '{}' could not be reached for {capability}: {error}",
                    candidate.credential_label
                );
            }
            FailureClass::Transient(wait) => {
                // Bench for exactly the header duration with no strike, and move
                // to the next credential immediately: a user waiting on a
                // dictation cannot absorb a 30 second sleep.
                info!(
                    "Credential '{}' is rate limited for {capability}; benching it for {:?}",
                    candidate.credential_label, wait
                );
                self.state.start_cooldown(
                    &candidate.entry.credential_id,
                    capability,
                    &candidate.entry.model,
                    wait,
                );
            }
            FailureClass::Permanent => {
                warn!(
                    "Credential '{}' was rejected as invalid ({})",
                    candidate.credential_label, error
                );
            }
            FailureClass::Failure => {
                let strikes = self.state.record_strike(
                    &candidate.entry.credential_id,
                    capability,
                    &candidate.entry.model,
                    window,
                );
                debug!(
                    "Credential '{}' failed for {capability} ({strikes}/{threshold}): {error}",
                    candidate.credential_label
                );
                if strikes >= threshold {
                    info!(
                        "Credential '{}' reached {strikes} strikes for {capability}; cooling down for {:?}",
                        candidate.credential_label, cooldown
                    );
                    self.state.start_cooldown(
                        &candidate.entry.credential_id,
                        capability,
                        &candidate.entry.model,
                        cooldown,
                    );
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fork::cloud::{Credential, RotationStateStore};
    use crate::settings::get_default_settings;
    use std::sync::atomic::{AtomicUsize, Ordering};

    const CAP: Capability = Capability::PostProcess;
    const WINDOW: Duration = Duration::from_secs(3600);
    const MODEL: &str = "llama-3.3-70b";

    /// Settings with `count` Groq credentials wired into the post-process binding.
    fn settings_with(count: usize) -> AppSettings {
        let mut settings = get_default_settings();
        for index in 0..count {
            let id = format!("cred_{index}");
            settings
                .cloud_credentials
                .push(Credential::new(&id, format!("key {index}"), "groq"));
            settings
                .cloud_credential_secrets
                .insert(id.clone(), format!("secret-{index}"));
            settings
                .cloud_bindings
                .post_process
                .entries
                .push(entry_for(&id));
        }
        settings.cloud_bindings.post_process.enabled = true;
        settings
    }

    fn entry_for(credential_id: &str) -> RotationEntry {
        RotationEntry {
            credential_id: credential_id.to_string(),
            model: "llama-3.3-70b".to_string(),
            prompt_id: None,
        }
    }

    fn pool() -> (CredentialPool, Arc<RotationStateStore>) {
        let state = Arc::new(RotationStateStore::in_memory().unwrap());
        (CredentialPool::new(state.clone()), state)
    }

    fn ok_op(attempt: Attempt) -> impl std::future::Future<Output = Result<String, ApiError>> {
        std::future::ready(Ok(attempt.credential_id().to_string()))
    }

    fn fails_with(
        error: ApiError,
    ) -> impl Fn(Attempt) -> std::future::Ready<Result<String, ApiError>> {
        move |_| std::future::ready(Err(error.clone()))
    }

    #[tokio::test]
    async fn a_failure_that_never_reached_the_provider_costs_no_strike() {
        let settings = settings_with(2);
        let plan = plan(&settings, CAP).unwrap();
        let (pool, state) = pool();

        for _ in 0..5 {
            let run = pool
                .execute(
                    CAP,
                    &plan,
                    None,
                    fails_with(ApiError::transport("dns error")),
                )
                .await;
            assert!(run.result.is_err());
        }

        for index in 0..2 {
            let pair = state.pair_state(&format!("cred_{index}"), CAP, MODEL, WINDOW);
            assert_eq!(pair.recent_strikes, 0, "cred_{index} was struck");
            assert!(!pair.is_cooling_down(now_ms()), "cred_{index} was benched");
        }
    }

    #[tokio::test]
    async fn a_rejected_key_is_not_tried_again_on_its_other_models() {
        let mut settings = settings_with(1);
        let mut second = entry_for("cred_0");
        second.model = "llama-3.1-8b".to_string();
        settings.cloud_bindings.post_process.entries.push(second);

        let plan = plan(&settings, CAP).unwrap();
        assert_eq!(plan.candidates.len(), 2, "both models are candidates");

        let (pool, _state) = pool();
        let calls = AtomicUsize::new(0);
        let run = pool
            .execute(CAP, &plan, None, |_| {
                calls.fetch_add(1, Ordering::SeqCst);
                std::future::ready(Err::<String, _>(ApiError::from_status(401, None, "nope")))
            })
            .await;

        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(matches!(
            run.result,
            Err(PoolError::Exhausted { attempted: 1, .. })
        ));
    }

    #[tokio::test]
    async fn following_my_order_keeps_following_it_when_a_key_is_benched() {
        let settings = settings_with(3);
        let plan = plan(&settings, CAP).unwrap();
        let (pool, state) = pool();

        state.start_cooldown("cred_1", CAP, MODEL, Duration::from_secs(600));

        let mut served = Vec::new();
        for _ in 0..4 {
            let run = pool.execute(CAP, &plan, None, ok_op).await;
            served.push(run.result.unwrap());
        }

        assert_eq!(served, vec!["cred_0", "cred_2", "cred_0", "cred_2"]);
    }

    #[test]
    fn a_capability_that_cannot_run_reports_why() {
        assert!(matches!(
            plan(&get_default_settings(), CAP),
            Err(PoolError::NotConfigured(_))
        ));

        let mut no_model = settings_with(1);
        no_model.cloud_bindings.post_process.entries[0].model = "  ".to_string();
        assert!(matches!(
            plan(&no_model, CAP),
            Err(PoolError::NotConfigured(_))
        ));

        // Anthropic has no speech endpoint, so a perfectly valid key on it is
        // still not an STT candidate.
        let mut wrong_capability = settings_with(1);
        wrong_capability.cloud_credentials[0].provider_id = "anthropic".to_string();
        wrong_capability.cloud_bindings.stt = wrong_capability.cloud_bindings.post_process.clone();
        assert!(matches!(
            plan(&wrong_capability, Capability::Stt),
            Err(PoolError::NotConfigured(_))
        ));

        let mut no_secret = settings_with(1);
        no_secret
            .cloud_credential_secrets
            .insert("cred_0".to_string(), "   ".to_string());
        assert!(matches!(
            plan(&no_secret, CAP),
            Err(PoolError::NotConfigured(_))
        ));
    }

    #[test]
    fn unusable_entries_are_filtered_without_failing_the_request() {
        let mut settings = settings_with(2);
        settings.cloud_credentials[0].validity = CredentialValidity::Invalid;
        settings
            .cloud_bindings
            .post_process
            .entries
            .push(entry_for("cred_deleted"));
        // A duplicate would otherwise serve twice in one request.
        settings
            .cloud_bindings
            .post_process
            .entries
            .push(entry_for("cred_1"));

        let resolved = plan(&settings, CAP).unwrap();
        assert_eq!(resolved.candidates.len(), 1);
        assert_eq!(resolved.candidates[0].entry.credential_id, "cred_1");
    }

    #[test]
    fn a_provider_needing_no_key_survives_an_empty_secret() {
        // Custom pointed at a local Ollama.
        let mut settings = settings_with(1);
        settings.cloud_credentials[0].provider_id = "custom".to_string();
        settings
            .cloud_credential_secrets
            .insert("cred_0".to_string(), String::new());

        assert_eq!(plan(&settings, CAP).unwrap().candidates.len(), 1);
    }

    #[tokio::test]
    async fn round_robin_advances_on_every_request_not_only_on_failure() {
        let (pool, _state) = pool();
        let resolved = plan(&settings_with(3), CAP).unwrap();

        let mut served = Vec::new();
        for _ in 0..4 {
            served.push(
                pool.execute(CAP, &resolved, None, ok_op)
                    .await
                    .result
                    .unwrap(),
            );
        }
        assert_eq!(served, vec!["cred_0", "cred_1", "cred_2", "cred_0"]);
    }

    #[tokio::test]
    async fn least_recently_used_picks_the_coldest_key() {
        let (pool, state) = pool();
        let mut settings = settings_with(3);
        settings.cloud_bindings.post_process.policy = RotationPolicy::LeastRecentlyUsed;
        let resolved = plan(&settings, CAP).unwrap();

        state.mark_used("cred_0", CAP, MODEL);
        state.mark_used("cred_1", CAP, MODEL);

        assert_eq!(
            pool.execute(CAP, &resolved, None, ok_op)
                .await
                .result
                .unwrap(),
            "cred_2"
        );
    }

    #[tokio::test]
    async fn a_success_clears_strikes_and_marks_the_key_valid() {
        let (pool, state) = pool();
        let resolved = plan(&settings_with(1), CAP).unwrap();
        state.record_strike("cred_0", CAP, MODEL, WINDOW);

        let run = pool.execute(CAP, &resolved, None, ok_op).await;

        assert_eq!(run.result.unwrap(), "cred_0");
        assert_eq!(
            run.validity_updates,
            vec![("cred_0".to_string(), CredentialValidity::Valid)]
        );
        assert_eq!(
            state
                .pair_state("cred_0", CAP, MODEL, WINDOW)
                .recent_strikes,
            0
        );
    }

    #[tokio::test]
    async fn a_rate_limit_benches_for_the_header_duration_without_a_strike() {
        let (pool, state) = pool();
        let resolved = plan(&settings_with(2), CAP).unwrap();

        let calls = AtomicUsize::new(0);
        let run = pool
            .execute(CAP, &resolved, None, |attempt: Attempt| {
                let first = calls.fetch_add(1, Ordering::SeqCst) == 0;
                std::future::ready(if first {
                    Err(ApiError::from_status(
                        429,
                        Some(Duration::from_secs(45)),
                        "slow down",
                    ))
                } else {
                    Ok(attempt.credential_id().to_string())
                })
            })
            .await;

        // Moved on immediately rather than sleeping 45 seconds.
        assert_eq!(run.result.unwrap(), "cred_1");

        let benched = state.pair_state("cred_0", CAP, MODEL, WINDOW);
        assert!(benched.is_cooling_down(now_ms()));
        assert_eq!(benched.recent_strikes, 0, "busy is not broken");
    }

    #[tokio::test]
    async fn strikes_accumulate_and_bench_the_key_at_the_threshold() {
        let (pool, state) = pool();
        let mut settings = settings_with(1);
        settings.cloud_bindings.post_process.strike_threshold = 2;
        let resolved = plan(&settings, CAP).unwrap();
        let fail = fails_with(ApiError::from_status(500, None, "boom"));

        pool.execute(CAP, &resolved, None, &fail).await;
        assert!(!state
            .pair_state("cred_0", CAP, MODEL, WINDOW)
            .is_cooling_down(now_ms()));

        pool.execute(CAP, &resolved, None, &fail).await;
        assert!(state
            .pair_state("cred_0", CAP, MODEL, WINDOW)
            .is_cooling_down(now_ms()));
    }

    #[tokio::test]
    async fn each_credential_is_tried_once_then_the_request_fails() {
        let (pool, _state) = pool();
        let resolved = plan(&settings_with(3), CAP).unwrap();

        let calls = AtomicUsize::new(0);
        let run = pool
            .execute(CAP, &resolved, None, |_: Attempt| {
                calls.fetch_add(1, Ordering::SeqCst);
                std::future::ready(Err::<String, _>(ApiError::from_status(
                    401,
                    None,
                    "unauthorized",
                )))
            })
            .await;

        assert_eq!(calls.load(Ordering::SeqCst), 3);
        assert!(matches!(
            run.result,
            Err(PoolError::Exhausted { attempted: 3, .. })
        ));
        assert_eq!(run.validity_updates.len(), 3, "401 marks each key invalid");
        assert!(run
            .validity_updates
            .iter()
            .all(|(_, v)| *v == CredentialValidity::Invalid));
    }

    #[tokio::test]
    async fn a_fully_benched_capability_reports_exhaustion_while_the_other_keeps_serving() {
        let (pool, state) = pool();
        let mut settings = settings_with(1);
        settings.cloud_bindings.stt = settings.cloud_bindings.post_process.clone();
        state.start_cooldown("cred_0", CAP, MODEL, Duration::from_secs(600));

        let calls = AtomicUsize::new(0);
        let chat = plan(&settings, CAP).unwrap();
        let run = pool
            .execute(CAP, &chat, None, |attempt: Attempt| {
                calls.fetch_add(1, Ordering::SeqCst);
                std::future::ready(Ok(attempt.credential_id().to_string()))
            })
            .await;

        assert_eq!(calls.load(Ordering::SeqCst), 0, "nothing should be called");
        // Benched, not failed: nothing was sent, so saying every key failed
        // would point the user at a key that was never contacted.
        assert!(matches!(run.result, Err(PoolError::AllCoolingDown { .. })));

        let speech = plan(&settings, Capability::Stt).unwrap();
        assert_eq!(
            pool.execute(Capability::Stt, &speech, None, ok_op)
                .await
                .result
                .unwrap(),
            "cred_0"
        );
    }

    #[tokio::test]
    async fn each_entry_carries_its_own_model_and_instruction() {
        // Two providers never share a model id, so a binding-wide model would
        // fail every request on whichever provider does not own it.
        let (pool, _state) = pool();
        let mut settings = settings_with(2);
        settings.cloud_credentials[1].provider_id = "openai".to_string();
        {
            let entries = &mut settings.cloud_bindings.post_process.entries;
            entries[0].model = "llama-3.3-70b".to_string();
            entries[0].prompt_id = Some("terse".to_string());
            entries[1].model = "gpt-4o-mini".to_string();
            entries[1].prompt_id = Some("verbose".to_string());
        }
        let resolved = plan(&settings, CAP).unwrap();

        let describe = |attempt: Attempt| {
            std::future::ready(Ok(format!(
                "{}|{}",
                attempt.model(),
                attempt.entry.prompt_id.clone().unwrap_or_default()
            )))
        };

        assert_eq!(
            pool.execute(CAP, &resolved, None, describe)
                .await
                .result
                .unwrap(),
            "llama-3.3-70b|terse"
        );
        assert_eq!(
            pool.execute(CAP, &resolved, None, describe)
                .await
                .result
                .unwrap(),
            "gpt-4o-mini|verbose"
        );
    }
    /// The reported bug: key 1 gets benched, key 2 is added afterwards, and the
    /// next request must go to key 2 carrying key 2's secret.
    #[tokio::test]
    async fn a_key_added_after_another_was_benched_is_the_one_that_serves() {
        let (pool, state) = pool();

        let one = settings_with(1);
        let plan_one = plan(&one, CAP).unwrap();
        assert!(pool
            .execute(CAP, &plan_one, None, ok_op)
            .await
            .result
            .is_ok());

        state.start_cooldown("cred_0", CAP, MODEL, Duration::from_secs(600));

        let two = settings_with(2);
        let plan_two = plan(&two, CAP).unwrap();
        assert_eq!(plan_two.candidates.len(), 2, "both keys are candidates");

        let seen = std::sync::Mutex::new(Vec::new());
        let run = pool
            .execute(CAP, &plan_two, None, |a: Attempt| {
                seen.lock()
                    .unwrap()
                    .push((a.credential_id().to_string(), a.secret.clone()));
                std::future::ready(Ok::<_, ApiError>(a.credential_id().to_string()))
            })
            .await;

        let seen = seen.into_inner().unwrap();
        assert_eq!(
            seen.len(),
            1,
            "the benched key must not be called: {seen:?}"
        );
        assert_eq!(
            seen[0],
            ("cred_1".to_string(), "secret-1".to_string()),
            "the new key must serve, with its own secret"
        );
        assert_eq!(run.result.unwrap(), "cred_1");
    }
    /// Nothing is sent when every key is benched, and the error says exactly
    /// that. Reporting "all credentials failed" here sends the user to inspect a
    /// key that was never contacted.
    #[tokio::test]
    async fn every_key_benched_reports_cooling_down_not_failure() {
        let settings = settings_with(2);
        let plan = plan(&settings, CAP).unwrap();
        let (pool, state) = pool();

        for index in 0..2 {
            state.start_cooldown(
                &format!("cred_{index}"),
                CAP,
                MODEL,
                Duration::from_secs(600),
            );
        }

        let calls = AtomicUsize::new(0);
        let run = pool
            .execute(CAP, &plan, None, |_| {
                calls.fetch_add(1, Ordering::SeqCst);
                std::future::ready(Ok::<_, ApiError>(String::new()))
            })
            .await;

        assert_eq!(calls.load(Ordering::SeqCst), 0, "no provider was contacted");
        match run.result {
            Err(PoolError::AllCoolingDown { retry_in }) => {
                let wait = retry_in.expect("the remaining cooldown is reported");
                assert!(wait.as_secs() > 0 && wait.as_secs() <= 600, "{wait:?}");
            }
            other => panic!("expected AllCoolingDown, got {other:?}"),
        }
    }
}
