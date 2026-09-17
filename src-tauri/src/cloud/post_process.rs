//! Post-processing through the credential pool.
//!
//! Sits beside upstream's single-key path rather than replacing it: with the
//! binding off, the caller falls through to upstream's code unchanged.

use crate::actions::{
    build_system_prompt, is_blank_transcription, strip_invisible_chars, strip_think_block,
    TRANSCRIPTION_FIELD,
};
use crate::cloud::pool::{plan, Attempt};
use crate::cloud::runtime::{
    apply_validity_updates, pool, report_degraded, CloudDegradeOutcome, CloudDegradeReason,
};
use crate::cloud::{ApiError, Capability};
use crate::settings::{AppSettings, LLMPrompt, APPLE_INTELLIGENCE_PROVIDER_ID};
use log::debug;
use tauri::AppHandle;

pub enum PooledPostProcess {
    /// Run upstream's single-key path instead.
    NotEngaged,
    Processed(String),
    /// Attempted and failed; the caller keeps the raw transcript.
    ///
    /// Unlike speech there is no user toggle here: returning the text
    /// unmodified is the only possible fallback, so a switch would only choose
    /// between keeping and losing the dictation.
    Degraded,
}

pub async fn run(
    app: &AppHandle,
    settings: &AppSettings,
    transcription: &str,
) -> PooledPostProcess {
    // Upstream's toggle is the single on/off: it already gates the sidebar entry
    // and the shortcut, and a second switch here would only be a way to have the
    // feature on and off at once.
    if !settings.post_process_enabled {
        return PooledPostProcess::NotEngaged;
    }

    if is_blank_transcription(transcription) {
        debug!("Pooled post-processing skipped: empty transcription");
        return PooledPostProcess::NotEngaged;
    }

    // An empty rotation means the user never opted into the pool, so upstream's
    // single-key path runs.
    if settings.cloud_bindings.post_process.entries.is_empty() {
        debug!("No rotation entries configured; using upstream post-processing");
        return PooledPostProcess::NotEngaged;
    }

    let degrade = |reason: CloudDegradeReason, detail: String| {
        report_degraded(
            app,
            Capability::PostProcess,
            CloudDegradeOutcome::RawTranscript,
            reason,
            detail,
        );
        PooledPostProcess::Degraded
    };

    let resolved = match plan(settings, Capability::PostProcess) {
        Ok(resolved) => resolved,
        // Entries exist but none is usable. Surfaced rather than falling through
        // to upstream's key, which would look like the rotation worked.
        Err(error) => {
            return degrade(
                CloudDegradeReason::for_pool_error(&error),
                error.to_string(),
            )
        }
    };

    let Some(pool) = pool(app) else {
        return degrade(
            CloudDegradeReason::Unexpected,
            "credential pool is unavailable".to_string(),
        );
    };

    // Instructions are resolved per entry: a prompt tuned for one model often
    // reads badly to another, which is the whole point of naming a template per
    // rotation entry.
    let templates = settings.post_process_prompts.clone();
    // Upstream's selected prompt is the fallback, so its existing picker keeps
    // meaning what it says instead of becoming a second, dead control.
    let default_prompt_id = settings.post_process_selected_prompt_id.clone();
    let transcription = transcription.to_string();

    let run = pool
        .execute(Capability::PostProcess, &resolved, |attempt: Attempt| {
            let transcription = transcription.clone();
            let instruction =
                resolve_instruction(&attempt, &templates, default_prompt_id.as_deref());
            async move {
                let Some(instruction) = instruction else {
                    return Err(ApiError::transport(
                        "no cleanup instruction is set for this credential",
                    ));
                };
                send(attempt, instruction, transcription).await
            }
        })
        .await;

    apply_validity_updates(app, &run.validity_updates);

    match run.result {
        Ok(text) => PooledPostProcess::Processed(text),
        Err(error) => degrade(
            CloudDegradeReason::for_pool_error(&error),
            error.to_string(),
        ),
    }
}

