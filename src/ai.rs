//! AI layer shared by every AI-assisted command.
//!
//! `complete` sends the request to the model and hands the raw reply to the
//! caller's `parse` closure. A reply that fails to parse counts as a failure,
//! so callers fall back to their manual flow instead of aborting.

use anyhow::{Context, Result};
use std::env;
use std::sync::OnceLock;
use std::time::Duration;

const DEFAULT_OPENAI_URL: &str = "https://api.openai.com/v1";
const DEFAULT_OPENAI_MODEL: &str = "gpt-5.4-mini";
const DEFAULT_OPENAI_TIMEOUT_SECS: u64 = 120;

/// Enough of an error payload to identify the problem, without pasting an
/// HTML error page into the terminal.
const MAX_ERROR_BODY_CHARS: usize = 400;

/// One structured request: a system prompt, a user prompt, and the JSON schema
/// the reply must satisfy (enforced natively by the Responses API).
pub(crate) struct Request<'a> {
    pub(crate) system: &'a str,
    pub(crate) user: &'a str,
    pub(crate) schema_name: &'a str,
    pub(crate) schema: serde_json::Value,
}

/// An AI call that failed, and whether trying the same request again could
/// plausibly succeed. Retrying a rejected key, an unknown model, or a schema
/// the endpoint will not accept just spends the user's time three times over.
#[derive(Debug)]
pub(crate) struct AiError {
    message: String,
    retryable: bool,
    /// The provider refused the native strict-schema `format` field, so the
    /// same request stands a chance if retried as a plain JSON-object reply.
    native_schema_rejected: bool,
}

impl AiError {
    fn rejects_native_schema(&self) -> bool {
        self.native_schema_rejected
    }
}

impl std::fmt::Display for AiError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for AiError {}

/// Parse and coverage problems carry no `AiError` and are always worth
/// another attempt — that retry loop is what makes synthesis reliable.
pub(crate) fn is_retryable(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<AiError>()
        .is_none_or(|failure| failure.retryable)
}

pub(crate) async fn complete<T>(
    request: &Request<'_>,
    parse: impl Fn(&str) -> Result<T>,
) -> Result<T> {
    ask_openai(request).await.and_then(|raw| parse(&raw))
}

pub(crate) fn flatten_error(error: &str) -> String {
    error
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join(" | ")
}

/// One client for the process; timeouts vary per request, connections pool.
fn http_client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .build()
            .expect("HTTP client should initialize")
    })
}

async fn ask_openai(request: &Request<'_>) -> Result<String> {
    let (base_url, model, api_key) = openai_env_config()?;
    let timeout = env_duration_secs("KITE_OPENAI_TIMEOUT_SECS", DEFAULT_OPENAI_TIMEOUT_SECS)?;
    let responses_url = format!("{}/responses", base_url.trim_end_matches('/'));

    // Prefer the native strict-schema format: OpenAI enforces it server-side.
    match send_responses(
        &responses_url,
        &api_key,
        timeout,
        strict_schema_body(&model, request),
    )
    .await
    {
        Err(failure) if failure.rejects_native_schema() => {
            // Some gateway providers (notably Bedrock-hosted Anthropic) reject
            // the `format` field outright. Fall back to a schema-less JSON reply
            // and describe the shape in the prompt instead; `extract_json_block`
            // tolerates the fenced or prose-wrapped output that results.
            send_responses(
                &responses_url,
                &api_key,
                timeout,
                json_object_body(&model, request),
            )
            .await
            .map_err(Into::into)
        }
        other => other.map_err(Into::into),
    }
}

/// The strict, server-enforced schema request — correct wherever it is honored.
fn strict_schema_body(model: &str, request: &Request<'_>) -> serde_json::Value {
    serde_json::json!({
        "model": model,
        "instructions": request.system,
        "input": request.user,
        "text": {
            "format": {
                "type": "json_schema",
                "name": request.schema_name,
                "strict": true,
                "schema": request.schema
            }
        }
    })
}

/// The portable request: ask for any JSON object and carry the schema in the
/// prompt, so providers that refuse a `format` field still return usable JSON.
fn json_object_body(model: &str, request: &Request<'_>) -> serde_json::Value {
    let instructions = format!(
        "{}\n\nRespond with a single JSON object and nothing else. It must satisfy this JSON Schema:\n{}",
        request.system, request.schema
    );
    serde_json::json!({
        "model": model,
        "instructions": instructions,
        "input": request.user,
        "text": { "format": { "type": "json_object" } }
    })
}

