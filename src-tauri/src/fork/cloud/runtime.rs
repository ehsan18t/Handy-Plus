//! Glue between the pure pool and the running app, kept out of `pool.rs` so
//! the rotation logic stays testable without an app handle.

use crate::fork::cloud::{Capability, CredentialPool, CredentialValidity, RotationStateStore};
use crate::settings::{get_settings, write_settings};
use log::{error, info, warn};
use serde::Serialize;
use specta::Type;
use std::sync::Arc;
use tauri::{AppHandle, Manager};
use tauri_specta::Event as _;

/// Emitted whenever output quality dropped. Silent degradation is worse than
/// failure: the user would blame the model instead of an exhausted key.
#[derive(Debug, Clone, Serialize, Type, tauri_specta::Event)]
pub struct CloudDegradedEvent {
    pub capability: String,
    pub outcome: CloudDegradeOutcome,
    /// What the UI shows, once translated.
    pub reason: CloudDegradeReason,
    /// Diagnostic detail for `handy.log` only, never rendered. Log-safe; never
    /// contains a key.
    pub detail: String,
}

#[derive(Debug, Clone, Copy, Serialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum CloudDegradeOutcome {
    FellBackToLocal,
    RawTranscript,
    /// Nothing ran and the dictation was lost.
    Failed,
}

/// Translatable cause. One variant per thing the user could act on.
#[derive(Debug, Clone, Copy, Serialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum CloudDegradeReason {
    NotConfigured,
    /// Keys are configured and fine, but all of them are benched right now, so
    /// nothing was sent. Separate from AllCredentialsFailed because the two
    /// need opposite reactions: wait, versus go and look at your keys.
    AllCredentialsCoolingDown,
    AllCredentialsFailed,
    RecordingTooLarge,
    TimedOut,
    Unexpected,
}

impl CloudDegradeReason {
    pub fn for_pool_error(error: &crate::fork::cloud::PoolError) -> Self {
        match error {
            crate::fork::cloud::PoolError::NotConfigured(_) => Self::NotConfigured,
            crate::fork::cloud::PoolError::AllCoolingDown { .. } => Self::AllCredentialsCoolingDown,
            crate::fork::cloud::PoolError::Exhausted { .. } => Self::AllCredentialsFailed,
        }
    }
}

pub fn build_pool(app: &AppHandle) -> anyhow::Result<Arc<CredentialPool>> {
    let app_data_dir = crate::portable::app_data_dir(app)?;
    let store = RotationStateStore::open_in(&app_data_dir)?;
    Ok(Arc::new(CredentialPool::new(Arc::new(store))))
}

pub fn pool(app: &AppHandle) -> Option<Arc<CredentialPool>> {
    app.try_state::<Arc<CredentialPool>>()
        .map(|state| state.inner().clone())
}

/// Only writes when something changed: every settings write reserializes the
/// whole object, and a no-op write per dictation is exactly the cost this fork
/// moved rotation state out of settings to avoid.
pub fn apply_validity_updates(app: &AppHandle, updates: &[(String, CredentialValidity)]) {
    if updates.is_empty() {
        return;
    }

    let mut settings = get_settings(app);
    let mut changed = false;

    for (credential_id, validity) in updates {
        if let Some(credential) = settings
            .cloud_credentials
            .iter_mut()
            .find(|c| &c.id == credential_id)
        {
            if credential.validity != *validity {
                if *validity == CredentialValidity::Invalid {
                    warn!(
                        "Credential '{}' was rejected by its provider and is now marked invalid",
                        credential.label
                    );
                }
                credential.validity = *validity;
                changed = true;
            }
        }
    }

    if changed {
        write_settings(app, settings);
    }
}

pub fn report_degraded(
    app: &AppHandle,
    capability: Capability,
    outcome: CloudDegradeOutcome,
    reason: CloudDegradeReason,
    detail: impl Into<String>,
) {
    let detail = detail.into();
    match outcome {
        CloudDegradeOutcome::Failed => {
            error!("Cloud {capability} failed with no fallback: {detail}")
        }
        _ => info!("Cloud {capability} degraded: {detail}"),
    }

    // Emitted through the generated typed event, not a literal: `collect_events!`
    // derives the channel name, and a hand-written string silently drifts from
    // it, leaving every fallback unreported.
    let payload = CloudDegradedEvent {
        capability: capability.as_key().to_string(),
        outcome,
        reason,
        detail,
    };
    if let Err(e) = payload.emit(app) {
        warn!("Failed to emit cloud degradation event: {e}");
    }
}