/// The entry's own template, falling back to the binding default.
fn resolve_instruction(
    attempt: &Attempt,
    templates: &[LLMPrompt],
    default_prompt_id: Option<&str>,
) -> Option<String> {
    let wanted = attempt.entry.prompt_id.as_deref().or(default_prompt_id)?;
    templates
        .iter()
        .find(|template| template.id == wanted)
        .map(|template| template.prompt.clone())
        .filter(|prompt| !prompt.trim().is_empty())
}

/// Mirrors upstream's request shape so both paths produce comparable output.
async fn send(
    attempt: Attempt,
    instruction: String,
    transcription: String,
) -> Result<String, ApiError> {
    let system_prompt = build_system_prompt(&instruction);

    // Apple Intelligence is reached through native APIs rather than HTTP, so it
    // cannot go through llm_client. Upstream's own dispatch lives in actions.rs;
    // the module it calls is shared, so this calls the same two functions rather
    // than duplicating them.
    if attempt.provider.id == APPLE_INTELLIGENCE_PROVIDER_ID {
        return apple_intelligence_completion(&system_prompt, &transcription, attempt.model());
    }

    // Reasoning adds seconds of latency and rarely improves cleanup.
    // `llm_client` retries without the fields when an endpoint rejects them and
    // remembers the rejection, so asking every provider is safe.
    let disable_reasoning = true;

    let raw = if attempt.provider.supports_structured_output {
        let schema = serde_json::json!({
            "type": "object",
            "properties": {
                (TRANSCRIPTION_FIELD): {
                    "type": "string",
                    "description": "The cleaned and processed transcription text"
                }
            },
            "required": [TRANSCRIPTION_FIELD],
            "additionalProperties": false
        });

        crate::llm_client::send_chat_completion_with_schema(
            &attempt.provider,
            attempt.secret.clone(),
            attempt.model(),
            transcription,
            Some(system_prompt),
            Some(schema),
            disable_reasoning,
        )
        .await?
    } else {
        let prompt = instruction.replace("${output}", &transcription);
        crate::llm_client::send_chat_completion(
            &attempt.provider,
            attempt.secret.clone(),
            attempt.model(),
            prompt,
            disable_reasoning,
        )
        .await?
    };

    // An empty answer is a failed call, not a successful empty cleanup:
    // treating it as success would blank the dictation.
    let Some(content) = raw else {
        return Err(ApiError::transport("provider returned no content"));
    };

    let content = strip_think_block(&content);

    if attempt.provider.supports_structured_output {
        if let Ok(json) = serde_json::from_str::<serde_json::Value>(content) {
            if let Some(text) = json.get(TRANSCRIPTION_FIELD).and_then(|t| t.as_str()) {
                return Ok(strip_invisible_chars(text));
            }
        }
        // Claimed structured output but did not deliver it. The body is still
        // the cleaned text in practice, so use it rather than discard a
        // successful call.
        debug!(
            "Provider '{}' returned no structured field; using the raw body",
            attempt.provider.id
        );
    }

    Ok(strip_invisible_chars(content))
}

/// Apple Intelligence runs on-device with no key and no endpoint; upstream
/// stores its token limit in the model field, so that is parsed here too.
fn apple_intelligence_completion(
    system_prompt: &str,
    transcription: &str,
    token_limit: &str,
) -> Result<String, ApiError> {
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    {
        if !crate::apple_intelligence::check_apple_intelligence_availability() {
            return Err(ApiError::transport(
                "Apple Intelligence is not currently available on this device",
            ));
        }

        let limit = token_limit.trim().parse::<i32>().unwrap_or(0);
        return crate::apple_intelligence::process_text_with_system_prompt(
            system_prompt,
            transcription,
            limit,
        )
        .map_err(|e| ApiError::transport(format!("Apple Intelligence failed: {e}")))
        .and_then(|result| {
            if result.trim().is_empty() {
                Err(ApiError::transport("Apple Intelligence returned nothing"))
            } else {
                Ok(strip_invisible_chars(&result))
            }
        });
    }

    #[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
    {
        let _ = (system_prompt, transcription, token_limit);
        Err(ApiError::transport(
            "Apple Intelligence is only available on Apple silicon Macs",
        ))
    }
}
