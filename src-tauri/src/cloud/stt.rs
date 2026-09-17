//! Cloud speech-to-text, hooked in *above* `TranscriptionManager`.
//!
//! The manager dispatches on a `LoadedEngine` enum matched at ~15 sites, most
//! assuming an in-process engine with a load lifecycle, GPU device and unload
//! timer. A remote endpoint has none of those, so a new variant would mean
//! editing every site to say "not applicable". One branch at the async call
//! site above costs nothing and leaves the local path untouched. The trade is
//! that cloud models do not appear in the model selector.

use crate::audio_toolkit::constants::WHISPER_SAMPLE_RATE;
use crate::audio_toolkit::OutputLanguageEvidence;
use crate::cloud::pool::{plan, Attempt};
use crate::cloud::runtime::{
    apply_validity_updates, pool, report_degraded, CloudDegradeOutcome, CloudDegradeReason,
};
use crate::cloud::{ApiError, Capability};
use crate::settings::AppSettings;
use bytes::Bytes;
use log::{debug, info};
use std::io::Cursor;
use std::sync::OnceLock;
use std::time::Duration;
use tauri::AppHandle;

/// Groq and OpenAI both cap requests at 25 MB. At 32 KB/s that is about
/// thirteen minutes. Audio past this falls back rather than being truncated:
/// returning the first thirteen minutes of a twenty minute dictation as if it
/// were complete is the worse failure.
const MAX_UPLOAD_BYTES: usize = 24 * 1024 * 1024;
const UPLOAD_TIMEOUT: Duration = Duration::from_secs(180);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// Backstop on the whole rotation, for a provider that accepts the connection
/// and then stalls on every key in turn.
const TOTAL_BUDGET: Duration = Duration::from_secs(240);

/// One client for the process, so the connection pool and TLS session survive
/// between requests.
fn client() -> Result<&'static reqwest::Client, ApiError> {
    static CLIENT: OnceLock<Result<reqwest::Client, String>> = OnceLock::new();
    CLIENT
        .get_or_init(|| {
            reqwest::Client::builder()
                .timeout(UPLOAD_TIMEOUT)
                .connect_timeout(CONNECT_TIMEOUT)
                .build()
                .map_err(|e| format!("Failed to build HTTP client: {e}"))
        })
        .as_ref()
        .map_err(ApiError::transport)
}

pub enum CloudTranscription {
    /// Run the local model as usual.
    NotEngaged,
    Transcribed(String),
    FallBackToLocal,
    Failed(String),
}

/// An enabled toggle with an empty rotation would suppress streaming and warn
/// on every dictation without ever placing a call.
pub fn is_enabled(settings: &AppSettings) -> bool {
    let binding = &settings.cloud_bindings.stt;
    binding.enabled && !binding.entries.is_empty()
}

/// What the local pipeline knows about a cloud transcript's language, in
/// upstream's terms, so filler-word removal picks the same profile it would for
/// a locally produced transcript.
///
/// `translate` maps to English because that is what the translation endpoint
/// returns. A provider configured without one silently serves plain
/// transcription instead, and that transcript is then treated as English; the
/// cost is one wrong filler profile, which is the same risk upstream already
/// carries on its own `translate_to_english` path.
pub fn output_language(settings: &AppSettings) -> OutputLanguageEvidence {
    if settings.translate_to_english {
        return OutputLanguageEvidence::TranslatedToEnglish;
    }
    // The language the request actually carried, which is the binding's, not
    // `selected_language`: the local engine and the rotation are configured
    // separately.
    match settings.cloud_bindings.stt.language.trim() {
        "" => OutputLanguageEvidence::Unknown,
        language => OutputLanguageEvidence::UserSelected(language.to_string()),
    }
}

/// The recorder already produces mono f32 at 16 kHz, so this is a format wrap
/// rather than a conversion. Samples are clamped because resampling can
/// overshoot slightly, and wrapping would turn a loud sample into a full-scale
/// click of the opposite sign.
pub fn encode_wav(samples: &[f32]) -> Result<Vec<u8>, String> {
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: WHISPER_SAMPLE_RATE,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };

    let mut buffer = Cursor::new(Vec::new());
    {
        let mut writer = hound::WavWriter::new(&mut buffer, spec)
            .map_err(|e| format!("Failed to start WAV encoding: {e}"))?;
        for sample in samples {
            let value = (sample.clamp(-1.0, 1.0) * i16::MAX as f32).round() as i16;
            writer
                .write_sample(value)
                .map_err(|e| format!("Failed to encode audio: {e}"))?;
        }
        writer
            .finalize()
            .map_err(|e| format!("Failed to finalize WAV encoding: {e}"))?;
    }

    Ok(buffer.into_inner())
}

