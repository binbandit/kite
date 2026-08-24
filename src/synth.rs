//! Turns the diff introduced by Kite saves into a validated commit plan.
//!
//! The model assigns every changed file to exactly one commit, so each commit
//! carries the complete change to the files it touches.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

use crate::ai::{self, extract_json_block};
use crate::diff::ChangedFiles;
use crate::git::{is_save_subject, recent_commit_style_examples};

const MAX_COMMIT_STYLE_EXAMPLES: usize = 6;
const MAX_SYNTHESIS_ATTEMPTS: usize = 3;
pub(crate) const MAX_DIFF_BYTES: usize = 60_000;

/// Used when the model returns a message Kite cannot let through.
const FALLBACK_COMMIT_MESSAGE: &str = "chore: update";

const SYSTEM_PROMPT: &str = "\
You are an expert version control synthesis engine. Analyze the git diff and group the changed files into distinct, atomic commits based on logical purpose. Every file lands whole in exactly one commit.
Write commit messages that match the repository's recent style examples when they show a clear pattern. If the examples are mixed or absent, fall back to a Conventional Commit style.

Rules for commit messages:
1. Match the repository's recent wording, prefixes, and scope style when those examples are consistent.
2. If no clear style emerges from the examples, use: <type>(<optional scope>): <description>
3. Use the imperative, present tense: 'add' not 'added' or 'adds'.
4. Keep the message concise and specific about technical intent.
5. No trailing periods.
6. Never emit `[kite] save` as a landed commit message.

Rules for file assignment:
1. Use only file paths that appear in the provided file list.
2. Copy each path exactly as provided.
3. Assign every path exactly once.
4. Do not omit paths.
5. Do not duplicate paths across groups.
6. Keep interdependent changes (a definition and its call sites) in the same commit so every commit is coherent on its own.
7. Order the groups so foundational changes come before the changes that depend on them.