async fn send_responses(
    responses_url: &str,
    api_key: &str,
    timeout: Duration,
    body: serde_json::Value,
) -> std::result::Result<String, AiError> {
    let mut http_request = http_client()
        .post(responses_url)
        .timeout(timeout)
        .bearer_auth(api_key)
        .json(&body);
    if url_uses_portkey(responses_url)
        && let Some(portkey_api_key) = first_non_empty_env(&["PORTKEY_API_KEY"])
    {
        http_request = http_request.header("x-portkey-api-key", portkey_api_key);
    }

    let response = http_request.send().await.map_err(|error| AiError {
        message: format!("Could not reach {responses_url}: {error}"),
        // Timeouts, connection resets and DNS blips are all worth another go.
        retryable: true,
        native_schema_rejected: false,
    })?;

    let status = response.status();
    if !status.is_success() {
        // The body is where the endpoint says what was actually wrong —
        // an unknown model, a rejected schema, an expired key. Dropping it,
        // as `error_for_status` does, leaves the user with a bare status code
        // and an AI that appears to never work for no stated reason.
        let body = response.text().await.unwrap_or_default();
        return Err(AiError {
            message: describe_api_failure(status, &body),
            retryable: is_retryable_status(status),
            native_schema_rejected: status == reqwest::StatusCode::BAD_REQUEST
                && mentions_format_rejection(&body),
        });
    }

    let body = response.text().await.map_err(|error| AiError {
        message: format!("Could not read the reply from {responses_url}: {error}"),
        retryable: true,
        native_schema_rejected: false,
    })?;

    let json: serde_json::Value = serde_json::from_str(&body).map_err(|error| AiError {
        message: format!(
            "{responses_url} returned a non-JSON reply ({error}): {}",
            elide(&body, MAX_ERROR_BODY_CHARS)
        ),
        retryable: true,
        native_schema_rejected: false,
    })?;

    Ok(extract_openai_output_text(&json))
}

/// A gateway rejecting the `format` field looks like a 400 complaining that the
/// output format / extra inputs are not permitted.
fn mentions_format_rejection(body: &str) -> bool {
    let body = body.to_ascii_lowercase();
    body.contains("extra inputs are not permitted") || body.contains("output_config.format")
}

/// 4xx means the request itself is wrong and will stay wrong; the throttling
/// and server-side codes are the ones worth repeating.
fn is_retryable_status(status: reqwest::StatusCode) -> bool {
    status.is_server_error() || matches!(status.as_u16(), 408 | 409 | 425 | 429)
}

/// Pulls the human-readable reason out of an error payload, and adds the fix
/// for the mistakes that are otherwise a guessing game.
fn describe_api_failure(status: reqwest::StatusCode, body: &str) -> String {
    let detail = serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|json| {
            ["/error/message", "/message", "/detail"]
                .iter()
                .find_map(|pointer| {
                    json.pointer(pointer)
                        .and_then(|value| value.as_str())
                        .map(ToOwned::to_owned)
                })
        })
        .unwrap_or_else(|| elide(body.trim(), MAX_ERROR_BODY_CHARS));

    let hint = match status.as_u16() {
        401 | 403 => " — check the API key in KITE_OPENAI_API_KEY or OPENAI_API_KEY",
        404 => " — check the model in KITE_OPENAI_MODEL and the base URL",
        429 => " — rate limited or out of quota",
        _ => "",
    };

    if detail.is_empty() {
        format!("AI request failed ({status}){hint}")
    } else {
        format!("AI request failed ({status}): {detail}{hint}")
    }
}

fn elide(text: &str, max_chars: usize) -> String {
    let mut elided: String = text.chars().take(max_chars).collect();
    if text.chars().count() > max_chars {
        elided.push('…');
    }
    elided
}

fn url_uses_portkey(url: &str) -> bool {
    url.to_ascii_lowercase().contains("portkey")
}

fn openai_env_config() -> Result<(String, String, String)> {
    let base_url = first_non_empty_env(&[
        "KITE_OPENAI_URL",
        "KITE_OPENAI_BASE_URL",
        "OPENAI_URL",
        "OPENAI_BASE_URL",
    ])
    .unwrap_or_else(|| DEFAULT_OPENAI_URL.to_string());

    let model = first_non_empty_env(&["KITE_OPENAI_MODEL", "OPENAI_MODEL"])
        .unwrap_or_else(|| DEFAULT_OPENAI_MODEL.to_string());

    let api_key = first_non_empty_env(&[
        "KITE_OPENAI_API_KEY",
        "OPENAI_API_KEY",
        "KITE_API_KEY",
        "OPENAI_KEY",
        "AI_GATEWAY_API_KEY",
    ])
    .context(
        "No OpenAI API key found in KITE_OPENAI_API_KEY, OPENAI_API_KEY, KITE_API_KEY, or OPENAI_KEY",
    )?;

    Ok((normalize_openai_base_url(&base_url), model, api_key))
}

