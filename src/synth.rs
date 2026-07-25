//! Turns the diff introduced by Kite saves into a validated commit plan.
//!
//! The diff is split into labeled hunks (see `hunks`), and the model assigns
//! every hunk id to exactly one commit — so one file's changes can land as
//! several commits when they serve different purposes.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

use crate::ai::{self, extract_json_block};
use crate::git::{is_save_subject, recent_commit_style_examples};
use crate::hunks::{DiffUnits, MIN_UNIT_BODY_BYTES};

const MAX_COMMIT_STYLE_EXAMPLES: usize = 6;
const MAX_SYNTHESIS_ATTEMPTS: usize = 3;
pub(crate) const MAX_DIFF_BYTES: usize = 60_000;

/// Past this many units the body budget cannot give each hunk enough content
/// for the model to tell them apart, and the prompt grows without bound. A
/// land that big groups whole files instead — see `DiffUnits::coarsened_to_files`.
pub(crate) const MAX_PLANNABLE_UNITS: usize = MAX_DIFF_BYTES / MIN_UNIT_BODY_BYTES;

/// Used when the model returns a message Kite cannot let through.
const FALLBACK_COMMIT_MESSAGE: &str = "chore: update";

const SYSTEM_PROMPT: &str = "\
You are an expert version control synthesis engine. Analyze the git diff, which is split into labeled hunks (h1, h2, ...). Some hunks represent a whole file that cannot be split.
Group the hunks into distinct, atomic commits based on logical purpose. Hunks from the same file may go into different commits when they serve different purposes.
Write commit messages that match the repository's recent style examples when they show a clear pattern. If the examples are mixed or absent, fall back to a Conventional Commit style.

Rules for commit messages:
1. Match the repository's recent wording, prefixes, and scope style when those examples are consistent.
2. If no clear style emerges from the examples, use: <type>(<optional scope>): <description>
3. Use the imperative, present tense: 'add' not 'added' or 'adds'.
4. Keep the message concise and specific about technical intent.
5. No trailing periods.
6. Never emit `[kite] save` as a landed commit message.

Rules for hunk assignment:
1. Use only hunk ids that appear in the provided hunk list.
2. Copy each hunk id exactly as provided.
3. Assign every hunk id exactly once.
4. Do not omit hunk ids.
5. Do not duplicate hunk ids across groups.
6. Keep interdependent changes (a definition and its call sites) in the same commit so every commit is coherent on its own.
7. Order the groups so foundational changes come before the changes that depend on them.

