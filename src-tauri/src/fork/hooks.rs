//! The seam between upstream files and this fork.
//!
//! Every call an upstream file makes into the fork goes through this module, and
//! every one of them is a single line at the call site. That is the whole point.
//! When upstream rewrites the function around a one-line call, the conflict is
//! mechanical: keep their new code, keep our line. When it rewrites the function
//! around a twenty-line inlined block, resolving it is a judgement call, and
//! judgement calls are where a merge quietly reverts a decision nobody
//! remembers making.
//!
//! The reasoning for *why* each hook exists lives here rather than at the call
//! site, so the upstream file stays as close to upstream as it can.
//!
//! Adding a hook: see `docs/FORK_RECIPE.md`.

use crate::fork::cloud;
use crate::managers::audio::AudioRecordingManager;
use crate::managers::transcription::TranscriptionManager;
use crate::settings::AppSettings;
use std::sync::Arc;
use tauri::{AppHandle, Manager};

/// The fork's Tauri commands and events, re-exported so `lib.rs` registers
/// them through the seam like everything else. A future feature adds its
/// commands here and `lib.rs` grows by one line each, never by an import.
pub use cloud::commands::*;
pub use cloud::runtime::CloudDegradedEvent;

pub use cloud::error::parse_retry_after;
/// Fork types that appear in upstream files: settings fields, an error type on
/// upstream call paths, the capability enum. Re-exported for the same reason as
/// the commands, so no upstream file ever names a path inside the fork.
pub use cloud::{ApiError, Capability, CapabilityBindings, Credential};

/// Build everything the fork needs at startup.
///
/// A failure here must never stop the app starting. The fork's features are all
/// opt-in, and without the pool the app simply behaves like vanilla Handy, which
/// is a far better outcome than refusing to launch over a feature the user may
/// not even have configured.
pub fn init(app: &AppHandle) {
    match cloud::runtime::build_pool(app) {
        Ok(pool) => {
            app.manage(pool);
        }
        Err(e) => log::error!(
            "Failed to initialize the credential pool: {e}. Cloud features will be unavailable."
        ),
    }
}

/// Clean up a transcript, through the rotation when one is configured.
///
/// An empty rotation runs upstream's original single-key path unchanged. That is
/// what stops an upgrade silently losing post-processing for someone who
/// configured it before this fork existed, and it is why provider configuration
/// stays on the API Keys page rather than being duplicated onto the
/// post-processing page.
///
/// `None` means the transcript is used as dictated: either upstream declined to
/// process it, or the rotation was exhausted and the user has already been told
/// why.
pub async fn post_process(app: &AppHandle, settings: &AppSettings, text: &str) -> Option<String> {
    use cloud::post_process::PooledPostProcess;

    match cloud::post_process::run(app, settings, text).await {
        PooledPostProcess::Processed(processed) => Some(processed),
        PooledPostProcess::NotEngaged => {
            crate::actions::post_process_transcription(settings, text).await
        }
        // Attempted and failed. The pool has already emitted the degradation.
        PooledPostProcess::Degraded => None,
    }
}

/// Start loading the local speech model unless a remote provider is going to
/// serve this dictation, and report which of the two happened.
///
/// Loading a multi-gigabyte model into VRAM on every dictation, for a path that
/// only runs if the whole rotation is exhausted, is not a trade worth making.
/// The fallback loads it at the point it actually needs it and pays the load
/// time only then.
///
/// The returned flag is also what suppresses live streaming, and that is not
/// cosmetic: cloud speech is batch-only, and a streaming engine finalizes first,
/// so its text would be used before the cloud path ever ran.
pub fn local_model_load_started(app: &AppHandle, settings: &AppSettings) -> bool {
    if cloud::stt::is_enabled(settings) {
        return false;
    }
    app.state::<Arc<TranscriptionManager>>()
        .initiate_model_load();
    true
}

/// Transcribe a recording, trying the rotation before the local engine.
///
/// Hooked in *above* `TranscriptionManager` rather than inside it. The manager
/// dispatches on a `LoadedEngine` enum matched at roughly fifteen sites, most of
/// which assume an in-process engine with a load lifecycle, a GPU device and an
/// unload timer. A remote endpoint has none of those, so a new variant would
/// mean editing every one of those sites to say "not applicable". One branch
/// here costs nothing and leaves the local path untouched. The trade is that
/// cloud models do not appear in the Models tab.
///
/// With the toggle off this is exactly `tm.transcribe(samples)`.
pub async fn transcribe(
    app: &AppHandle,
    samples: Vec<f32>,
    cancel_generation: u64,
) -> anyhow::Result<String> {
    use cloud::stt::CloudTranscription;

    let tm = app.state::<Arc<TranscriptionManager>>();
    let rm = app.state::<Arc<AudioRecordingManager>>();
    let settings = crate::settings::get_settings(app);

    // Wrapped like post-processing: a provider that accepts the connection and
    // then stalls would otherwise hold the pipeline for the whole rotation
    // budget with no way to abandon it.
    let attempt = crate::actions::complete_unless_cancelled(
        cloud::stt::transcribe(app, &settings, &samples),
        || rm.was_cancelled_since(cancel_generation),
    )
    .await;

    match attempt {
        Some(CloudTranscription::Transcribed(text)) => {
            Ok(tm.apply_text_post_processing(&text, &cloud::stt::output_language(&settings)))
        }
        // Fallback is off and the cloud path failed. The user asked to be told
        // rather than quietly served lower-quality local output.
        Some(CloudTranscription::Failed(reason)) => {
            Err(anyhow::anyhow!("Cloud transcription failed: {reason}"))
        }
        Some(CloudTranscription::NotEngaged) | Some(CloudTranscription::FallBackToLocal) => {
            // `cloud::stt::transcribe` errors rather than loading, and
            // `local_model_load_started` skipped the load when cloud speech was
            // going to serve this one.
            tm.initiate_model_load();
            tm.transcribe(samples)
        }
        // Cancelled mid-upload. The caller's own cancellation check tears down.
        None => Ok(String::new()),
    }
}