fn normalize_openai_base_url(base_url: &str) -> String {
    let base = base_url.trim_end_matches('/');
    if let Some(stripped) = base.strip_suffix("/responses") {
        stripped.to_string()
    } else if let Some(stripped) = base.strip_suffix("/chat/completions") {
        stripped.to_string()
    } else if base.ends_with("/v1") {
        base.to_string()
    } else {
        format!("{base}/v1")
    }
}

/// Pulls the reply text out of a Responses API payload, tolerating structured
/// output items, plain text items, and chat-completions-shaped proxies.
fn extract_openai_output_text(json: &serde_json::Value) -> String {
    if let Some(text) = json.get("output_text").and_then(|v| v.as_str()) {
        return text.to_string();
    }

    for item in json
        .get("output")
        .and_then(|v| v.as_array())
        .into_iter()
        .flatten()
    {
        for content in item
            .get("content")
            .and_then(|v| v.as_array())
            .into_iter()
            .flatten()
        {
            if let Some(structured) = content.get("json") {
                return structured.to_string();
            }
            if let Some(text) = content.get("text").and_then(|v| v.as_str()) {
                return text.to_string();
            }
        }
    }

    json.pointer("/choices/0/message/content")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string()
}

fn first_non_empty_env(keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|key| {
        env::var(key)
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
    })
}

fn env_duration_secs(key: &str, default_secs: u64) -> Result<Duration> {
    let Some(raw) = first_non_empty_env(&[key]) else {
        return Ok(Duration::from_secs(default_secs));
    };

    parse_timeout_secs(&raw)
        .map(Duration::from_secs)
        .with_context(|| format!("Invalid timeout in {key}"))
}

fn parse_timeout_secs(raw: &str) -> Result<u64> {
    let seconds: u64 = raw.trim().parse()?;
    if seconds == 0 {
        anyhow::bail!("timeout must be greater than zero seconds");
    }
    Ok(seconds)
}

