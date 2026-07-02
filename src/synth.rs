//! Turns the diff introduced by Kite saves into a validated commit plan.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

use crate::ai::{self, ProviderFailure, extract_json_block};
use crate::git::{recent_commit_style_examples, sorted_files};

const MAX_COMMIT_STYLE_EXAMPLES: usize = 6;
pub(crate) const MAX_DIFF_BYTES: usize = 20_000;

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

Return ONLY valid JSON. Absolutely no markdown or conversational text.
Schema: { \"groups\": [ { \"message\": \"feat(auth): implement JWT validation\", \"files\": [\"src/auth.rs\"] } ] }";

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub(crate) struct CommitGroup {
    pub(crate) message: String,
    pub(crate) files: Vec<String>,
}

#[derive(Deserialize)]
struct CommitGroupsEnvelope {
    groups: Vec<CommitGroup>,
}

pub(crate) async fn synthesize_groups(
    diff: &str,
    actual_files: &HashSet<String>,
) -> std::result::Result<(Vec<CommitGroup>, &'static str), Vec<ProviderFailure>> {
    let user = build_synthesis_input(diff, actual_files);

    let request = ai::Request {
        system: SYSTEM_PROMPT,
        user: &user,
        schema_name: "commit_groups",
        schema: groups_schema(),
    };

    ai::complete(&request, |raw| {
        parse_groups(raw).and_then(|groups| validate_group_coverage(groups, actual_files))
    })
    .await
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

fn groups_schema() -> serde_json::Value {
    serde_json::json!({
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
    })
}

fn build_synthesis_input(diff: &str, actual_files: &HashSet<String>) -> String {
    let mut prompt = String::new();
    let examples = recent_commit_style_examples(MAX_COMMIT_STYLE_EXAMPLES);

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
    prompt.push_str(truncate_for_prompt(diff, MAX_DIFF_BYTES));
    prompt
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

/// Accepts the three shapes models actually produce: a bare array, a
/// `{ "groups": [...] }` envelope, or either of those buried in prose/fences.
fn parse_groups(raw: &str) -> Result<Vec<CommitGroup>> {
    let raw = raw.trim();

    if let Ok(groups) = serde_json::from_str::<Vec<CommitGroup>>(raw)
        && !groups.is_empty()
    {
        return Ok(groups);
    }

    if let Ok(envelope) = serde_json::from_str::<CommitGroupsEnvelope>(raw)
        && !envelope.groups.is_empty()
    {
        return Ok(envelope.groups);
    }

    let embedded_array = extract_json_block(raw, '[', ']').unwrap_or("[]");
    let groups: Vec<CommitGroup> = serde_json::from_str(embedded_array)?;
    if groups.is_empty() {
        anyhow::bail!("Model reply contained no commit groups");
    }
    Ok(groups)
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
    use crate::test_support::{acquire_cwd_lock, git, init_repo, with_repo_cwd, write_file};

    fn assert_single_group(groups: Vec<CommitGroup>, message: &str, file: &str) {
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].message, message);
        assert_eq!(groups[0].files, vec![file.to_string()]);
    }

    #[test]
    fn parse_groups_accepts_bare_arrays() {
        let raw = r#"[{"message":"feat: add parser","files":["src/main.rs"]}]"#;
        let parsed = parse_groups(raw).expect("bare array should parse");

        assert_single_group(parsed, "feat: add parser", "src/main.rs");
    }

    #[test]
    fn parse_groups_accepts_groups_envelope_shape() {
        let raw = r#"{"groups":[{"message":"fix: tighten parsing","files":["src/main.rs"]}]}"#;
        let parsed = parse_groups(raw).expect("groups envelope should parse");

        assert_single_group(parsed, "fix: tighten parsing", "src/main.rs");
    }

    #[test]
    fn parse_groups_extracts_array_from_mixed_text() {
        let raw = "Result:\n```json\n[{\"message\":\"chore: update deps\",\"files\":[\"Cargo.toml\"]}]\n```";
        let parsed = parse_groups(raw).expect("embedded json array should parse");

        assert_single_group(parsed, "chore: update deps", "Cargo.toml");
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
            build_synthesis_input("diff --git a/src/main.rs b/src/main.rs", &actual_files)
        });

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
    fn build_synthesis_input_includes_commit_style_examples() {
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
            build_synthesis_input("abcdefghijklmnopqrstuvwxyz", &actual_files)
        });

        assert!(prompt.contains("Recent non-Kite commit message examples from this repository:"));
        assert!(prompt.contains("- docs: refresh usage"));
        assert!(prompt.contains("- fix(cli): tighten landing"));
    }

    #[test]
    fn truncate_for_prompt_respects_char_boundaries() {
        assert_eq!(truncate_for_prompt("abcdefgh", 8), "abcdefgh");
        assert_eq!(truncate_for_prompt("abcdefghij", 8), "abcdefgh");
        assert_eq!(truncate_for_prompt("héllo", 2), "h"); // no mid-codepoint cuts
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
