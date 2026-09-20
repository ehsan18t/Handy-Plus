use crate::fork::hooks::{parse_retry_after, ApiError};
use crate::settings::PostProcessProvider;
use log::{debug, error, info, warn};
use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION, CONTENT_TYPE, REFERER, USER_AGENT};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashSet;
use std::error::Error as StdError;
use std::sync::{Mutex, OnceLock};

#[derive(Debug, Serialize)]
struct ChatMessage {
    role: String,
    content: String,
}

#[derive(Debug, Serialize)]
struct JsonSchema {
    name: String,
    strict: bool,
    schema: Value,
}

#[derive(Debug, Serialize)]
struct ResponseFormat {
    #[serde(rename = "type")]
    format_type: String,
    json_schema: JsonSchema,
}

#[derive(Debug, Serialize, Clone, Default, PartialEq)]
struct ReasoningConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    effort: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    exclude: Option<bool>,
}

/// Request fields used to ask an endpoint to skip reasoning/thinking.
/// Providers disagree on the field name and accepted values, so at most one of
/// these is set per request (see `reasoning_disable_params`).
#[derive(Debug, Serialize, Clone, Default, PartialEq)]
struct ReasoningParams {
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning_effort: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning: Option<ReasoningConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    thinking: Option<Value>,
}

impl ReasoningParams {
    fn is_empty(&self) -> bool {
        self.reasoning_effort.is_none() && self.reasoning.is_none() && self.thinking.is_none()
    }
}

/// Pick the reasoning-disable request fields an endpoint understands.
/// Unknown endpoints get the common OpenAI-style field; if they reject it,
/// the request is retried without it (see `send_chat_completion_with_schema`).
fn reasoning_disable_params(provider: &PostProcessProvider) -> ReasoningParams {
    let base_url = provider.base_url.to_lowercase();
    if base_url.contains("api.deepseek.com") {
        // DeepSeek rejects reasoning_effort "none" and uses its own field:
        // https://api-docs.deepseek.com/guides/thinking_mode
        ReasoningParams {
            thinking: Some(serde_json::json!({ "type": "disabled" })),
            ..Default::default()
        }
    } else if provider.id == "openrouter" {
        // OpenRouter nested object; exclude:true also keeps reasoning text out
        // of the response so it can't pollute structured-output JSON parsing
        ReasoningParams {
            reasoning: Some(ReasoningConfig {
                effort: Some("none".to_string()),
                exclude: Some(true),
            }),
            ..Default::default()
        }
    } else {
        ReasoningParams {
            reasoning_effort: Some("none".to_string()),
            ..Default::default()
        }
    }
}

/// Floor on the answer, so a one-word dictation still has room to come back
/// cleaned up rather than cut off.
const MIN_OUTPUT_TOKENS: u32 = 256;
/// Ceiling, so a very long dictation does not reserve an absurd budget. Past
/// this the answer is truncated and refused rather than used, which is the
/// honest outcome: the transcript survives untouched.
const MAX_OUTPUT_TOKENS: u32 = 4096;

/// How much room to reserve for the answer.
///
/// Without this the request carries no ceiling, and a provider metering output
/// tokens per minute then reserves whatever the model could theoretically emit.
/// Groq's free tier allows 1000 a minute and estimated 1005 to 2048 for
/// transcripts of 17 to 1330 characters, so every single cleanup was rejected
/// before a token was generated, on an account that was doing nothing else.
///
/// Cleanup rewrites its input, so the input's own length is the estimate, with
/// half again for a model that expands rather than trims. Four characters per
/// token is the usual rule of thumb for English and is deliberately rough: this
/// picks a reservation, and `finish_reason` catches it if the guess was low.
///
/// **This cannot rescue a dictation whose cleanup genuinely needs more output
/// than the account allows in a minute**, which on Groq's free tier is about
/// 2,300 characters, some four minutes of speech. Nothing here knows that
/// ceiling: it appears only in the rejection. Past it the call is refused, the
/// transcript is kept exactly as dictated, and the user is told the request was
/// too large, which is the honest outcome. Clamping to a guessed ceiling instead
/// would return cleanups chopped off mid-sentence.
fn output_token_cap(system_prompt: Option<&str>, user_content: &str) -> u32 {
    let chars = system_prompt.map_or(0, str::len) + user_content.len();
    let estimated_input_tokens = (chars / 4) as u32;
    estimated_input_tokens
        .saturating_mul(3)
        .saturating_div(2)
        .saturating_add(128)
        .clamp(MIN_OUTPUT_TOKENS, MAX_OUTPUT_TOKENS)
}