/// Extracts the first balanced JSON value delimited by `open`/`close` from
/// free-form model output, skipping brackets inside string literals.
pub(crate) fn extract_json_block(raw: &str, open: char, close: char) -> Option<&str> {
    let mut start_idx: Option<usize> = None;
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;

    for (idx, ch) in raw.char_indices() {
        if in_string {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
            }
            continue;
        }

        match ch {
            '"' => in_string = true,
            c if c == open => {
                start_idx.get_or_insert(idx);
                depth += 1;
            }
            c if c == close && depth > 0 => {
                depth -= 1;
                if depth == 0
                    && let Some(start) = start_idx
                {
                    return Some(&raw[start..=idx]);
                }
            }
            _ => {}
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn extract_json_block_ignores_brackets_inside_strings() {
        let raw = r#"noise "[ignore]" before [{"message":"feat: add parser","files":["src/main.rs"]}] after"#;
        let extracted = extract_json_block(raw, '[', ']').expect("array should be extracted");

        assert_eq!(
            extracted,
            r#"[{"message":"feat: add parser","files":["src/main.rs"]}]"#
        );
    }

    #[test]
    fn extract_json_block_finds_objects_in_fenced_output() {
        let raw = "```json\n{\"title\":\"feat: add pr\",\"body\":\"Adds [kt pr].\"}\n```";
        let extracted = extract_json_block(raw, '{', '}').expect("object should be extracted");

        assert_eq!(
            extracted,
            r#"{"title":"feat: add pr","body":"Adds [kt pr]."}"#
        );
    }

    #[test]
    fn extract_openai_output_text_prefers_structured_json() {
        let payload = json!({
            "output": [
                { "content": [ { "json": { "groups": [] } } ] }
            ]
        });

        assert_eq!(extract_openai_output_text(&payload), r#"{"groups":[]}"#);
    }

    #[test]
    fn extract_openai_output_text_falls_back_to_output_text_and_chat_shapes() {
        let output_text = json!({ "output_text": "hello" });
        assert_eq!(extract_openai_output_text(&output_text), "hello");

        let chat = json!({ "choices": [ { "message": { "content": "from proxy" } } ] });
        assert_eq!(extract_openai_output_text(&chat), "from proxy");
    }

    #[test]
    fn normalize_openai_base_url_handles_common_shapes() {
        assert_eq!(
            normalize_openai_base_url("https://api.openai.com/v1/responses"),
            "https://api.openai.com/v1"
        );
        assert_eq!(
            normalize_openai_base_url("https://proxy.dev/v1/chat/completions"),
            "https://proxy.dev/v1"
        );
        assert_eq!(
            normalize_openai_base_url("https://api.openai.com/v1/"),
            "https://api.openai.com/v1"
        );
        assert_eq!(
            normalize_openai_base_url("https://proxy.dev"),
            "https://proxy.dev/v1"
        );
    }

    #[test]
    fn parse_timeout_secs_accepts_positive_integer_seconds() {
        assert_eq!(
            parse_timeout_secs("120").expect("timeout should parse"),
            120
        );
        assert_eq!(
            parse_timeout_secs(" 45 ").expect("trimmed timeout should parse"),
            45
        );
    }

    #[test]
    fn parse_timeout_secs_rejects_zero_and_invalid_values() {
        let zero = parse_timeout_secs("0").expect_err("zero should fail");
        assert!(format!("{zero:#}").contains("greater than zero"));

        let invalid = parse_timeout_secs("slow").expect_err("non-number should fail");
        assert!(format!("{invalid:#}").contains("invalid digit"));
    }

    #[test]
    fn describe_api_failure_surfaces_the_endpoints_own_message() {
        // `error_for_status` threw this away, leaving users with a bare status
        // code and no way to tell why the AI never worked.
        let body = r#"{"error":{"message":"Invalid schema for response_format 'commit_groups'.","type":"invalid_request_error"}}"#;
        let described = describe_api_failure(reqwest::StatusCode::BAD_REQUEST, body);

        assert!(described.contains("400 Bad Request"));
        assert!(described.contains("Invalid schema for response_format 'commit_groups'."));
    }

    #[test]
    fn describe_api_failure_adds_hints_and_survives_non_json_bodies() {
        let unauthorized = describe_api_failure(
            reqwest::StatusCode::UNAUTHORIZED,
            r#"{"error":{"message":"Incorrect API key."}}"#,
        );
        assert!(unauthorized.contains("check the API key"));

        let not_found = describe_api_failure(reqwest::StatusCode::NOT_FOUND, "<html>nope</html>");
        assert!(not_found.contains("<html>nope</html>"));
        assert!(not_found.contains("check the model"));

        let empty = describe_api_failure(reqwest::StatusCode::BAD_GATEWAY, "");
        assert_eq!(empty, "AI request failed (502 Bad Gateway)");
    }

    #[test]
    fn only_transient_statuses_are_worth_retrying() {
        // Repeating a rejected key or an unsupported schema just makes the
        // user wait three times as long for the same failure.
        for code in [400, 401, 403, 404, 422] {
            let status = reqwest::StatusCode::from_u16(code).expect("valid status");
            assert!(!is_retryable_status(status), "{code} should not retry");
        }
        for code in [408, 409, 425, 429, 500, 502, 503] {
            let status = reqwest::StatusCode::from_u16(code).expect("valid status");
            assert!(is_retryable_status(status), "{code} should retry");
        }
    }

    #[test]
    fn parse_failures_stay_retryable() {
        let parse_failure = anyhow::anyhow!("Model reply contained no commit groups");
        assert!(is_retryable(&parse_failure));

        let hard_failure: anyhow::Error = AiError {
            message: "AI request failed (401 Unauthorized)".to_string(),
            retryable: false,
            native_schema_rejected: false,
        }
        .into();
        assert!(!is_retryable(&hard_failure));
    }

    #[test]
    fn elide_truncates_on_character_boundaries() {
        assert_eq!(elide("héllo", 10), "héllo");
        assert_eq!(elide("héllo", 2), "hé…");
    }

    #[test]
    fn url_uses_portkey_detects_portkey_hosts_case_insensitively() {
        assert!(url_uses_portkey("https://example.PortKey.ai/v1/responses"));
        assert!(!url_uses_portkey("https://api.openai.com/v1/responses"));
    }

    #[test]
    fn mentions_format_rejection_spots_the_bedrock_anthropic_refusal() {
        // The exact 400 a Bedrock-hosted Anthropic model returns for `format`.
        let body = r#"{"error":{"message":"bedrock error: The model returned the following errors: output_config.format: Extra inputs are not permitted"}}"#;
        assert!(mentions_format_rejection(body));

        // An unrelated 400 must not trigger the JSON-object fallback.
        let unrelated =
            r#"{"error":{"message":"Invalid schema for response_format 'commit_groups'."}}"#;
        assert!(!mentions_format_rejection(unrelated));
    }

    #[test]
    fn json_object_body_embeds_the_schema_and_drops_strict_format() {
        let request = Request {
            system: "You are terse.",
            user: "Group the files.",
            schema_name: "commit_groups",
            schema: json!({ "type": "object" }),
        };
        let body = json_object_body("@bedrock-au/au.anthropic.claude-opus-4-8", &request);

        assert_eq!(body["text"]["format"]["type"], "json_object");
        let instructions = body["instructions"].as_str().expect("instructions string");
        assert!(instructions.contains("You are terse."));
        assert!(instructions.contains("JSON Schema"));
        assert!(instructions.contains("\"type\":\"object\""));
    }
}
