//! What the fork knows about upstream's providers, kept out of upstream's type.
//!
//! This used to be four extra fields on `PostProcessProvider`, which cost a
//! `Default` impl and a `..Default::default()` line inside each of upstream's
//! nine provider literals. Those literals are exactly what upstream edits when
//! it adds or changes a provider, so every one of those lines was a standing
//! conflict, and the `Default` impl stopped compiling whenever upstream added a
//! field of its own.
//!
//! None of it was ever user data. The metadata is static, and
//! `ensure_post_process_defaults` re-derived it from code on every load, so the
//! copy in the settings file was redundant the whole time. A store written by
//! the old build still carries those four keys; serde ignores them.

use crate::fork::cloud::Capability;
use crate::settings::{PostProcessProvider, APPLE_INTELLIGENCE_PROVIDER_ID};
use serde::{Deserialize, Serialize};
use specta::Type;

/// Speech endpoints, for providers that expose an OpenAI-compatible audio API.
const TRANSCRIPTIONS: &str = "/audio/transcriptions";
/// Speech-to-English is a different endpoint, not a parameter.
const TRANSLATIONS: &str = "/audio/translations";

/// The fork's view of one provider.
pub struct Meta {
    pub capabilities: &'static [Capability],
    pub stt_endpoint: Option<&'static str>,
    pub stt_translate_endpoint: Option<&'static str>,
    pub requires_credential: bool,
}

const POST_PROCESS_ONLY: Meta = Meta {
    capabilities: &[Capability::PostProcess],
    stt_endpoint: None,
    stt_translate_endpoint: None,
    requires_credential: true,
};

const SPEECH_AND_POST_PROCESS: Meta = Meta {
    capabilities: &[Capability::PostProcess, Capability::Stt],
    stt_endpoint: Some(TRANSCRIPTIONS),
    stt_translate_endpoint: Some(TRANSLATIONS),
    requires_credential: true,
};

/// Fork metadata for a provider id. Anything upstream adds that this does not
/// name is post-processing only, which is the safe default: a provider is never
/// sent audio because nobody got round to listing it.
pub fn meta(provider_id: &str) -> Meta {
    match provider_id {
        "openai" | "groq" => SPEECH_AND_POST_PROCESS,
        // Native Swift APIs, reached without HTTP and without a key.
        APPLE_INTELLIGENCE_PROVIDER_ID => Meta {
            requires_credential: false,
            ..POST_PROCESS_ONLY
        },
        // Upstream already treats a missing key here as legal, so rotation must
        // not skip it for being keyless.
        "custom" => Meta {
            requires_credential: false,
            ..SPEECH_AND_POST_PROCESS
        },
        _ => POST_PROCESS_ONLY,
    }
}

/// Whether this provider needs an API key at all.
pub fn requires_credential(provider: &PostProcessProvider) -> bool {
    meta(&provider.id).requires_credential
}

/// Whether this provider can actually serve `capability`.
pub fn supports(provider: &PostProcessProvider, capability: Capability) -> bool {
    let meta = meta(&provider.id);
    if !meta.capabilities.contains(&capability) {
        return false;
    }
    // Trusting the claim alone would produce requests to `{base_url}/None`.
    match capability {
        Capability::Stt => meta
            .stt_endpoint
            .is_some_and(|path| !path.trim().is_empty()),
        Capability::PostProcess => true,
    }
}

/// Fully-qualified speech URL, when this provider has one.
///
/// `translate` falls back to plain transcription rather than failing, for a
/// provider that transcribes but cannot translate.
pub fn stt_url(provider: &PostProcessProvider, translate: bool) -> Option<String> {
    let meta = meta(&provider.id);
    let path = if translate {
        meta.stt_translate_endpoint
            .filter(|path| !path.trim().is_empty())
            .or(meta.stt_endpoint)?
    } else {
        meta.stt_endpoint?
    };

    let path = path.trim();
    if path.is_empty() {
        return None;
    }
    let base = provider.base_url.trim_end_matches('/');
    Some(format!("{}/{}", base, path.trim_start_matches('/')))
}

/// What the settings UI needs to know about a provider, joined to its id.
///
/// The frontend used to read these off the provider objects in the settings
/// store. It reads them from `get_cloud_providers` instead, so nothing has to
/// be persisted to make them visible.
#[derive(Serialize, Deserialize, Debug, Clone, Type)]
pub struct ProviderInfo {
    pub id: String,
    /// Effective, not claimed: a capability appears here only when the provider
    /// has what it needs to serve it.
    pub capabilities: Vec<Capability>,
    pub requires_credential: bool,
}

pub fn info(provider: &PostProcessProvider) -> ProviderInfo {
    ProviderInfo {
        id: provider.id.clone(),
        capabilities: [Capability::PostProcess, Capability::Stt]
            .into_iter()
            .filter(|capability| supports(provider, *capability))
            .collect(),
        requires_credential: requires_credential(provider),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn provider(id: &str) -> PostProcessProvider {
        PostProcessProvider {
            id: id.to_string(),
            label: id.to_string(),
            base_url: "https://example.test/v1".to_string(),
            allow_base_url_edit: false,
            models_endpoint: Some("/models".to_string()),
            supports_structured_output: false,
        }
    }

    #[test]
    fn speech_providers_resolve_both_endpoints() {
        for id in ["openai", "groq", "custom"] {
            let p = provider(id);
            assert!(supports(&p, Capability::Stt), "{id} must serve speech");
            assert_eq!(
                stt_url(&p, false).as_deref(),
                Some("https://example.test/v1/audio/transcriptions"),
                "{id}"
            );
            assert_eq!(
                stt_url(&p, true).as_deref(),
                Some("https://example.test/v1/audio/translations"),
                "{id}"
            );
        }
    }

    #[test]
    fn providers_without_a_speech_endpoint_do_not_claim_speech() {
        for id in ["anthropic", "openrouter", "cerebras", "zai"] {
            let p = provider(id);
            assert!(
                !supports(&p, Capability::Stt),
                "{id} must not advertise speech-to-text"
            );
            assert_eq!(stt_url(&p, false), None, "{id}");
        }
    }

    #[test]
    fn an_unknown_provider_is_post_process_only() {
        // Upstream adding a provider must never silently enrol it in speech.
        let p = provider("something-upstream-added-later");
        assert!(supports(&p, Capability::PostProcess));
        assert!(!supports(&p, Capability::Stt));
        assert!(requires_credential(&p));
    }

    #[test]
    fn keyless_providers_are_named_explicitly() {
        assert!(!requires_credential(&provider(
            APPLE_INTELLIGENCE_PROVIDER_ID
        )));
        assert!(!requires_credential(&provider("custom")));
        assert!(requires_credential(&provider("openai")));
    }

    #[test]
    fn info_reports_effective_capabilities() {
        let groq = info(&provider("groq"));
        assert_eq!(
            groq.capabilities,
            vec![Capability::PostProcess, Capability::Stt]
        );
        let anthropic = info(&provider("anthropic"));
        assert_eq!(anthropic.capabilities, vec![Capability::PostProcess]);
    }
}
