//! Tauri commands backing the Providers tab.
//!
//! Every command here flattens the fork's typed errors into `String`: the
//! frontend only ever displays them. Callers that need the classification talk
//! to the pool directly.

use crate::cloud::binding::{CapabilityBinding, MAX_COOLDOWN_SECS, MAX_STRIKE_THRESHOLD};
use crate::cloud::runtime::pool;
use crate::cloud::{new_credential_id, Capability, Credential, CredentialValidity};
use crate::settings::{get_settings, write_settings};
use serde::Serialize;
use specta::Type;
use tauri::AppHandle;

/// Not merged into the credentials list: a key can cool down for chat while
/// serving speech, so a single badge would be a lie.
#[derive(Debug, Clone, Serialize, Type)]
pub struct CredentialCapabilityStatus {
    pub credential_id: String,
    /// Identifies the rotation entry alongside `credential_id`: quotas are
    /// metered per model, so the same key has separate state per model.
    pub model: String,
    pub capability: String,
    /// Seconds of cooldown left, or 0 when the credential is available.
    pub cooldown_remaining_secs: u64,
    pub recent_strikes: u32,
    pub last_used_ms: Option<i64>,
    /// A revoked key is skipped on every request but has no cooldown and no
    /// strikes, so without this it would read as healthy here.
    pub validity: CredentialValidity,
}

#[tauri::command]
#[specta::specta]
pub fn add_cloud_credential(
    app: AppHandle,
    label: String,
    provider_id: String,
    secret: String,
) -> Result<String, String> {
    let label = label.trim().to_string();
    if label.is_empty() {
        return Err("A credential needs a label".to_string());
    }

    let mut settings = get_settings(&app);
    let Some(provider) = settings.post_process_provider(&provider_id) else {
        return Err(format!("Unknown provider '{provider_id}'"));
    };

    // `pool::plan` skips keyless credentials silently; reject at the point the
    // user can still act on it.
    if provider.requires_credential && secret.trim().is_empty() {
        return Err(format!("{} needs an API key", provider.label));
    }

    let id = new_credential_id(&settings.cloud_credentials);
    settings
        .cloud_credentials
        .push(Credential::new(&id, label, provider_id));
    settings.cloud_credential_secrets.insert(id.clone(), secret);
    write_settings(&app, settings);

    Ok(id)
}

/// Edit a credential in place.
///
/// A `None` secret leaves the stored key untouched, so the UI can save a label
/// or provider change without ever round-tripping the secret through the
/// frontend.
#[tauri::command]
#[specta::specta]
pub fn update_cloud_credential(
    app: AppHandle,
    id: String,
    label: Option<String>,
    provider_id: Option<String>,
    secret: Option<String>,
) -> Result<(), String> {
    let mut settings = get_settings(&app);

    if let Some(provider_id) = &provider_id {
        if settings.post_process_provider(provider_id).is_none() {
            return Err(format!("Unknown provider '{provider_id}'"));
        }
    }

    let Some(credential) = settings.cloud_credentials.iter_mut().find(|c| c.id == id) else {
        return Err(format!("Unknown credential '{id}'"));
    };

    if let Some(label) = label {
        let label = label.trim().to_string();
        if label.is_empty() {
            return Err("A credential needs a label".to_string());
        }
        credential.label = label;
    }

    let provider_changed = provider_id
        .as_ref()
        .is_some_and(|next| next != &credential.provider_id);
    if let Some(provider_id) = provider_id {
        credential.provider_id = provider_id;
    }

    let secret_changed = secret.is_some();
    if let Some(secret) = secret {
        settings.cloud_credential_secrets.insert(id.clone(), secret);
    }

    // Both invalidate the basis of the previous verdict: the key may have been
    // marked invalid precisely because its provider was wrong. Without this the
    // user fix has no effect and the pool keeps skipping it.
    if secret_changed || provider_changed {
        credential.validity = CredentialValidity::Untested;
        if let Some(pool) = pool(&app) {
            // Key-wide: every bucket's history was about the old key.
            pool.state().clear_all_cooldowns(&id);
        }
    }

    write_settings(&app, settings);
    Ok(())
}

#[tauri::command]
#[specta::specta]
pub fn delete_cloud_credential(app: AppHandle, id: String) -> Result<(), String> {
    let mut settings = get_settings(&app);
    settings.cloud_credentials.retain(|c| c.id != id);
    settings.cloud_credential_secrets.remove(&id);

    // Otherwise the id lingers as a dangling reference the pool skips forever.
    for capability in Capability::ALL {
        settings
            .cloud_bindings
            .get_mut(capability)
            .entries
            .retain(|entry| entry.credential_id != id);
    }
    write_settings(&app, settings);

    // So a later credential minted with the same id cannot inherit its history.
    if let Some(pool) = pool(&app) {
        pool.state().forget_credential(&id);
    }

    Ok(())
}