/// Whether the answer stopped because it hit a ceiling rather than because it
/// finished.
///
/// OpenAI and Groq say `length`; Anthropic-shaped compatibility layers say
/// `max_tokens`, Gemini's says `MAX_TOKENS`, and some gateways use
/// `model_length`. Matching only the first of those was the difference between
/// refusing a truncated cleanup and pasting it over the transcript.
fn stopped_at_the_limit(finish_reason: Option<&str>) -> bool {
    finish_reason.is_some_and(|reason| {
        matches!(
            reason.to_ascii_lowercase().as_str(),
            "length" | "max_tokens" | "max_output_tokens" | "model_length" | "token_limit"
        )
    })
}

/// Pick the field this endpoint understands for the output ceiling.
///
/// `max_tokens` is the one every OpenAI-compatible server has accepted for
/// years, including local ones, but OpenAI itself now rejects it for reasoning
/// models and wants `max_completion_tokens`. Groq takes both and documents the
/// newer one. Everything else, a local llama.cpp or Ollama included, only
/// reliably knows `max_tokens`, so it stays the default.
fn token_limit_params(provider: &PostProcessProvider, cap: u32) -> TokenLimitParams {
    match provider.id.as_str() {
        "openai" | "groq" => TokenLimitParams {
            max_completion_tokens: Some(cap),
            ..Default::default()
        },
        _ => TokenLimitParams {
            max_tokens: Some(cap),
            ..Default::default()
        },
    }
}

/// An optional request field an endpoint turned out not to accept.
///
/// Tracked separately, because conflating them is a live bug and not a
/// theoretical one: Groq takes `reasoning_effort` on its reasoning models and
/// answers 400 for it on the others, so a single flag would let one 400 on
/// `llama-3.3-70b` also drop the output ceiling, and dropping the ceiling is
/// exactly what made Groq reject every cleanup in the first place.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum OptionalField {
    Reasoning,
    TokenLimit,
}

/// Endpoints (base_url|model) that rejected one of the optional fields. Kept for
/// the lifetime of the process so every request after the first skips the doomed
/// attempt instead of paying a retry each time.
fn field_rejections() -> &'static Mutex<HashSet<(String, OptionalField)>> {
    static REJECTED: OnceLock<Mutex<HashSet<(String, OptionalField)>>> = OnceLock::new();
    REJECTED.get_or_init(|| Mutex::new(HashSet::new()))
}

fn endpoint_key(provider: &PostProcessProvider, model: &str) -> String {
    format!("{}|{}", provider.base_url.trim_end_matches('/'), model)
}

fn is_known_rejected(key: &str, field: OptionalField) -> bool {
    field_rejections()
        .lock()
        .map(|set| set.contains(&(key.to_string(), field)))
        .unwrap_or(false)
}

fn remember_rejection(key: &str, field: OptionalField) {
    if let Ok(mut set) = field_rejections().lock() {
        set.insert((key.to_string(), field));
    }
}

/// Which field a 400/422 body is complaining about.
///
/// Providers name the offending parameter ("Unrecognized request argument
/// supplied: reasoning_effort", "max_tokens is not supported with this model").
/// When the body names neither, both are dropped, because one more probe costs
/// another round trip on the dictation path.
fn fields_blamed_by(body: &str) -> Vec<OptionalField> {
    let body = body.to_ascii_lowercase();
    let reasoning = ["reasoning_effort", "\"reasoning\"", "thinking"]
        .iter()
        .any(|name| body.contains(name));
    let token_limit = ["max_completion_tokens", "max_tokens"]
        .iter()
        .any(|name| body.contains(name));

    match (reasoning, token_limit) {
        (true, false) => vec![OptionalField::Reasoning],
        (false, true) => vec![OptionalField::TokenLimit],
        _ => vec![OptionalField::Reasoning, OptionalField::TokenLimit],
    }
}

/// The ceiling on the answer. Providers disagree on the field name, and sending
/// both is itself an error on OpenAI, so at most one of these is set per request
/// (see `token_limit_params`).
#[derive(Debug, Serialize, Clone, Default, PartialEq)]
struct TokenLimitParams {
    #[serde(skip_serializing_if = "Option::is_none")]
    max_completion_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
}

impl TokenLimitParams {
    fn is_empty(&self) -> bool {
        self.max_completion_tokens.is_none() && self.max_tokens.is_none()
    }
}

#[derive(Debug, Serialize)]
struct ChatCompletionRequest {
    model: String,
    messages: Vec<ChatMessage>,
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    response_format: Option<ResponseFormat>,
    #[serde(flatten)]
    reasoning: ReasoningParams,
    #[serde(flatten)]
    token_limit: TokenLimitParams,
}

#[derive(Debug, Deserialize)]
struct ChatCompletionResponse {
    choices: Vec<ChatChoice>,
}