Return ONLY valid JSON. Absolutely no markdown or conversational text.
Schema: { \"groups\": [ { \"message\": \"feat(auth): implement JWT validation\", \"files\": [\"src/auth.rs\", \"src/routes.rs\"] } ] }";

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub(crate) struct CommitGroup {
    pub(crate) message: String,
    pub(crate) files: Vec<String>,
}

#[derive(Deserialize)]
struct CommitGroupsEnvelope {
    groups: Vec<CommitGroup>,
}

/// Asks the AI for a commit plan, feeding coverage problems back for another
/// attempt. A reply that parses but leaves files unassigned is still accepted
/// after the retries run out — `normalize_groups` sweeps the leftovers into a
/// `chore: unclassified updates` commit, which beats dropping the user to a
/// single manual commit message over one missed path.
pub(crate) async fn synthesize_groups(files: &ChangedFiles) -> Result<Vec<CommitGroup>> {
    let input = build_synthesis_input(files);

    let mut feedback: Option<String> = None;
    let mut last_parsed: Option<Vec<CommitGroup>> = None;
    let mut last_error = anyhow::anyhow!("synthesis was not attempted");

    for _ in 0..MAX_SYNTHESIS_ATTEMPTS {
        let user = match &feedback {
            None => input.clone(),
            Some(problems) => format!(
                "{input}\n\nYour previous reply was rejected: {problems}.\nReturn corrected JSON that assigns every file path from the list exactly once."
            ),
        };

        let request = ai::Request {
            system: SYSTEM_PROMPT,
            user: &user,
            schema_name: "commit_groups",
            schema: groups_schema(),
        };

        match ai::complete(&request, parse_groups).await {
            Ok(groups) => match validate_group_coverage(&groups, files.paths()) {
                Ok(()) => return Ok(groups),
                Err(error) => {
                    feedback = Some(format!("{error:#}"));
                    last_parsed = Some(groups);
                    last_error = error;
                }
            },
            Err(error) => {
                // A rejected key, an unknown model or a schema the endpoint
                // will not accept fails identically every time; repeating it
                // only makes the user wait three times as long to find out.
                let worth_retrying = ai::is_retryable(&error);
                last_error = error;
                if !worth_retrying {
                    break;
                }
            }
        }
    }

    last_parsed.map(Ok).unwrap_or(Err(last_error))
}

/// Keeps the plan to exactly the changed files, once each: unknown and
/// duplicate paths are dropped, and anything the model left out lands in a
/// visible catch-all commit rather than being silently lost.
pub(crate) fn normalize_groups(groups: Vec<CommitGroup>, files: &ChangedFiles) -> Vec<CommitGroup> {
    let mut remaining: HashSet<&str> = files.paths().iter().map(String::as_str).collect();
    let mut normalized = Vec::new();

    for group in groups {
        let assigned: Vec<String> = group
            .files
            .into_iter()
            .filter(|path| remaining.remove(path.as_str()))
            .collect();

        if !assigned.is_empty() {
            normalized.push(CommitGroup {
                message: sanitize_commit_message(&group.message),
                files: assigned,
            });
        }
    }

    let unclassified: Vec<String> = files
        .paths()
        .iter()
        .filter(|path| remaining.contains(path.as_str()))
        .cloned()
        .collect();

    if !unclassified.is_empty() {
        normalized.push(CommitGroup {
            message: "chore: unclassified updates".to_string(),
            files: unclassified,
        });
    }

    normalized
}

/// A landed commit whose subject looks like a Kite save is indistinguishable
/// from an unlanded one: `kt` reports work still to land, `kt land` re-lands
/// it forever, and `kt pr` refuses to open a pull request. The system prompt
/// forbids it — this makes it impossible. Also collapses blank messages,
/// which `git commit -m ""` would reject mid-rewrite.
pub(crate) fn sanitize_commit_message(message: &str) -> String {
    let trimmed = message.trim();
    let subject = trimmed.lines().next().unwrap_or("").trim();

    if subject.is_empty() || is_save_subject(subject) {
        return FALLBACK_COMMIT_MESSAGE.to_string();
    }

    trimmed.to_string()
}

/// Deliberately free of `minItems`/`minLength`. Strict structured-output
/// implementations — and OpenAI-compatible proxies especially — reject
/// keywords outside the supported subset with a 400, which would take the AI
/// path down entirely. Emptiness is checked in `parse_groups` and coverage in
/// `validate_group_coverage`, where a bad reply can be retried instead.
fn groups_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["groups"],
        "properties": {
            "groups": {
                "type": "array",
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["message", "files"],
                    "properties": {
                        "message": { "type": "string" },
                        "files": {
                            "type": "array",
                            "items": { "type": "string" }
                        }
                    }
                }
            }
        }
    })
}

fn build_synthesis_input(files: &ChangedFiles) -> String {
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

    prompt.push_str("Files (assign each path exactly once and copy paths verbatim):\n");
    prompt.push_str(&files.render_index());
    prompt.push('\n');

    prompt.push_str("Diff (may be trimmed; rely on the file list for full coverage):\n");
    prompt.push_str(truncate_for_prompt(
        &files.render_diff(MAX_DIFF_BYTES),
        MAX_DIFF_BYTES,
    ));
    prompt
}