/// Verify a credential, and return the models it can reach.
///
/// One command rather than two: listing models and proving the key works are
/// the same `GET /models` call, and splitting them meant the model picker could
/// learn a key was rejected and then not say so. On several providers this
/// succeeds for a key with no chat or audio scope, so it proves authentication,
/// not capability.
#[tauri::command]
#[specta::specta]
pub async fn test_cloud_credential(app: AppHandle, id: String) -> Result<Vec<String>, String> {
    let settings = get_settings(&app);

    let credential = settings
        .cloud_credentials
        .iter()
        .find(|c| c.id == id)
        .ok_or_else(|| format!("Unknown credential '{id}'"))?
        .clone();

    let provider = settings
        .post_process_provider(&credential.provider_id)
        .ok_or_else(|| format!("Unknown provider '{}'", credential.provider_id))?
        .clone();

    let secret = settings
        .cloud_credential_secrets
        .get(&id)
        .cloned()
        .unwrap_or_default();

    if provider.requires_credential && secret.trim().is_empty() {
        return Err(format!("{} needs an API key", provider.label));
    }

    // Nothing to call: Apple Intelligence is not an HTTP provider, and some
    // custom endpoints do not list models. Neither is a failure.
    if provider.models_endpoint.is_none() {
        return Ok(Vec::new());
    }

    let outcome = crate::llm_client::fetch_models(&provider, secret).await;

    let mut settings = get_settings(&app);
    if let Some(credential) = settings.cloud_credentials.iter_mut().find(|c| c.id == id) {
        credential.validity = match &outcome {
            Ok(_) => CredentialValidity::Valid,
            Err(error) if error.is_auth_failure() => CredentialValidity::Invalid,
            // A network blip or a 500 says nothing about the key itself.
            Err(_) => credential.validity,
        };
    }
    write_settings(&app, settings);

    outcome.map_err(|e| e.to_string())
}

#[tauri::command]
#[specta::specta]
pub fn set_cloud_binding(
    app: AppHandle,
    capability: Capability,
    binding: CapabilityBinding,
) -> Result<(), String> {
    let mut settings = get_settings(&app);

    let mut binding = binding;
    binding.cooldown_secs = binding.cooldown_secs.clamp(0, MAX_COOLDOWN_SECS);
    binding.strike_threshold = binding.strike_threshold.clamp(1, MAX_STRIKE_THRESHOLD);
    // Upper bound too: `record_strike` casts this to i64, and a hand-edited
    // absurd value wrapped negative, which silently disabled striking outright.
    binding.strike_window_secs = binding.strike_window_secs.clamp(1, MAX_COOLDOWN_SECS);

    binding.prune_entries(
        |id| settings.cloud_credentials.iter().any(|c| c.id == id),
        |id| settings.post_process_prompts.iter().any(|p| p.id == id),
    );

    // Enabling cloud speech deliberately does not rewrite `overlay_style`:
    // streaming is already suppressed at runtime (see `actions.rs`), and
    // persisting it here would destroy the preference with no way back.

    *settings.cloud_bindings.get_mut(capability) = binding;
    write_settings(&app, settings);
    Ok(())
}

#[tauri::command]
#[specta::specta]
pub fn get_cloud_credential_status(
    app: AppHandle,
    capability: Capability,
) -> Result<Vec<CredentialCapabilityStatus>, String> {
    let settings = get_settings(&app);
    let binding = settings.cloud_bindings.get(capability);
    let window = binding.strike_window();

    let Some(pool) = pool(&app) else {
        return Ok(Vec::new());
    };

    let now = crate::cloud::now_ms();
    let states = pool.state().states_for(capability, window);

    // Per entry rather than per credential: one key serving two models has two
    // independent quotas, so it can be paused for one and ready for the other.
    Ok(binding
        .entries
        .iter()
        .map(|entry| {
            let state = states
                .get(&(entry.credential_id.clone(), entry.model.clone()))
                .cloned()
                .unwrap_or_default();
            let validity = settings
                .cloud_credentials
                .iter()
                .find(|c| c.id == entry.credential_id)
                .map(|c| c.validity)
                .unwrap_or_default();

            CredentialCapabilityStatus {
                credential_id: entry.credential_id.clone(),
                model: entry.model.clone(),
                capability: capability.as_key().to_string(),
                cooldown_remaining_secs: state
                    .cooldown_remaining(now)
                    .map(|d| d.as_secs())
                    .unwrap_or(0),
                recent_strikes: state.recent_strikes,
                last_used_ms: state.last_used_ms,
                validity,
            }
        })
        .collect())
}

/// Lift a cooldown by hand, so a just-fixed key does not need hours or a
/// restart to prove it works.
#[tauri::command]
#[specta::specta]
pub fn clear_cloud_cooldown(
    app: AppHandle,
    capability: Capability,
    credential_id: String,
    model: String,
) -> Result<(), String> {
    if let Some(pool) = pool(&app) {
        // Scoped to the row the button sits on. Clearing key-wide would also
        // un-bench a different model whose quota really is spent.
        pool.state()
            .clear_cooldown(&credential_id, capability, &model);
    }

    // The user is asserting the key is good again, so lift the invalid mark
    // that would otherwise keep it filtered out entirely.
    let mut settings = get_settings(&app);
    if let Some(credential) = settings
        .cloud_credentials
        .iter_mut()
        .find(|c| c.id == credential_id)
    {
        if credential.validity == CredentialValidity::Invalid {
            credential.validity = CredentialValidity::Untested;
            write_settings(&app, settings);
        }
    }

    Ok(())
}