#[derive(Debug, Deserialize)]
struct ChatChoice {
    message: ChatMessageResponse,
    /// `"length"` means the answer stopped at the cap rather than finishing.
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ChatMessageResponse {
    content: Option<String>,
}

/// Build headers for API requests based on provider type
fn build_headers(provider: &PostProcessProvider, api_key: &str) -> Result<HeaderMap, ApiError> {
    let mut headers = HeaderMap::new();

    // Common headers
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    headers.insert(
        REFERER,
        HeaderValue::from_static("https://github.com/cjpais/Handy"),
    );
    headers.insert(
        USER_AGENT,
        HeaderValue::from_static("Handy/1.0 (+https://github.com/cjpais/Handy)"),
    );
    headers.insert("X-Title", HeaderValue::from_static("Handy"));

    // Provider-specific auth headers
    if !api_key.is_empty() {
        if provider.id == "anthropic" {
            headers.insert(
                "x-api-key",
                HeaderValue::from_str(api_key).map_err(|_| {
                    // The error's Display quotes the offending header value, so
                    // it is dropped entirely rather than sanitized: that value
                    // is the API key.
                    ApiError::transport(
                        "API key contains characters that are not valid in an HTTP header",
                    )
                })?,
            );
            headers.insert("anthropic-version", HeaderValue::from_static("2023-06-01"));
        } else {
            headers.insert(
                AUTHORIZATION,
                HeaderValue::from_str(&format!("Bearer {}", api_key)).map_err(|_| {
                    ApiError::transport(
                        "API key contains characters that are not valid in an HTTP header",
                    )
                })?,
            );
        }
    }

    Ok(headers)
}

/// Create an HTTP client with provider-specific headers
fn create_client(
    provider: &PostProcessProvider,
    api_key: &str,
) -> Result<reqwest::Client, ApiError> {
    let headers = build_headers(provider, api_key)?;
    // Deliberately no timeout, matching upstream.
    //
    // A timeout was added here and then removed: chat completions legitimately
    // run for minutes (a cold local Ollama loading weights, a reasoning model on
    // a queued free tier), and any fixed ceiling silently turns a slow success
    // into a dropped post-process with the raw transcript pasted instead. The
    // escape hatch already exists and is better: `complete_unless_cancelled`
    // wraps this whole path and polls cancellation every 25 ms, dropping the
    // request future, so a stalled connection never wedges dictation.
    reqwest::Client::builder()
        .default_headers(headers)
        .build()
        .map_err(|e| report_reqwest_error("Failed to build HTTP client", &e))
}

/// Format a bounded error source chain.
///
/// `reqwest::Error`'s Display implementation intentionally gives only a short
/// summary. Nested causes contain the useful transport details, such as a
/// certificate validation failure, an HTTP/2 error, or a connection reset.
/// Callers must skip source types whose Display text can quote payload data.
fn error_source_chain(error: &(dyn StdError + 'static)) -> Vec<String> {
    let mut causes = Vec::new();
    let mut source = error.source();

    // Defensive cap in case a third-party error exposes a cyclic source chain.
    for _ in 0..16 {
        let Some(cause) = source else {
            break;
        };
        causes.push(cause.to_string());
        source = cause.source();
    }

    causes
}

fn reqwest_error_kinds(error: &reqwest::Error) -> String {
    let mut kinds = Vec::new();

    if error.is_builder() {
        kinds.push("builder");
    }
    if error.is_connect() {
        kinds.push("connect");
    }
    if error.is_request() {
        kinds.push("request");
    }
    if error.is_redirect() {
        kinds.push("redirect");
    }
    if error.is_timeout() {
        kinds.push("timeout");
    }
    if error.is_status() {
        kinds.push("status");
    }
    if error.is_body() {
        kinds.push("body");
    }
    if error.is_decode() {
        kinds.push("decode");
    }
    if error.is_upgrade() {
        kinds.push("upgrade");
    }

    if kinds.is_empty() {
        "unknown".to_string()
    } else {
        kinds.join(", ")
    }
}

fn sanitized_url(url: &reqwest::Url) -> String {
    let mut url = url.clone();

    // Custom endpoints should not contain credentials or query-string tokens,
    // but omit them from diagnostics in case one does.
    let _ = url.set_username("");
    let _ = url.set_password(None);
    url.set_query(None);
    url.set_fragment(None);

    url.to_string()
}

fn sanitized_url_for_log(url: &str) -> String {
    reqwest::Url::parse(url)
        .map(|url| sanitized_url(&url))
        // Do not echo an invalid URL: the parse failure might have been caused
        // by sensitive data entered in the custom endpoint field.
        .unwrap_or_else(|_| "<invalid URL>".to_string())
}

/// Longest provider error body kept in a log line or a returned error.
///
/// Bodies are provider-controlled and occasionally enormous (an HTML error page
/// from a proxy). Truncating keeps one bad response from flooding the log file.
const MAX_ERROR_BODY: usize = 2000;

/// Strip the API key from provider-supplied text before it reaches a log or the UI.
///
/// Some endpoints echo the offending credential back in an auth-failure body.
/// The fork's rule is that keys never appear in logs, including error paths, and
/// an exact match on the key we just sent is the one redaction that is always
/// correct.
pub(crate) fn redact_and_truncate(text: &str, api_key: &str) -> String {
    let mut redacted = if api_key.trim().is_empty() {
        text.to_string()
    } else {
        text.replace(api_key, "[REDACTED]")
    };

    if redacted.len() > MAX_ERROR_BODY {
        // Truncate on a char boundary; provider bodies are not guaranteed ASCII.
        let cutoff = (0..=MAX_ERROR_BODY)
            .rev()
            .find(|index| redacted.is_char_boundary(*index))
            .unwrap_or(0);
        redacted.truncate(cutoff);
        redacted.push_str("… [truncated]");
    }

    redacted
}

pub(crate) fn report_reqwest_error(context: &str, error: &reqwest::Error) -> ApiError {
    let kinds = reqwest_error_kinds(error);
    let url = error
        .url()
        .map(sanitized_url)
        .map(|url| format!(", url: {url}"))
        .unwrap_or_default();

    // serde_json's error text can quote values from a malformed response. That
    // response may contain transcription content, so retain the useful decode
    // classification but never put its nested source in logs or UI errors.
    let causes = if error.is_decode() {
        Vec::new()
    } else {
        error_source_chain(error)
    };
    let cause_details = if !causes.is_empty() {
        format!(": caused by: {}", causes.join(" -> "))
    } else if error.url().is_none() {
        // Reqwest's short Display text is safe when it cannot append a raw URL.
        format!(": {error}")
    } else {
        // The sanitized URL is already included above. Avoid formatting the
        // original error because its Display implementation includes the raw URL.
        String::new()
    };

    let details = format!("{context} (kind: {kinds}{url}){cause_details}");
    error!("{details}");
    // A reqwest error means the exchange failed below the HTTP status layer
    // (connect, TLS, timeout, decode), so there is no status and no
    // `retry-after` to carry. The pool reads a missing status as `Unreachable`
    // and takes no strike: being offline says nothing about the key. Callers
    // that do want a strike, such as a 200 with an empty body, build an error
    // with a status instead of coming through here.
    ApiError::transport(details)
}

/// Send a chat completion request to an OpenAI-compatible API
/// Returns Ok(Some(content)) on success, Ok(None) if response has no content,
/// or Err on actual errors (HTTP, parsing, etc.)
pub async fn send_chat_completion(
    provider: &PostProcessProvider,
    api_key: String,
    model: &str,
    prompt: String,
    disable_reasoning: bool,
) -> Result<Option<String>, ApiError> {
    send_chat_completion_with_schema(
        provider,
        api_key,
        model,
        prompt,
        None,
        None,
        disable_reasoning,
    )
    .await
}

/// Send a chat completion request with structured output support.
/// When json_schema is provided, uses structured outputs mode.
/// system_prompt is used as the system message when provided.
///
/// When disable_reasoning is set, the request carries the reasoning-disable
/// fields the endpoint is expected to understand. Not every OpenAI-compatible
/// endpoint accepts them (DeepSeek, Gemini's compat layer, and some OpenRouter
/// upstreams reject with 400), so a 400/422 answer to such a request triggers
/// one retry without the fields, and the rejection is remembered per
/// (base_url, model) so later requests skip the failing attempt entirely.
pub async fn send_chat_completion_with_schema(
    provider: &PostProcessProvider,
    api_key: String,
    model: &str,
    user_content: String,
    system_prompt: Option<String>,
    json_schema: Option<Value>,
    disable_reasoning: bool,
) -> Result<Option<String>, ApiError> {
    let base_url = provider.base_url.trim_end_matches('/');
    let url = format!("{}/chat/completions", base_url);

    debug!(
        "Sending chat completion request to: {}",
        sanitized_url_for_log(&url)
    );

    let client = create_client(provider, &api_key)?;

    // Sized before the strings move into the message list below.
    let cap = output_token_cap(system_prompt.as_deref(), &user_content);

    // Build messages vector
    let mut messages = Vec::new();

    // Add system prompt if provided
    if let Some(system) = system_prompt {
        messages.push(ChatMessage {
            role: "system".to_string(),
            content: system,
        });
    }

    // Add user message
    messages.push(ChatMessage {
        role: "user".to_string(),
        content: user_content,
    });

    // Build response_format if schema is provided
    let response_format = json_schema.map(|schema| ResponseFormat {
        format_type: "json_schema".to_string(),
        json_schema: JsonSchema {
            name: "transcription_output".to_string(),
            strict: true,
            schema,
        },
    });

    let key = endpoint_key(provider, model);
    let reasoning = if disable_reasoning && !is_known_rejected(&key, OptionalField::Reasoning) {
        reasoning_disable_params(provider)
    } else {
        ReasoningParams::default()
    };
    let token_limit = if is_known_rejected(&key, OptionalField::TokenLimit) {
        TokenLimitParams::default()
    } else {
        token_limit_params(provider, cap)
    };

    let mut request_body = ChatCompletionRequest {
        model: model.to_string(),
        messages,
        stream: false,
        response_format,
        reasoning,
        token_limit,
    };

    let mut response = client
        .post(&url)
        .json(&request_body)
        .send()
        .await
        .map_err(|e| report_reqwest_error("HTTP request failed", &e))?;
    let mut status = response.status();
    debug!(
        "Chat completion response received with status {} over {:?} from {}",
        status,
        response.version(),
        sanitized_url(response.url())
    );

    // A 400/422 on a request carrying optional shaping fields is almost always
    // the endpoint rejecting one of them — retry once without any of them.
    //
    // Both are stripped together rather than bisected. Which field offended is
    // not recoverable from the body in general, a second probe costs another
    // round trip on the dictation path, and the two providers that meter output
    // per minute, where losing the cap would matter, both accept both fields.
    if !status.is_success()
        && matches!(status.as_u16(), 400 | 422)
        && !(request_body.reasoning.is_empty() && request_body.token_limit.is_empty())
    {
        let error_text = response
            .text()
            .await
            .map(|body| redact_and_truncate(&body, &api_key))
            .unwrap_or_else(|e| {
                report_reqwest_error("Failed to read field rejection response", &e).message
            });
        let blamed = fields_blamed_by(&error_text);
        info!(
            "Endpoint rejected an optional request field (status {}): {}. Retrying without {:?}",
            status, error_text, blamed
        );

        if blamed.contains(&OptionalField::Reasoning) {
            request_body.reasoning = ReasoningParams::default();
        }
        if blamed.contains(&OptionalField::TokenLimit) {
            request_body.token_limit = TokenLimitParams::default();
        }
        response = client
            .post(&url)
            .json(&request_body)
            .send()
            .await
            .map_err(|e| report_reqwest_error("HTTP retry failed", &e))?;
        status = response.status();
        debug!(
            "Chat completion retry response received with status {} over {:?} from {}",
            status,
            response.version(),
            sanitized_url(response.url())
        );

        if status.is_success() {
            info!(
                "Retry without {:?} succeeded; '{}' (model '{}') will skip them from now on",
                blamed,
                sanitized_url_for_log(base_url),
                model
            );
            for field in blamed {
                remember_rejection(&key, field);
            }
        }
    }

    if !status.is_success() {
        // Read `retry-after` before the body is consumed: `text()` takes the
        // response by value, and this header is the single fact that separates
        // a healthy rate-limited key from one that needs a strike.
        let retry_after = parse_retry_after(response.headers());
        let error_text = response
            .text()
            .await
            .map(|body| redact_and_truncate(&body, &api_key))
            .unwrap_or_else(|e| {
                report_reqwest_error("Failed to read API error response", &e).message
            });
        let error = ApiError::from_status(
            status.as_u16(),
            retry_after,
            format!("API request failed: {}", error_text),
        );
        error!("{error}");
        return Err(error);
    }

    let completion: ChatCompletionResponse = response
        .json()
        .await
        .map_err(|e| report_reqwest_error("Failed to parse API response", &e))?;

    let Some(choice) = completion.choices.first() else {
        return Ok(None);
    };
    let content = choice.message.content.clone();

    if !stopped_at_the_limit(choice.finish_reason.as_deref()) {
        return Ok(content);
    }

    // Nothing visible came back, so the ceiling was spent before the answer
    // started. That is a reasoning model thinking inside the same budget, not a
    // transcript too long to clean, and the old uncapped request served it fine.
    // Retry once without the ceiling and stop sending one here.
    if content.as_deref().is_none_or(|text| text.trim().is_empty()) {
        if request_body.token_limit.is_empty() {
            return Ok(content);
        }
        warn!(
            "'{}' (model '{}') spent the whole {cap} token ceiling before answering; retrying without one",
            sanitized_url_for_log(base_url),
            model
        );
        remember_rejection(&key, OptionalField::TokenLimit);
        request_body.token_limit = TokenLimitParams::default();

        let retry = client
            .post(&url)
            .json(&request_body)
            .send()
            .await
            .map_err(|e| report_reqwest_error("HTTP retry without a ceiling failed", &e))?;
        if !retry.status().is_success() {
            return Err(ApiError::from_response("API request failed", retry, &api_key).await);
        }
        let completion: ChatCompletionResponse = retry
            .json()
            .await
            .map_err(|e| report_reqwest_error("Failed to parse API response", &e))?;
        return Ok(completion
            .choices
            .first()
            .and_then(|choice| choice.message.content.clone()));
    }

    // Visible text that stops mid-sentence is worse than no cleanup at all: the
    // caller writes it over the transcript and the tail of the dictation is
    // gone. The request is what was wrong, so this must not strike the key.
    let error = ApiError::request_rejected(format!(
        "answer stopped at the {cap} token output ceiling instead of finishing"
    ));
    warn!("{error}");
    Err(error)
}

/// Fetch available models from an OpenAI-compatible API
/// Returns a list of model IDs
pub async fn fetch_models(
    provider: &PostProcessProvider,
    api_key: String,
) -> Result<Vec<String>, ApiError> {
    let base_url = provider.base_url.trim_end_matches('/');
    let url = format!("{}/models", base_url);

    debug!("Fetching models from: {}", sanitized_url_for_log(&url));

    let client = create_client(provider, &api_key)?;

    let response = client
        .get(&url)
        .send()
        .await
        .map_err(|e| report_reqwest_error("Failed to fetch models", &e))?;

    let status = response.status();
    debug!(
        "Model list response received with status {} over {:?} from {}",
        status,
        response.version(),
        sanitized_url(response.url())
    );
    if !status.is_success() {
        let retry_after = parse_retry_after(response.headers());
        let error_text = response
            .text()
            .await
            .map(|body| redact_and_truncate(&body, &api_key))
            .unwrap_or_else(|e| {
                report_reqwest_error("Failed to read model list error", &e).message
            });
        let error = ApiError::from_status(
            status.as_u16(),
            retry_after,
            format!("Model list request failed: {}", error_text),
        );
        error!("{error}");
        return Err(error);
    }

    let parsed: serde_json::Value = response
        .json()
        .await
        .map_err(|e| report_reqwest_error("Failed to parse model list response", &e))?;

    let mut models = Vec::new();

    // Handle OpenAI format: { data: [ { id: "..." }, ... ] }
    if let Some(data) = parsed.get("data").and_then(|d| d.as_array()) {
        for entry in data {
            if let Some(id) = entry.get("id").and_then(|i| i.as_str()) {
                models.push(id.to_string());
            } else if let Some(name) = entry.get("name").and_then(|n| n.as_str()) {
                models.push(name.to_string());
            }
        }
    }
    // Handle array format: [ "model1", "model2", ... ]
    else if let Some(array) = parsed.as_array() {
        for entry in array {
            if let Some(model) = entry.as_str() {
                models.push(model.to_string());
            }
        }
    }

    Ok(models)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fmt;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[derive(Debug)]
    struct TestError {
        message: &'static str,
        source: Option<Box<TestError>>,
    }

    impl fmt::Display for TestError {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str(self.message)
        }
    }

    impl StdError for TestError {
        fn source(&self) -> Option<&(dyn StdError + 'static)> {
            self.source
                .as_deref()
                .map(|source| source as &(dyn StdError + 'static))
        }
    }

    fn provider(id: &str, base_url: &str) -> PostProcessProvider {
        PostProcessProvider {
            id: id.to_string(),
            label: id.to_string(),
            base_url: base_url.to_string(),
            allow_base_url_edit: true,
            models_endpoint: None,
            supports_structured_output: false,
        }
    }

    fn request_json(reasoning: ReasoningParams) -> Value {
        request_json_with(reasoning, TokenLimitParams::default())
    }

    fn request_json_with(reasoning: ReasoningParams, token_limit: TokenLimitParams) -> Value {
        let request = ChatCompletionRequest {
            model: "test-model".to_string(),
            messages: vec![ChatMessage {
                role: "user".to_string(),
                content: "hi".to_string(),
            }],
            stream: false,
            response_format: None,
            reasoning,
            token_limit,
        };
        serde_json::to_value(&request).unwrap()
    }

    async fn serve_one_response(status: &str, body: &str) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let response = format!(
            "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );

        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0_u8; 2048];
            let _ = stream.read(&mut request).await.unwrap();
            stream.write_all(response.as_bytes()).await.unwrap();
        });

        format!("http://{address}")
    }

