//! Sends AI requests and returns reply text. Commands parse and validate it.

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
pub(crate) struct Request {
    pub(crate) system: String,
    pub(crate) user: String,
    pub(crate) schema_name: String,
    pub(crate) schema: serde_json::Value,
}

/// Provider failure with enough detail to choose whether to retry.
#[derive(Debug)]
struct AiError {
    message: String,
    retryable: bool,
    /// Retry without native schema enforcement when a gateway rejects it.
    native_schema_rejected: bool,
}

impl std::fmt::Display for AiError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for AiError {}

/// Transport failures follow provider retry rules; invalid model output can retry.
pub(crate) fn is_retryable(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<AiError>()
        .is_none_or(|failure| failure.retryable)
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

pub(crate) async fn complete(request: &Request) -> Result<String> {
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
        Err(failure) if failure.native_schema_rejected => {
            // Some gateways reject `format`; describe the schema in the prompt instead.
            send_responses(
                &responses_url,
                &api_key,
                timeout,
                prompt_schema_body(&model, request),
            )
            .await
            .map_err(Into::into)
        }
        other => other.map_err(Into::into),
    }
}

/// The strict, server-enforced schema request — correct wherever it is honored.
fn strict_schema_body(model: &str, request: &Request) -> serde_json::Value {
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

/// Providers that reject `format` need it omitted entirely, including JSON mode.
fn prompt_schema_body(model: &str, request: &Request) -> serde_json::Value {
    let instructions = format!(
        "{}\n\nRespond with a single JSON object and nothing else. It must satisfy this JSON Schema:\n{}",
        request.system, request.schema
    );
    serde_json::json!({
        "model": model,
        "instructions": instructions,
        "input": request.user
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
        // Preserve the provider's explanation, not just its HTTP status.
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

    extract_openai_output_text(&json)
}

/// A gateway rejecting the `format` field looks like a 400 complaining that the
/// output format / extra inputs are not permitted.
fn mentions_format_rejection(body: &str) -> bool {
    let body = body.to_ascii_lowercase();
    body.contains("output_config.format")
        || (body.contains("format") && body.contains("extra inputs are not permitted"))
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
        .unwrap_or_else(|| body.trim().to_string());
    let detail = elide(&detail, MAX_ERROR_BODY_CHARS);

    let hint = match status.as_u16() {
        401 | 403 => " - check the API key in KITE_OPENAI_API_KEY or OPENAI_API_KEY",
        404 => " - check the model in KITE_OPENAI_MODEL and the base URL",
        429 => " - rate limited or out of quota",
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
    reqwest::Url::parse(url).is_ok_and(|url| {
        url.host_str()
            .is_some_and(|host| host == "portkey.ai" || host.ends_with(".portkey.ai"))
    })
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
        "No OpenAI API key found in KITE_OPENAI_API_KEY, OPENAI_API_KEY, KITE_API_KEY, OPENAI_KEY, or AI_GATEWAY_API_KEY",
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
fn extract_openai_output_text(json: &serde_json::Value) -> std::result::Result<String, AiError> {
    let failure = |message: String, retryable| AiError {
        message,
        retryable,
        native_schema_rejected: false,
    };
    if let Some(error) = json.get("error").filter(|error| !error.is_null()) {
        let detail = error.get("message").and_then(|value| value.as_str());
        return Err(failure(
            format!(
                "AI response failed: {}",
                elide(
                    detail.unwrap_or("unknown provider error"),
                    MAX_ERROR_BODY_CHARS
                )
            ),
            matches!(
                error.get("code").and_then(|value| value.as_str()),
                Some("server_error" | "rate_limit_exceeded")
            ),
        ));
    }
    if let Some(status) = json.get("status").and_then(|value| value.as_str())
        && status != "completed"
    {
        let reason = json
            .pointer("/incomplete_details/reason")
            .and_then(|value| value.as_str())
            .unwrap_or(status);
        return Err(failure(
            format!(
                "AI response did not complete: {}",
                elide(reason, MAX_ERROR_BODY_CHARS)
            ),
            false,
        ));
    }
    if let Some(text) = json.get("output_text").and_then(|v| v.as_str()) {
        return Ok(text.to_string());
    }

    let mut text = String::new();
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
                return Ok(structured.to_string());
            }
            if let Some(refusal) = content.get("refusal").and_then(|v| v.as_str()) {
                return Err(failure(
                    format!(
                        "AI refused the request: {}",
                        elide(refusal, MAX_ERROR_BODY_CHARS)
                    ),
                    false,
                ));
            }
            if let Some(part) = content.get("text").and_then(|v| v.as_str()) {
                text.push_str(part);
            }
        }
    }

    if text.is_empty()
        && let Some(content) = json
            .pointer("/choices/0/message/content")
            .and_then(|v| v.as_str())
    {
        text.push_str(content);
    }
    if text.is_empty() {
        return Err(failure(
            "AI response contained no output text".to_string(),
            true,
        ));
    }
    Ok(text)
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

pub(crate) fn truncate_for_prompt(text: &str, max_bytes: usize) -> &str {
    if text.len() <= max_bytes {
        return text;
    }

    let mut cutoff = max_bytes;
    while cutoff > 0 && !text.is_char_boundary(cutoff) {
        cutoff -= 1;
    }

    &text[..cutoff]
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::{Read, Write};

    #[test]
    fn truncate_for_prompt_respects_char_boundaries() {
        assert_eq!(truncate_for_prompt("abcdefgh", 8), "abcdefgh");
        assert_eq!(truncate_for_prompt("abcdefghij", 8), "abcdefgh");
        assert_eq!(truncate_for_prompt("héllo", 2), "h"); // no mid-codepoint cuts
    }

    fn local_response(payload: serde_json::Value) -> std::result::Result<String, AiError> {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/v1/responses", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut request = Vec::new();
            let mut byte = [0];
            while !request.ends_with(b"\r\n\r\n") {
                socket.read_exact(&mut byte).unwrap();
                request.push(byte[0]);
            }
            let length = String::from_utf8(request)
                .unwrap()
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().unwrap())
                })
                .unwrap();
            socket.read_exact(&mut vec![0; length]).unwrap();
            let body = payload.to_string();
            write!(
                socket,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .unwrap();
        });
        let result = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(send_responses(
                &url,
                "test-key",
                Duration::from_secs(5),
                json!({}),
            ));
        server.join().unwrap();
        result
    }

    #[test]
    fn response_text_in_multiple_parts_is_not_dropped() {
        let reply = local_response(json!({
            "status": "completed",
            "output": [{ "content": [
                { "type": "output_text", "text": "{\"groups\":" },
                { "type": "output_text", "text": "[]}" }
            ] }]
        }))
        .unwrap();

        assert_eq!(reply, r#"{"groups":[]}"#);
    }

    #[test]
    fn failed_response_reports_the_provider_error() {
        let error = local_response(json!({
            "status": "failed",
            "error": { "code": "server_error", "message": "The provider could not complete this request" }
        }))
        .expect_err("a failed response is not model output");

        assert!(
            error
                .to_string()
                .contains("The provider could not complete this request")
        );
        assert!(error.retryable);
    }

    #[test]
    fn incomplete_and_refused_responses_are_not_usable_output() {
        let incomplete = local_response(json!({
            "status": "incomplete",
            "incomplete_details": { "reason": "max_output_tokens" },
            "output_text": "{\"groups\":[]}"
        }))
        .expect_err("even valid JSON must not conceal an incomplete response");
        assert!(incomplete.to_string().contains("max_output_tokens"));
        assert!(!incomplete.retryable);

        let refused = local_response(json!({
            "status": "completed",
            "output": [{ "content": [{ "type": "refusal", "refusal": "Cannot process this request" }] }]
        }))
        .expect_err("a refusal is not an empty model reply");
        assert!(refused.to_string().contains("Cannot process this request"));
        assert!(!refused.retryable);
    }

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

        assert_eq!(
            extract_openai_output_text(&payload).unwrap(),
            r#"{"groups":[]}"#
        );
    }

    #[test]
    fn extract_openai_output_text_falls_back_to_output_text_and_chat_shapes() {
        let output_text = json!({ "output_text": "hello" });
        assert_eq!(extract_openai_output_text(&output_text).unwrap(), "hello");

        let chat = json!({ "choices": [ { "message": { "content": "from proxy" } } ] });
        assert_eq!(extract_openai_output_text(&chat).unwrap(), "from proxy");
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
        assert!(!url_uses_portkey("https://example.com/portkey/responses"));
        assert!(!url_uses_portkey(
            "https://portkey.ai.example.com/v1/responses"
        ));
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
        assert!(!mentions_format_rejection(
            "reasoning: Extra inputs are not permitted"
        ));
    }

    #[test]
    fn prompt_schema_body_omits_the_rejected_format_field_entirely() {
        let request = Request {
            system: "You are terse.".to_string(),
            user: "Group the files.".to_string(),
            schema_name: "commit_groups".to_string(),
            schema: json!({ "type": "object" }),
        };
        let body = prompt_schema_body("@bedrock-au/au.anthropic.claude-opus-4-8", &request);

        assert!(body.get("text").is_none());
        let instructions = body["instructions"].as_str().expect("instructions string");
        assert!(instructions.contains("You are terse."));
        assert!(instructions.contains("JSON Schema"));
        assert!(instructions.contains("\"type\":\"object\""));
    }
}