pub async fn transcribe(
    app: &AppHandle,
    settings: &AppSettings,
    samples: &[f32],
) -> CloudTranscription {
    if !is_enabled(settings) || samples.is_empty() {
        return CloudTranscription::NotEngaged;
    }

    let fallback = settings.cloud_bindings.stt.fallback_enabled;
    // Single place that decides between degrading and failing.
    let give_up = |reason: CloudDegradeReason, detail: String| -> CloudTranscription {
        if fallback {
            report_degraded(
                app,
                Capability::Stt,
                CloudDegradeOutcome::FellBackToLocal,
                reason,
                detail,
            );
            CloudTranscription::FallBackToLocal
        } else {
            report_degraded(
                app,
                Capability::Stt,
                CloudDegradeOutcome::Failed,
                reason,
                &detail,
            );
            CloudTranscription::Failed(detail)
        }
    };

    let resolved = match plan(settings, Capability::Stt) {
        Ok(resolved) => resolved,
        Err(error) => {
            return give_up(
                CloudDegradeReason::for_pool_error(&error),
                error.to_string(),
            )
        }
    };

    // `Bytes` so the per-attempt clone is a refcount bump, not up to 24 MB.
    let audio = match encode_wav(samples) {
        Ok(audio) => Bytes::from(audio),
        Err(error) => return give_up(CloudDegradeReason::Unexpected, error),
    };

    if audio.len() > MAX_UPLOAD_BYTES {
        return give_up(
            CloudDegradeReason::RecordingTooLarge,
            format!(
                "recording is {:.0}s ({:.1} MB), larger than the {} MB upload limit",
                samples.len() as f64 / WHISPER_SAMPLE_RATE as f64,
                audio.len() as f64 / (1024.0 * 1024.0),
                MAX_UPLOAD_BYTES / (1024 * 1024)
            ),
        );
    }

    let Some(pool) = pool(app) else {
        return give_up(
            CloudDegradeReason::Unexpected,
            "credential pool is unavailable".to_string(),
        );
    };

    debug!(
        "Sending {:.1}s of audio ({} KB) to cloud speech-to-text",
        samples.len() as f64 / WHISPER_SAMPLE_RATE as f64,
        audio.len() / 1024
    );

    let translate = settings.translate_to_english;

    let pooled = tokio::time::timeout(
        TOTAL_BUDGET,
        pool.execute(Capability::Stt, &resolved, |attempt: Attempt| {
            let audio = audio.clone();
            async move { send(attempt, audio, translate).await }
        }),
    )
    .await;

    let run = match pooled {
        Ok(run) => run,
        Err(_) => {
            return give_up(
                CloudDegradeReason::TimedOut,
                format!(
                    "cloud transcription exceeded its {}s budget",
                    TOTAL_BUDGET.as_secs()
                ),
            )
        }
    };

    apply_validity_updates(app, &run.validity_updates);

    match run.result {
        Ok(text) => {
            info!("Cloud speech-to-text returned {} chars", text.len());
            CloudTranscription::Transcribed(text)
        }
        Err(error) => give_up(
            CloudDegradeReason::for_pool_error(&error),
            error.to_string(),
        ),
    }
}

async fn send(attempt: Attempt, audio: Bytes, translate: bool) -> Result<String, ApiError> {
    let url = attempt.provider.stt_url(translate).ok_or_else(|| {
        ApiError::transport(format!(
            "provider '{}' has no speech endpoint",
            attempt.provider.id
        ))
    })?;

    let mut form = reqwest::multipart::Form::new()
        .text("model", attempt.model().to_string())
        // Plain text avoids a parse step and a class of schema drift between
        // providers.
        .text("response_format", "text")
        .part(
            "file",
            reqwest::multipart::Part::bytes(audio.to_vec())
                .file_name("audio.wav")
                .mime_str("audio/wav")
                .map_err(|e| ApiError::transport(format!("Invalid audio MIME type: {e}")))?,
        );

    // The translation endpoint takes no source language: it detects, then
    // renders English. Sending one is an error on OpenAI.
    if !translate && !attempt.language.trim().is_empty() {
        form = form.text("language", attempt.language.clone());
    }
    if !attempt.vocabulary.trim().is_empty() {
        form = form.text("prompt", attempt.vocabulary.clone());
    }

    let mut request = client()?.post(&url);
    if !attempt.secret.trim().is_empty() {
        request = request.bearer_auth(&attempt.secret);
    }

    // Never format the reqwest error directly: its Display appends the raw URL,
    // which on a custom endpoint can carry the key.
    let response = request
        .multipart(form)
        .send()
        .await
        .map_err(|e| crate::llm_client::report_reqwest_error("Speech request failed", &e))?;

    if !response.status().is_success() {
        return Err(
            ApiError::from_response("Speech request failed", response, &attempt.secret).await,
        );
    }

    let text = response.text().await.map_err(|e| {
        crate::llm_client::report_reqwest_error("Failed to read speech response", &e)
    })?;

    let text = text.trim();
    if text.is_empty() {
        // A failed call, not a silent recording: treating it as success would
        // swallow the dictation and give rotation nothing to react to.
        return Err(ApiError::transport("provider returned an empty transcript"));
    }

    Ok(text.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encoding_matches_what_the_upload_cap_assumes() {
        // MAX_UPLOAD_BYTES is derived from ~32 KB per second, so a change here
        // silently makes the cap describe a different duration.
        let one_second = vec![0.0_f32; WHISPER_SAMPLE_RATE as usize];
        let wav = encode_wav(&one_second).unwrap();

        assert!(
            (32_000..33_000).contains(&wav.len()),
            "expected ~32 KB/s, got {}",
            wav.len()
        );
    }

    #[test]
    fn samples_outside_the_nominal_range_clamp_instead_of_wrapping() {
        let wav = encode_wav(&[1.5, -1.5, 0.0]).unwrap();
        let samples: Vec<i16> = hound::WavReader::new(Cursor::new(wav))
            .unwrap()
            .into_samples::<i16>()
            .map(|s| s.unwrap())
            .collect();
        assert_eq!(samples, vec![i16::MAX, -i16::MAX, 0]);
    }
}
