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

/// One structured request: a system prompt, a user prompt, and the JSON schema
/// the reply must satisfy (enforced natively by the Responses API).
pub(crate) struct Request<'a> {
    pub(crate) system: &'a str,
    pub(crate) user: &'a str,
    pub(crate) schema_name: &'a str,
    pub(crate) schema: serde_json::Value,
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

    let body = serde_json::json!({
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
    });

    let responses_url = format!("{}/responses", base_url.trim_end_matches('/'));
    let mut http_request = http_client()
        .post(&responses_url)
        .timeout(timeout)
        .bearer_auth(api_key)
        .json(&body);
    if url_uses_portkey(&responses_url)
        && let Some(portkey_api_key) = first_non_empty_env(&["PORTKEY_API_KEY"])
    {
        http_request = http_request.header("x-portkey-api-key", portkey_api_key);
    }

    let response = http_request
        .send()
        .await
        .context("Failed to send request to OpenAI Responses API")?
        .error_for_status()
        .context("OpenAI Responses API returned non-success status")?;

    let json: serde_json::Value = response.json().await?;
    Ok(extract_openai_output_text(&json))
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
    fn url_uses_portkey_detects_portkey_hosts_case_insensitively() {
        assert!(url_uses_portkey("https://example.PortKey.ai/v1/responses"));
        assert!(!url_uses_portkey("https://api.openai.com/v1/responses"));
    }
}