fn validate_group_coverage(groups: &[CommitGroup], paths: &[String]) -> Result<()> {
    let known: HashSet<&String> = paths.iter().collect();
    let mut seen = HashSet::new();
    let mut duplicates = Vec::new();
    let mut unknown = Vec::new();

    for group in groups {
        for path in &group.files {
            if !known.contains(path) {
                unknown.push(path.clone());
                continue;
            }

            if !seen.insert(path.clone()) {
                duplicates.push(path.clone());
            }
        }
    }

    let missing: Vec<String> = paths
        .iter()
        .filter(|path| !seen.contains(*path))
        .cloned()
        .collect();

    if duplicates.is_empty() && unknown.is_empty() && missing.is_empty() {
        return Ok(());
    }

    let mut problems = Vec::new();

    if !missing.is_empty() {
        problems.push(format!("missing files: {}", missing.join(", ")));
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
    use crate::diff::ChangedFiles;
    use crate::test_support::{acquire_cwd_lock, git, init_repo, with_repo_cwd, write_file};

    const SAMPLE_DIFF: &str = "\
diff --git a/src/main.rs b/src/main.rs
index 1111111..2222222 100644
--- a/src/main.rs
+++ b/src/main.rs
@@ -1,3 +1,4 @@
 fn main() {
+    init();
 }
 // tail
@@ -10,3 +11,4 @@
 fn helper() {
+    log();
 }
 // end
diff --git a/README.md b/README.md
index 3333333..4444444 100644
--- a/README.md
+++ b/README.md
@@ -1,1 +1,2 @@
 # Title
+More docs
";

    fn paths(paths: &[&str]) -> Vec<String> {
        paths.iter().map(|path| path.to_string()).collect()
    }

    fn sample_files() -> ChangedFiles {
        ChangedFiles::new(
            paths(&["src/main.rs", "README.md"]),
            SAMPLE_DIFF.to_string(),
        )
    }

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
        let raw = r#"{"groups":[{"message":"fix: tighten parsing","files":["README.md"]}]}"#;
        let parsed = parse_groups(raw).expect("groups envelope should parse");

        assert_single_group(parsed, "fix: tighten parsing", "README.md");
    }

    #[test]
    fn parse_groups_extracts_array_from_mixed_text() {
        let raw = "Result:\n```json\n[{\"message\":\"chore: update deps\",\"files\":[\"Cargo.toml\"]}]\n```";
        let parsed = parse_groups(raw).expect("embedded json array should parse");

        assert_single_group(parsed, "chore: update deps", "Cargo.toml");
    }

    #[test]
    fn validate_group_coverage_requires_full_exact_assignment() {
        let changed = paths(&["src/main.rs", "README.md"]);

        validate_group_coverage(
            &[
                CommitGroup {
                    message: "feat(cli): improve landing".to_string(),
                    files: vec!["src/main.rs".to_string()],
                },
                CommitGroup {
                    message: "docs: refresh readme".to_string(),
                    files: vec!["README.md".to_string()],
                },
            ],
            &changed,
        )
        .expect("full coverage should validate");

        let err = validate_group_coverage(
            &[CommitGroup {
                message: "feat(cli): improve landing".to_string(),
                files: vec!["src/main.rs".to_string()],
            }],
            &changed,
        )
        .expect_err("a missing file should fail validation");
        assert!(format!("{err:#}").contains("missing files: README.md"));
    }

    #[test]
    fn validate_group_coverage_rejects_duplicate_and_unknown_files() {
        let changed = paths(&["src/main.rs", "README.md"]);

        let err = validate_group_coverage(
            &[
                CommitGroup {
                    message: "feat(cli): improve landing".to_string(),
                    files: vec!["src/main.rs".to_string(), "src/main.rs".to_string()],
                },
                CommitGroup {
                    message: "docs: refresh readme".to_string(),
                    files: vec!["src/imagined.rs".to_string()],
                },
            ],
            &changed,
        )
        .expect_err("duplicate and unknown files should fail validation");

        let rendered = format!("{err:#}");
        assert!(rendered.contains("missing files: README.md"));
        assert!(rendered.contains("duplicate files: src/main.rs"));
        assert!(rendered.contains("unknown files: src/imagined.rs"));
    }

    #[test]
    fn build_synthesis_input_lists_every_changed_file() {
        let _lock = acquire_cwd_lock();
        let repo = init_repo();
        let files = sample_files();

        let prompt = with_repo_cwd(&repo.path, || build_synthesis_input(&files));

        assert!(prompt.contains("Files (assign each path exactly once and copy paths verbatim):"));
        assert!(prompt.contains("- src/main.rs\n"));
        assert!(prompt.contains("- README.md\n"));
        assert!(prompt.contains("Diff (may be trimmed; rely on the file list for full coverage):"));
        assert!(prompt.contains("diff --git a/src/main.rs b/src/main.rs"));
        assert!(prompt.contains("+    init();"));
        // One file, one entry: both of src/main.rs's hunks stay together.
        assert_eq!(prompt.matches("- src/main.rs\n").count(), 1);
    }

    #[test]
    fn build_synthesis_input_includes_commit_style_examples() {
        let _lock = acquire_cwd_lock();
        let repo = init_repo();
        let files = sample_files();

        write_file(&repo.path, "tracked.txt", "first\n");
        git(&repo.path, &["add", "tracked.txt"]);
        git(&repo.path, &["commit", "-m", "fix(cli): tighten landing"]);

        write_file(&repo.path, "tracked.txt", "second\n");
        git(&repo.path, &["add", "tracked.txt"]);
        git(&repo.path, &["commit", "-m", "docs: refresh usage"]);

        let prompt = with_repo_cwd(&repo.path, || build_synthesis_input(&files));

        assert!(prompt.contains("Recent non-Kite commit message examples from this repository:"));
        assert!(prompt.contains("- docs: refresh usage"));
        assert!(prompt.contains("- fix(cli): tighten landing"));
    }

    #[test]
    fn sanitize_commit_message_never_lets_a_save_subject_through() {
        // A landed commit that looks like a save makes the branch unlandable
        // forever: kt keeps reporting saves, kt land re-lands, kt pr refuses.
        assert_eq!(
            sanitize_commit_message("[kite] save 09:00:00"),
            "chore: update"
        );
        assert_eq!(
            sanitize_commit_message("  [kite] save 09:00:00  "),
            "chore: update"
        );
        assert_eq!(sanitize_commit_message("   "), "chore: update");

        // Mentioning the prefix mid-subject is fine; only the subject counts.
        assert_eq!(
            sanitize_commit_message("fix: ignore [kite] save prefixes"),
            "fix: ignore [kite] save prefixes"
        );
        assert_eq!(
            sanitize_commit_message("feat: add thing\n\nWith a body."),
            "feat: add thing\n\nWith a body."
        );
    }

    #[test]
    fn normalize_groups_rewrites_save_shaped_messages() {
        let files = sample_files();

        let normalized = normalize_groups(
            vec![CommitGroup {
                message: "[kite] save 12:00:00".to_string(),
                files: vec!["src/main.rs".to_string(), "README.md".to_string()],
            }],
            &files,
        );

        assert_eq!(normalized.len(), 1);
        assert_eq!(normalized[0].message, "chore: update");
    }

    #[test]
    fn truncate_for_prompt_respects_char_boundaries() {
        assert_eq!(truncate_for_prompt("abcdefgh", 8), "abcdefgh");
        assert_eq!(truncate_for_prompt("abcdefghij", 8), "abcdefgh");
        assert_eq!(truncate_for_prompt("héllo", 2), "h"); // no mid-codepoint cuts
    }

    #[test]
    fn normalize_groups_drops_noise_and_sweeps_forgotten_files_into_a_chore_commit() {
        let files = sample_files();

        let normalized = normalize_groups(
            vec![CommitGroup {
                message: "feat(cli): tighten landing".to_string(),
                files: vec![
                    "src/main.rs".to_string(),
                    "src/main.rs".to_string(),
                    "src/imagined.rs".to_string(),
                ],
            }],
            &files,
        );

        // The repeat and the invented path are dropped; the file the model
        // forgot still lands, in a visible catch-all commit.
        assert_eq!(normalized.len(), 2);
        assert_eq!(normalized[0].files, vec!["src/main.rs".to_string()]);
        assert_eq!(normalized[1].message, "chore: unclassified updates");
        assert_eq!(normalized[1].files, vec!["README.md".to_string()]);
    }
}