Return ONLY valid JSON. Absolutely no markdown or conversational text.
Schema: { \"groups\": [ { \"message\": \"feat(auth): implement JWT validation\", \"hunks\": [\"h1\", \"h4\"] } ] }";

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub(crate) struct CommitGroup {
    pub(crate) message: String,
    pub(crate) hunks: Vec<String>,
}

#[derive(Deserialize)]
struct CommitGroupsEnvelope {
    groups: Vec<CommitGroup>,
}

/// Asks the AI for a commit plan, feeding coverage problems back for another
/// attempt. A reply that parses but leaves hunks unassigned is still accepted
/// after the retries run out — `normalize_groups` sweeps the leftovers into a
/// `chore: unclassified updates` commit, which beats dropping the user to a
/// single manual commit message over one missed id.
pub(crate) async fn synthesize_groups(units: &DiffUnits) -> Result<Vec<CommitGroup>> {
    let unit_ids = units.unit_ids();
    let input = build_synthesis_input(units);

    let mut feedback: Option<String> = None;
    let mut last_parsed: Option<Vec<CommitGroup>> = None;
    let mut last_error = anyhow::anyhow!("synthesis was not attempted");

    for _ in 0..MAX_SYNTHESIS_ATTEMPTS {
        let user = match &feedback {
            None => input.clone(),
            Some(problems) => format!(
                "{input}\n\nYour previous reply was rejected: {problems}.\nReturn corrected JSON that assigns every hunk id from the list exactly once."
            ),
        };

        let request = ai::Request {
            system: SYSTEM_PROMPT,
            user: &user,
            schema_name: "commit_groups",
            schema: groups_schema(),
        };

        match ai::complete(&request, parse_groups).await {
            Ok(groups) => match validate_group_coverage(&groups, &unit_ids) {
                Ok(()) => return Ok(groups),
                Err(error) => {
                    feedback = Some(format!("{error:#}"));
                    last_parsed = Some(groups);
                    last_error = error;
                }
            },
            Err(error) => last_error = error,
        }
    }

    last_parsed.map(Ok).unwrap_or(Err(last_error))
}

pub(crate) fn normalize_groups(groups: Vec<CommitGroup>, units: &DiffUnits) -> Vec<CommitGroup> {
    let unit_ids = units.unit_ids();
    let mut remaining: HashSet<String> = unit_ids.iter().cloned().collect();
    let mut normalized = Vec::new();

    for group in groups {
        let hunks: Vec<String> = group
            .hunks
            .into_iter()
            .filter(|id| remaining.remove(id))
            .collect();

        if !hunks.is_empty() {
            normalized.push(CommitGroup {
                message: sanitize_commit_message(&group.message),
                hunks,
            });
        }
    }

    // A leftover hunk joins the only group already touching its file — the
    // same commit file-level grouping would have chosen. Anything ambiguous
    // lands in a visible catch-all commit instead of being dropped.
    let mut unclassified = Vec::new();
    for id in unit_ids.iter().filter(|id| remaining.contains(*id)) {
        let path = units.path_for(id);
        let owners: Vec<usize> = normalized
            .iter()
            .enumerate()
            .filter(|(_, group)| {
                path.is_some() && group.hunks.iter().any(|hunk| units.path_for(hunk) == path)
            })
            .map(|(index, _)| index)
            .collect();

        match owners[..] {
            [owner] => normalized[owner].hunks.push(id.clone()),
            _ => unclassified.push(id.clone()),
        }
    }

    if !unclassified.is_empty() {
        normalized.push(CommitGroup {
            message: "chore: unclassified updates".to_string(),
            hunks: unclassified,
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
                    "required": ["message", "hunks"],
                    "properties": {
                        "message": { "type": "string", "minLength": 1 },
                        "hunks": {
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

fn build_synthesis_input(units: &DiffUnits) -> String {
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

    prompt.push_str("Hunks (assign each id exactly once and copy ids verbatim):\n");
    prompt.push_str(&units.render_unit_index());
    prompt.push('\n');

    prompt.push_str("Hunk contents (may be trimmed; rely on the hunk list for full coverage):\n");
    let bodies = units.render_unit_bodies(MAX_DIFF_BYTES);
    prompt.push_str(truncate_for_prompt(&bodies, MAX_DIFF_BYTES));
    prompt
}

fn validate_group_coverage(groups: &[CommitGroup], unit_ids: &[String]) -> Result<()> {
    let known: HashSet<&String> = unit_ids.iter().collect();
    let mut seen = HashSet::new();
    let mut duplicates = Vec::new();
    let mut unknown = Vec::new();

    for group in groups {
        for id in &group.hunks {
            if !known.contains(id) {
                unknown.push(id.clone());
                continue;
            }

            if !seen.insert(id.clone()) {
                duplicates.push(id.clone());
            }
        }
    }

    let missing: Vec<String> = unit_ids
        .iter()
        .filter(|id| !seen.contains(*id))
        .cloned()
        .collect();

    if duplicates.is_empty() && unknown.is_empty() && missing.is_empty() {
        return Ok(());
    }

    let mut problems = Vec::new();

    if !missing.is_empty() {
        problems.push(format!("missing hunks: {}", missing.join(", ")));
    }

    if !duplicates.is_empty() {
        duplicates.sort();
        duplicates.dedup();
        problems.push(format!("duplicate hunks: {}", duplicates.join(", ")));
    }

    if !unknown.is_empty() {
        unknown.sort();
        unknown.dedup();
        problems.push(format!("unknown hunks: {}", unknown.join(", ")));
    }

    anyhow::bail!(
        "Synthesis output did not cover the diff hunks correctly ({})",
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
    use crate::hunks::parse_diff;
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

    fn unit_ids(ids: &[&str]) -> Vec<String> {
        ids.iter().map(|id| id.to_string()).collect()
    }

    fn assert_single_group(groups: Vec<CommitGroup>, message: &str, hunk: &str) {
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].message, message);
        assert_eq!(groups[0].hunks, vec![hunk.to_string()]);
    }

    #[test]
    fn parse_groups_accepts_bare_arrays() {
        let raw = r#"[{"message":"feat: add parser","hunks":["h1"]}]"#;
        let parsed = parse_groups(raw).expect("bare array should parse");

        assert_single_group(parsed, "feat: add parser", "h1");
    }

    #[test]
    fn parse_groups_accepts_groups_envelope_shape() {
        let raw = r#"{"groups":[{"message":"fix: tighten parsing","hunks":["h2"]}]}"#;
        let parsed = parse_groups(raw).expect("groups envelope should parse");

        assert_single_group(parsed, "fix: tighten parsing", "h2");
    }

    #[test]
    fn parse_groups_extracts_array_from_mixed_text() {
        let raw =
            "Result:\n```json\n[{\"message\":\"chore: update deps\",\"hunks\":[\"h1\"]}]\n```";
        let parsed = parse_groups(raw).expect("embedded json array should parse");

        assert_single_group(parsed, "chore: update deps", "h1");
    }

    #[test]
    fn validate_group_coverage_requires_full_exact_assignment() {
        let ids = unit_ids(&["h1", "h2"]);

        validate_group_coverage(
            &[
                CommitGroup {
                    message: "feat(cli): improve landing".to_string(),
                    hunks: vec!["h1".to_string()],
                },
                CommitGroup {
                    message: "docs: refresh readme".to_string(),
                    hunks: vec!["h2".to_string()],
                },
            ],
            &ids,
        )
        .expect("full coverage should validate");

        let err = validate_group_coverage(
            &[CommitGroup {
                message: "feat(cli): improve landing".to_string(),
                hunks: vec!["h1".to_string()],
            }],
            &ids,
        )
        .expect_err("missing hunks should fail validation");
        assert!(format!("{err:#}").contains("missing hunks: h2"));
    }

    #[test]
    fn validate_group_coverage_rejects_duplicate_and_unknown_hunks() {
        let ids = unit_ids(&["h1", "h2"]);

        let err = validate_group_coverage(
            &[
                CommitGroup {
                    message: "feat(cli): improve landing".to_string(),
                    hunks: vec!["h1".to_string(), "h1".to_string()],
                },
                CommitGroup {
                    message: "docs: refresh readme".to_string(),
                    hunks: vec!["h99".to_string()],
                },
            ],
            &ids,
        )
        .expect_err("duplicate and unknown hunks should fail validation");

        let rendered = format!("{err:#}");
        assert!(rendered.contains("missing hunks: h2"));
        assert!(rendered.contains("duplicate hunks: h1"));
        assert!(rendered.contains("unknown hunks: h99"));
    }

    #[test]
    fn build_synthesis_input_lists_every_hunk_id() {
        let _lock = acquire_cwd_lock();
        let repo = init_repo();
        let units = parse_diff(SAMPLE_DIFF);

        let prompt = with_repo_cwd(&repo.path, || build_synthesis_input(&units));

        assert!(prompt.contains("Hunks (assign each id exactly once and copy ids verbatim):"));
        assert!(prompt.contains("- h1 src/main.rs @@ -1,3 +1,4 @@"));
        assert!(prompt.contains("- h2 src/main.rs @@ -10,3 +11,4 @@"));
        assert!(prompt.contains("- h3 README.md @@ -1,1 +1,2 @@"));
        assert!(
            prompt.contains(
                "Hunk contents (may be trimmed; rely on the hunk list for full coverage):"
            )
        );
        assert!(prompt.contains("[h1] src/main.rs"));
        assert!(prompt.contains("+    init();"));
    }

    #[test]
    fn build_synthesis_input_includes_commit_style_examples() {
        let _lock = acquire_cwd_lock();
        let repo = init_repo();
        let units = parse_diff(SAMPLE_DIFF);

        write_file(&repo.path, "tracked.txt", "first\n");
        git(&repo.path, &["add", "tracked.txt"]);
        git(&repo.path, &["commit", "-m", "fix(cli): tighten landing"]);

        write_file(&repo.path, "tracked.txt", "second\n");
        git(&repo.path, &["add", "tracked.txt"]);
        git(&repo.path, &["commit", "-m", "docs: refresh usage"]);

        let prompt = with_repo_cwd(&repo.path, || build_synthesis_input(&units));

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
        let units = parse_diff(SAMPLE_DIFF);

        let normalized = normalize_groups(
            vec![CommitGroup {
                message: "[kite] save 12:00:00".to_string(),
                hunks: vec!["h1".to_string(), "h2".to_string(), "h3".to_string()],
            }],
            &units,
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
    fn normalize_groups_joins_leftovers_to_their_files_group_or_a_chore_commit() {
        // SAMPLE_DIFF: h1 and h2 in src/main.rs, h3 in README.md.
        let units = parse_diff(SAMPLE_DIFF);

        let normalized = normalize_groups(
            vec![CommitGroup {
                message: "feat(cli): tighten landing".to_string(),
                hunks: vec!["h2".to_string(), "h2".to_string(), "h99".to_string()],
            }],
            &units,
        );

        // h1 joins the group that already owns src/main.rs; h3 has no owner.
        assert_eq!(normalized.len(), 2);
        assert_eq!(
            normalized[0].hunks,
            vec!["h2".to_string(), "h1".to_string()]
        );
        assert_eq!(normalized[1].message, "chore: unclassified updates");
        assert_eq!(normalized[1].hunks, vec!["h3".to_string()]);
    }
}
