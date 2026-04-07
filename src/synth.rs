use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::env;
use std::time::Duration;

use crate::git::{recent_commit_style_examples, sorted_files};

const MAX_COMMIT_STYLE_EXAMPLES: usize = 6;

const SYSTEM_PROMPT: &str = "\
You are an expert version control synthesis engine. Analyze the git diff.
Group the changed files into distinct, atomic commits based on logical purpose.
Write commit messages that match the repository's recent style examples when they show a clear pattern. If the examples are mixed or absent, fall back to a Conventional Commit style.

Rules for commit messages:
1. Match the repository's recent wording, prefixes, and scope style when those examples are consistent.
2. If no clear style emerges from the examples, use: <type>(<optional scope>): <description>
3. Use the imperative, present tense: 'add' not 'added' or 'adds'.
4. Keep the message concise and specific about technical intent.
5. No trailing periods.
6. Never emit `[kite] save` as a landed commit message.

Rules for file assignment:
1. Use only file paths that appear in the provided changed-file list.
2. Copy each file path exactly as provided.
3. Assign every changed file exactly once.
4. Do not omit files.
5. Do not duplicate files across groups.

Return ONLY a valid JSON array of objects. Absolutely no markdown or conversational text.
Schema: [ { \"message\": \"feat(auth): implement JWT validation\", \"files\": [\"src/auth.rs\"] } ]";

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub(crate) struct CommitGroup {
    pub(crate) message: String,
    pub(crate) files: Vec<String>,
}

#[derive(Deserialize)]
struct CommitGroupsEnvelope {
    groups: Vec<CommitGroup>,
}

#[derive(Clone, Debug)]
pub(crate) struct ProviderFailure {
    pub(crate) provider: &'static str,
    pub(crate) error: String,
}

pub(crate) async fn synthesize_groups(
    diff: &str,
    actual_files: &HashSet<String>,
) -> std::result::Result<(Vec<CommitGroup>, &'static str), Vec<ProviderFailure>> {
    let mut failures = Vec::new();

    match try_local_ollama(diff, actual_files).await {
        Ok(groups) => return Ok((groups, "local")),
        Err(local_error) => failures.push(ProviderFailure {
            provider: "local",
            error: format!("{local_error:#}"),
        }),
    }

    match try_openai(diff, actual_files).await {
        Ok(groups) => Ok((groups, "cloud")),
        Err(openai_error) => {
            failures.push(ProviderFailure {
                provider: "cloud",
                error: format!("{openai_error:#}"),
            });
            Err(failures)
        }
    }
}

pub(crate) fn normalize_groups(
    groups: Vec<CommitGroup>,
    actual_files: &HashSet<String>,
) -> Vec<CommitGroup> {
    let mut remaining = actual_files.clone();
    let mut normalized = Vec::new();

    for group in groups {
        let mut files = Vec::new();
        for file in group.files {
            if remaining.remove(&file) {
                files.push(file);
            }
        }

        if !files.is_empty() {
            normalized.push(CommitGroup {
                message: group.message.trim().to_string(),
                files,
            });
        }
    }

    if !remaining.is_empty() {
        normalized.push(CommitGroup {
            message: "chore: unclassified updates".to_string(),
            files: sorted_files(&remaining),
        });
    }

    normalized
}

pub(crate) fn flatten_error(error: &str) -> String {
    error
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join(" | ")
}

async fn try_local_ollama(diff: &str, actual_files: &HashSet<String>) -> Result<Vec<CommitGroup>> {
    let prompt = build_synthesis_input(diff, actual_files, 15_000)?;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(3))
        .build()?;

    let body = serde_json::json!({
        "model": env::var("KITE_LOCAL_MODEL").unwrap_or_else(|_| "llama3".to_string()),
        "messages": [
            { "role": "system", "content": SYSTEM_PROMPT },
            { "role": "user", "content": prompt }
        ],
        "stream": false,
        "format": "json"
    });

    let res = client
        .post("http://localhost:11434/api/chat")
        .json(&body)
        .send()
        .await?
        .error_for_status()?;
    let json: serde_json::Value = res.json().await?;
    let content = json["message"]["content"].as_str().unwrap_or("[]");

    parse_json(content).and_then(|groups| validate_group_coverage(groups, actual_files))
}

async fn try_openai(diff: &str, actual_files: &HashSet<String>) -> Result<Vec<CommitGroup>> {
    let (base_url, model, api_key) = get_openai_env_config()?;
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(30))
        .build()?;

    let prompt = format!(
        "{}\nFor this provider, return an object exactly like: {{ \"groups\": [{{\"message\":\"...\",\"files\":[\"...\"]}}] }}\n\n{}",
        SYSTEM_PROMPT,
        build_synthesis_input(diff, actual_files, 20_000)?
    );

    let body = serde_json::json!({
        "model": model,
        "input": prompt,
        "text": {
            "format": {
                "type": "json_schema",
                "name": "commit_groups",
                "strict": true,
                "schema": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["groups"],
                    "properties": {
                        "groups": {
                            "type": "array",
                            "minItems": 1,
                            "items": {
                                "type": "object",
                                "additionalProperties": false,
                                "required": ["message", "files"],
                                "properties": {
                                    "message": { "type": "string", "minLength": 1 },
                                    "files": {
                                        "type": "array",
                                        "minItems": 1,
                                        "items": { "type": "string", "minLength": 1 }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    });

    let responses_url = format!("{}/responses", base_url.trim_end_matches('/'));
    let res = client
        .post(responses_url)
        .bearer_auth(api_key)
        .json(&body)
        .send()
        .await
        .with_context(|| "Failed to send request to OpenAI Responses API")?
        .error_for_status()
        .with_context(|| "OpenAI Responses API returned non-success status")?;

    let json: serde_json::Value = res.json().await?;
    parse_openai_groups(&json).and_then(|groups| validate_group_coverage(groups, actual_files))
}

fn build_synthesis_input(
    diff: &str,
    actual_files: &HashSet<String>,
    max_diff_bytes: usize,
) -> Result<String> {
    let mut prompt = String::new();
    let examples = recent_commit_style_examples(MAX_COMMIT_STYLE_EXAMPLES)?;

    if !examples.is_empty() {
        prompt.push_str("Recent non-Kite commit message examples from this repository:\n");
        for example in examples {
            prompt.push_str("- ");
            prompt.push_str(&example);
            prompt.push('\n');
        }
        prompt.push('\n');
    }

    prompt.push_str("Changed files (assign each file exactly once and copy paths verbatim):\n");
    for file in sorted_files(actual_files) {
        prompt.push_str("- ");
        prompt.push_str(&file);
        prompt.push('\n');
    }
    prompt.push('\n');

    prompt.push_str("Diff (may be truncated; rely on the changed-file list for full coverage):\n");
    prompt.push_str(truncate_for_prompt(diff, max_diff_bytes));
    Ok(prompt)
}

fn validate_group_coverage(
    groups: Vec<CommitGroup>,
    actual_files: &HashSet<String>,
) -> Result<Vec<CommitGroup>> {
    let mut seen = HashSet::new();
    let mut missing = actual_files.clone();
    let mut duplicates = Vec::new();
    let mut unknown = Vec::new();

    for group in &groups {
        for file in &group.files {
            if !actual_files.contains(file) {
                unknown.push(file.clone());
                continue;
            }

            if !seen.insert(file.clone()) {
                duplicates.push(file.clone());
                continue;
            }

            missing.remove(file);
        }
    }

    if duplicates.is_empty() && unknown.is_empty() && missing.is_empty() {
        return Ok(groups);
    }

    let mut problems = Vec::new();

    if !missing.is_empty() {
        problems.push(format!(
            "missing files: {}",
            sorted_files(&missing).join(", ")
        ));
    }

    if !duplicates.is_empty() {
        duplicates.sort();
        duplicates.dedup();
        problems.push(format!("duplicate files: {}", duplicates.join(", ")));
    }

    if !unknown.is_empty() {
        unknown.sort();
        unknown.dedup();
        problems.push(format!("unknown files: {}", unknown.join(", ")));
    }

    anyhow::bail!(
        "Synthesis output did not cover the changed files correctly ({})",
        problems.join("; ")
    );
}

fn get_openai_env_config() -> Result<(String, String, String)> {
    let base_url = first_non_empty_env(&[
        "KITE_OPENAI_URL",
        "KITE_OPENAI_BASE_URL",
        "OPENAI_URL",
        "OPENAI_BASE_URL",
    ])
    .unwrap_or_else(|| "https://api.openai.com/v1".to_string());

    let model = first_non_empty_env(&["KITE_OPENAI_MODEL", "OPENAI_MODEL"])
        .unwrap_or_else(|| "gpt-5.4-mini".to_string());

    let api_key = first_non_empty_env(&[
        "KITE_OPENAI_API_KEY",
        "OPENAI_API_KEY",
        "KITE_API_KEY",
        "OPENAI_KEY",
    ])
    .context(
        "No OpenAI API key found in KITE_OPENAI_API_KEY, OPENAI_API_KEY, KITE_API_KEY, or OPENAI_KEY",
    )?;

    let completions_base = base_url.trim_end_matches('/');
    let normalized_base = if completions_base.ends_with("/responses") {
        completions_base.trim_end_matches("/responses").to_string()
    } else if completions_base.ends_with("/chat/completions") {
        completions_base
            .trim_end_matches("/chat/completions")
            .to_string()
    } else if completions_base.ends_with("/v1") {
        completions_base.to_string()
    } else {
        format!("{}/v1", completions_base)
    };

    Ok((normalized_base, model, api_key))
}

fn extract_openai_output_text(json: &serde_json::Value) -> String {
    if let Some(s) = json.get("output_text").and_then(|v| v.as_str()) {
        return s.to_string();
    }

    if let Some(output) = json.get("output").and_then(|v| v.as_array()) {
        for item in output {
            if let Some(content_items) = item.get("content").and_then(|v| v.as_array()) {
                for content_item in content_items {
                    if let Some(text) = content_item.get("text").and_then(|v| v.as_str()) {
                        return text.to_string();
                    }
                }
            }
        }
    }

    json.pointer("/choices/0/message/content")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string()
}

fn parse_openai_groups(json: &serde_json::Value) -> Result<Vec<CommitGroup>> {
    if let Some(output) = json.get("output").and_then(|v| v.as_array()) {
        for item in output {
            if let Some(content_items) = item.get("content").and_then(|v| v.as_array()) {
                for content_item in content_items {
                    if let Some(structured) = content_item.get("json") {
                        let parsed: CommitGroupsEnvelope =
                            serde_json::from_value(structured.clone())?;
                        if !parsed.groups.is_empty() {
                            return Ok(parsed.groups);
                        }
                    }
                }
            }
        }
    }

    let content = extract_openai_output_text(json);
    if let Ok(parsed) = serde_json::from_str::<CommitGroupsEnvelope>(content.trim()) {
        if !parsed.groups.is_empty() {
            return Ok(parsed.groups);
        }
    }

    parse_json(&content)
}

fn first_non_empty_env(keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|key| {
        env::var(key)
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
    })
}

fn parse_json(raw: &str) -> Result<Vec<CommitGroup>> {
    if let Ok(groups) = serde_json::from_str::<Vec<CommitGroup>>(raw.trim()) {
        if !groups.is_empty() {
            return Ok(groups);
        }
    }

    if let Ok(value) = serde_json::from_str::<serde_json::Value>(raw.trim()) {
        if let Some(groups_value) = value.get("groups") {
            let groups: Vec<CommitGroup> = serde_json::from_value(groups_value.clone())?;
            if !groups.is_empty() {
                return Ok(groups);
            }
        }
    }

    let json_str = extract_first_json_array(raw).unwrap_or_else(|| "[]".to_string());
    let groups: Vec<CommitGroup> = serde_json::from_str(&json_str)?;

    if groups.is_empty() {
        anyhow::bail!("Empty JSON array parsed");
    }
    Ok(groups)
}

fn extract_first_json_array(raw: &str) -> Option<String> {
    let mut start_idx: Option<usize> = None;
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;

    for (idx, ch) in raw.char_indices() {
        if in_string {
            if escaped {
                escaped = false;
                continue;
            }
            if ch == '\\' {
                escaped = true;
                continue;
            }
            if ch == '"' {
                in_string = false;
            }
            continue;
        }

        match ch {
            '"' => in_string = true,
            '[' => {
                if start_idx.is_none() {
                    start_idx = Some(idx);
                }
                depth += 1;
            }
            ']' => {
                if depth == 0 {
                    continue;
                }
                depth -= 1;
                if depth == 0 {
                    if let Some(start) = start_idx {
                        return Some(raw[start..=idx].to_string());
                    }
                }
            }
            _ => {}
        }
    }

    None
}

fn truncate_for_prompt(text: &str, max_bytes: usize) -> &str {
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
    use crate::test_support::{acquire_cwd_lock, git, init_repo, with_repo_cwd, write_file};
    use serde_json::json;

    fn assert_single_group(groups: Vec<CommitGroup>, message: &str, file: &str) {
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].message, message);
        assert_eq!(groups[0].files, vec![file.to_string()]);
    }

    #[test]
    fn extract_first_json_array_ignores_brackets_inside_strings() {
        let raw = r#"noise "[ignore]" before [{"message":"feat: add parser","files":["src/main.rs"]}] after"#;
        let extracted = extract_first_json_array(raw).expect("array should be extracted");

        assert_eq!(
            extracted,
            r#"[{"message":"feat: add parser","files":["src/main.rs"]}]"#
        );
    }

    #[test]
    fn parse_json_accepts_groups_envelope_shape() {
        let raw = r#"{"groups":[{"message":"fix: tighten parsing","files":["src/main.rs"]}]}"#;
        let parsed = parse_json(raw).expect("groups envelope should parse");

        assert_single_group(parsed, "fix: tighten parsing", "src/main.rs");
    }

    #[test]
    fn parse_json_extracts_array_from_mixed_text() {
        let raw = "Result:\n```json\n[{\"message\":\"chore: update deps\",\"files\":[\"Cargo.toml\"]}]\n```";
        let parsed = parse_json(raw).expect("embedded json array should parse");

        assert_single_group(parsed, "chore: update deps", "Cargo.toml");
    }

    #[test]
    fn parse_openai_groups_uses_structured_json_when_present() {
        let payload = json!({
            "output": [
                {
                    "content": [
                        {
                            "json": {
                                "groups": [
                                    {
                                        "message": "feat(cli): add flow command",
                                        "files": ["src/main.rs"]
                                    }
                                ]
                            }
                        }
                    ]
                }
            ]
        });

        let parsed = parse_openai_groups(&payload).expect("structured output should parse");

        assert_single_group(parsed, "feat(cli): add flow command", "src/main.rs");
    }

    #[test]
    fn parse_openai_groups_falls_back_to_output_text() {
        let payload = json!({
            "output_text": "[{\"message\":\"docs: clarify readme\",\"files\":[\"README.md\"]}]"
        });

        let parsed = parse_openai_groups(&payload).expect("output_text should parse");

        assert_single_group(parsed, "docs: clarify readme", "README.md");
    }

    #[test]
    fn validate_group_coverage_requires_full_exact_assignment() {
        let actual_files: HashSet<String> = ["src/main.rs", "README.md"]
            .into_iter()
            .map(str::to_string)
            .collect();

        let valid = validate_group_coverage(
            vec![
                CommitGroup {
                    message: "feat(cli): improve landing".to_string(),
                    files: vec!["src/main.rs".to_string()],
                },
                CommitGroup {
                    message: "docs: refresh readme".to_string(),
                    files: vec!["README.md".to_string()],
                },
            ],
            &actual_files,
        )
        .expect("full coverage should validate");
        assert_eq!(valid.len(), 2);

        let err = validate_group_coverage(
            vec![CommitGroup {
                message: "feat(cli): improve landing".to_string(),
                files: vec!["src/main.rs".to_string()],
            }],
            &actual_files,
        )
        .expect_err("missing files should fail validation");
        assert!(format!("{err:#}").contains("missing files: README.md"));
    }

    #[test]
    fn validate_group_coverage_rejects_duplicate_and_unknown_files() {
        let actual_files: HashSet<String> = ["src/main.rs", "README.md"]
            .into_iter()
            .map(str::to_string)
            .collect();

        let err = validate_group_coverage(
            vec![
                CommitGroup {
                    message: "feat(cli): improve landing".to_string(),
                    files: vec!["src/main.rs".to_string(), "src/main.rs".to_string()],
                },
                CommitGroup {
                    message: "docs: refresh readme".to_string(),
                    files: vec!["bogus.txt".to_string()],
                },
            ],
            &actual_files,
        )
        .expect_err("duplicate and unknown files should fail validation");

        let rendered = format!("{err:#}");
        assert!(rendered.contains("missing files: README.md"));
        assert!(rendered.contains("duplicate files: src/main.rs"));
        assert!(rendered.contains("unknown files: bogus.txt"));
    }

    #[test]
    fn build_synthesis_input_lists_all_changed_files() {
        let _lock = acquire_cwd_lock();
        let repo = init_repo();
        let actual_files: HashSet<String> = ["src/main.rs", "README.md"]
            .into_iter()
            .map(str::to_string)
            .collect();

        let prompt = with_repo_cwd(&repo.path, || {
            build_synthesis_input("diff --git a/src/main.rs b/src/main.rs", &actual_files, 500)
        })
        .expect("prompt should build");

        assert!(
            prompt
                .contains("Changed files (assign each file exactly once and copy paths verbatim):")
        );
        assert!(prompt.contains("- README.md"));
        assert!(prompt.contains("- src/main.rs"));
        assert!(
            prompt.contains(
                "Diff (may be truncated; rely on the changed-file list for full coverage):"
            )
        );

        let readme_index = prompt
            .find("- README.md")
            .expect("README entry should exist");
        let src_index = prompt
            .find("- src/main.rs")
            .expect("src/main.rs entry should exist");
        assert!(readme_index < src_index, "file list should be sorted");
    }

    #[test]
    fn build_synthesis_input_includes_examples_and_truncates_diff() {
        let _lock = acquire_cwd_lock();
        let repo = init_repo();
        let actual_files: HashSet<String> =
            ["src/main.rs"].into_iter().map(str::to_string).collect();

        write_file(&repo.path, "tracked.txt", "first\n");
        git(&repo.path, &["add", "tracked.txt"]);
        git(&repo.path, &["commit", "-m", "fix(cli): tighten landing"]);

        write_file(&repo.path, "tracked.txt", "second\n");
        git(&repo.path, &["add", "tracked.txt"]);
        git(&repo.path, &["commit", "-m", "docs: refresh usage"]);

        let prompt = with_repo_cwd(&repo.path, || {
            build_synthesis_input("abcdefghijklmnopqrstuvwxyz", &actual_files, 8)
        })
        .expect("prompt should build");

        assert!(prompt.contains("Recent non-Kite commit message examples from this repository:"));
        assert!(prompt.contains("- docs: refresh usage"));
        assert!(prompt.contains("- fix(cli): tighten landing"));
        assert!(prompt.contains(
            "Diff (may be truncated; rely on the changed-file list for full coverage):\nabcdefgh"
        ));
        assert!(!prompt.contains("abcdefghi"));
    }

    #[test]
    fn normalize_groups_deduplicates_files_and_catches_leftovers() {
        let actual_files: HashSet<String> = ["src/main.rs", "README.md"]
            .into_iter()
            .map(str::to_string)
            .collect();

        let normalized = normalize_groups(
            vec![CommitGroup {
                message: "feat(cli): tighten landing".to_string(),
                files: vec![
                    "src/main.rs".to_string(),
                    "src/main.rs".to_string(),
                    "missing.txt".to_string(),
                ],
            }],
            &actual_files,
        );

        assert_eq!(normalized.len(), 2);
        assert_eq!(normalized[0].files, vec!["src/main.rs".to_string()]);
        assert_eq!(normalized[1].message, "chore: unclassified updates");
        assert_eq!(normalized[1].files, vec!["README.md".to_string()]);
    }
}