    #[test]
    fn error_source_chain_includes_all_nested_causes() {
        let error = TestError {
            message: "request failed",
            source: Some(Box::new(TestError {
                message: "TLS handshake failed",
                source: Some(Box::new(TestError {
                    message: "unknown certificate authority",
                    source: None,
                })),
            })),
        };

        assert_eq!(
            error_source_chain(&error),
            vec!["TLS handshake failed", "unknown certificate authority"]
        );
    }

    #[test]
    fn log_url_sanitization_removes_credentials_and_tokens() {
        let url = "https://user:password@example.com/v1/models?api_key=secret#private";
        assert_eq!(sanitized_url_for_log(url), "https://example.com/v1/models");
    }

    #[test]
    fn invalid_log_urls_are_not_echoed() {
        assert_eq!(
            sanitized_url_for_log("not a URL containing secret"),
            "<invalid URL>"
        );
    }

    #[tokio::test]
    async fn decode_error_does_not_echo_response_values() {
        let base_url =
            serve_one_response("200 OK", r#"{"choices":"PRIVATE TRANSCRIPTION CONTENT"}"#).await;
        let error = reqwest::get(base_url)
            .await
            .unwrap()
            .json::<ChatCompletionResponse>()
            .await
            .unwrap_err();

        let details = report_reqwest_error("Failed to parse API response", &error).message;
        assert!(details.contains("kind: decode"));
        assert!(!details.contains("PRIVATE TRANSCRIPTION CONTENT"));
    }

    #[tokio::test]
    async fn raw_error_url_is_not_reintroduced_without_a_source() {
        let base_url = serve_one_response("400 Bad Request", "bad request").await;
        let error = reqwest::get(format!(
            "{base_url}/private?api_key=SECRET_QUERY_TOKEN#private"
        ))
        .await
        .unwrap()
        .error_for_status()
        .unwrap_err();

        let details = report_reqwest_error("Request failed", &error).message;
        assert!(details.contains(&format!("url: {base_url}/private")));
        assert!(!details.contains("SECRET_QUERY_TOKEN"));
        assert!(!details.contains("#private"));
    }

    #[test]
    fn error_bodies_never_echo_the_api_key() {
        let body = r#"{"error":"invalid key sk-live-DEADBEEF supplied"}"#;
        let redacted = redact_and_truncate(body, "sk-live-DEADBEEF");
        assert!(!redacted.contains("sk-live-DEADBEEF"));
        assert!(redacted.contains("[REDACTED]"));
    }

    #[test]
    fn empty_api_keys_do_not_redact_everything() {
        // `str::replace` with an empty needle inserts the replacement between
        // every character, which would destroy the diagnostic entirely.
        let body = "upstream returned 502";
        assert_eq!(redact_and_truncate(body, ""), body);
        assert_eq!(redact_and_truncate(body, "   "), body);
    }

    #[test]
    fn oversized_error_bodies_are_truncated_on_a_char_boundary() {
        let body = "é".repeat(MAX_ERROR_BODY);
        let redacted = redact_and_truncate(&body, "unused");
        assert!(redacted.ends_with("… [truncated]"));
        assert!(redacted.len() < body.len());
    }

    #[test]
    fn requests_explicitly_disable_streaming() {
        let json = request_json(ReasoningParams::default());
        assert_eq!(json["stream"], false);
    }

    #[test]
    fn default_reasoning_params_serialize_to_no_fields() {
        let json = request_json(ReasoningParams::default());
        assert!(json.get("reasoning_effort").is_none());
        assert!(json.get("reasoning").is_none());
        assert!(json.get("thinking").is_none());
    }

    #[test]
    fn custom_provider_uses_top_level_reasoning_effort() {
        let params = reasoning_disable_params(&provider("custom", "http://localhost:11434/v1"));
        let json = request_json(params);
        assert_eq!(json["reasoning_effort"], "none");
        assert!(json.get("reasoning").is_none());
        assert!(json.get("thinking").is_none());
    }

    #[test]
    fn openrouter_uses_nested_reasoning_object() {
        let params =
            reasoning_disable_params(&provider("openrouter", "https://openrouter.ai/api/v1"));
        let json = request_json(params);
        assert!(json.get("reasoning_effort").is_none());
        assert_eq!(json["reasoning"]["effort"], "none");
        assert_eq!(json["reasoning"]["exclude"], true);
        assert!(json.get("thinking").is_none());
    }

    #[test]
    fn deepseek_base_url_uses_thinking_disabled() {
        let params = reasoning_disable_params(&provider("custom", "https://api.deepseek.com"));
        let json = request_json(params);
        assert!(json.get("reasoning_effort").is_none());
        assert!(json.get("reasoning").is_none());
        assert_eq!(json["thinking"]["type"], "disabled");
    }

    #[test]
    fn reasoning_params_is_empty_tracks_all_fields() {
        assert!(ReasoningParams::default().is_empty());
        assert!(!ReasoningParams {
            reasoning_effort: Some("none".to_string()),
            ..Default::default()
        }
        .is_empty());
        assert!(!ReasoningParams {
            thinking: Some(serde_json::json!({ "type": "disabled" })),
            ..Default::default()
        }
        .is_empty());
    }

    #[test]
    fn rejection_memo_is_keyed_by_base_url_and_model() {
        let deepseek = provider("custom", "https://api.deepseek.com/");
        let key = endpoint_key(&deepseek, "deepseek-chat");
        assert_eq!(key, "https://api.deepseek.com|deepseek-chat");
        assert!(!is_known_rejected(&key, OptionalField::Reasoning));
        remember_rejection(&key, OptionalField::Reasoning);
        assert!(is_known_rejected(&key, OptionalField::Reasoning));
        // A different model on the same endpoint is tracked separately
        assert!(!is_known_rejected(
            &endpoint_key(&deepseek, "other-model"),
            OptionalField::Reasoning
        ));
        // And so is the other field: an endpoint that rejects `reasoning_effort`
        // must keep its output ceiling, or losing it re-creates the rejection
        // the ceiling exists to prevent.
        assert!(!is_known_rejected(&key, OptionalField::TokenLimit));
    }

    #[test]
    fn a_rejection_is_blamed_on_the_field_the_body_names() {
        use OptionalField::*;

        // Groq on a non-reasoning model.
        assert_eq!(
            fields_blamed_by(
                "{\"error\":{\"message\":\"'reasoning_effort' is not supported with this model\"}}"
            ),
            vec![Reasoning]
        );
        // OpenAI on a reasoning model.
        assert_eq!(
            fields_blamed_by(
                "{\"error\":{\"message\":\"Unsupported parameter: 'max_tokens' is not supported \
                 with this model. Use 'max_completion_tokens' instead.\"}}"
            ),
            vec![TokenLimit]
        );
        // Nothing named: drop both rather than pay another probe.
        assert_eq!(fields_blamed_by("Bad Request"), vec![Reasoning, TokenLimit]);
    }

    #[test]
    fn an_answer_cut_off_is_recognised_whatever_the_provider_calls_it() {
        for reason in [
            "length",
            "max_tokens",
            "MAX_TOKENS",
            "model_length",
            "token_limit",
        ] {
            assert!(stopped_at_the_limit(Some(reason)), "{reason}");
        }
        for reason in ["stop", "tool_calls", "content_filter"] {
            assert!(!stopped_at_the_limit(Some(reason)), "{reason}");
        }
        assert!(!stopped_at_the_limit(None));
    }

    #[test]
    fn every_request_carries_an_output_ceiling() {
        // Without one, a provider metering output per minute reserves whatever
        // the model could emit and rejects the call before generating anything.
        for id in ["openai", "groq"] {
            let json = request_json_with(
                ReasoningParams::default(),
                token_limit_params(&provider(id, "https://example.test/v1"), 640),
            );
            assert_eq!(json["max_completion_tokens"], 640, "{id}");
            assert!(json.get("max_tokens").is_none(), "{id} must send only one");
        }

        // Local and unknown endpoints only reliably know the older field.
        for id in ["custom", "openrouter", "cerebras"] {
            let json = request_json_with(
                ReasoningParams::default(),
                token_limit_params(&provider(id, "http://localhost:11434/v1"), 640),
            );
            assert_eq!(json["max_tokens"], 640, "{id}");
            assert!(
                json.get("max_completion_tokens").is_none(),
                "{id} must send only one"
            );
        }
    }

    #[test]
    fn the_ceiling_leaves_room_to_rewrite_the_input() {
        // The failing case from the log: a 1330 character transcript was
        // estimated at 2048 output tokens by the provider and rejected against a
        // 1000 per minute limit. Sizing it ourselves has to land under that.
        let transcript = "a".repeat(1330);
        let cap = output_token_cap(None, &transcript);
        assert!(
            (MIN_OUTPUT_TOKENS..1000).contains(&cap),
            "1330 chars produced {cap}"
        );

        // A one word dictation still gets room to come back cleaned up.
        assert_eq!(output_token_cap(None, "hi"), MIN_OUTPUT_TOKENS);

        // And nothing reserves an unbounded budget.
        assert_eq!(
            output_token_cap(None, &"a".repeat(2_000_000)),
            MAX_OUTPUT_TOKENS
        );
    }

    #[test]
    fn a_long_dictation_asks_for_more_than_a_free_tier_minute_allows() {
        // Pinning the documented limit rather than a fix. Past roughly 2,300
        // characters the cap exceeds Groq's 1000 per minute and the provider
        // refuses the call. That path keeps the transcript and strikes nobody
        // (see `FailureClass::RequestRejected`); this test exists so the cliff
        // is a known number rather than a surprise.
        assert!(output_token_cap(None, &"a".repeat(2_200)) <= 1000);
        assert!(output_token_cap(None, &"a".repeat(2_400)) > 1000);
    }
}
