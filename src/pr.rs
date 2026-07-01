//! `kt pr` — open a GitHub pull request for the current branch with `gh`.
//!
//! The command gathers everything a good pull request needs — the branch's
//! commits and diff, the repository's PR template, recent PR titles for style,
//! and any PR-related agent skills installed on the machine — then asks the AI
//! provider cascade for a title and body. Without AI it falls back to a
//! deterministic draft. Nothing is created until the user approves the preview.

use anyhow::{Context, Result};
use colored::*;
use serde::Deserialize;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::ai::{self, extract_json_block};
use crate::git::{
    SAVE_PREFIX, check_ref, execute_git, get_current_branch, get_default_branch, has_remote,
    kite_save_stack, repo_root,
};
use crate::land::publish_current_branch;
use crate::synth::truncate_for_prompt;
use crate::ui::{Spinner, confirm, pluralize, print_provider_failures};

const MAX_COMMIT_SUBJECTS: usize = 50;
const MAX_PR_TITLE_EXAMPLES: usize = 8;
const MAX_SKILLS: usize = 3;
const MAX_SKILL_BYTES: usize = 4_000;
const MAX_TEMPLATE_BYTES: usize = 6_000;
const MAX_DIFF_BYTES: usize = 15_000;

const SYSTEM_PROMPT: &str = "\
You write pull requests for software teams. Using the branch's commits and diff, produce a pull request title and body.

Title rules:
1. Match the style of the recent pull request titles when they show a clear pattern; otherwise use Conventional Commit style.
2. Use the imperative, present tense. Keep it concise and specific. No trailing period.

Body rules:
1. If a template is provided, follow its structure exactly: keep its headings and checklists, fill each section with real content, and drop instructional HTML comments.
2. Without a template, write a short summary paragraph followed by a bulleted list of notable changes.
3. Describe only changes that appear in the commits or diff. Never invent content or leave placeholders.
4. Use GitHub-flavored markdown.
5. If skill guidance is provided, follow it wherever it does not conflict with the template.

Return ONLY valid JSON: { \"title\": \"...\", \"body\": \"...\" }";

pub(crate) struct PrOptions {
    pub(crate) draft: bool,
    pub(crate) base: Option<String>,
    pub(crate) yes: bool,
}

#[derive(Deserialize, Debug, PartialEq, Eq)]
struct PrDraft {
    title: String,
    body: String,
}

/// A named piece of guidance (a PR template or an agent skill) fed to the AI.
struct Guidance {
    label: String,
    content: String,
}

struct PrContext {
    branch: String,
    base: String,
    commits: Vec<String>,
    diff: String,
    template: Option<Guidance>,
    skills: Vec<Guidance>,
    title_examples: Vec<String>,
}

pub(crate) async fn create_pull_request(options: PrOptions) -> Result<()> {
    ensure_gh_available()?;
    if !has_remote() {
        anyhow::bail!(
            "A remote is required to open a pull request. Add one with `git remote add origin <url>`."
        );
    }

    let branch = get_current_branch()?;
    let base = match options.base {
        Some(base) => base,
        None => get_default_branch()?,
    };
    if branch == base {
        anyhow::bail!(
            "You are on `{base}`. Start a flow branch with `kt go <name>`, land your work, then run `kt pr`."
        );
    }

    if let Some(stack) = kite_save_stack()? {
        anyhow::bail!(
            "This branch has {} unlanded — run `kt land` first so the pull request shows polished commits.",
            pluralize(stack.count, "save")
        );
    }

    println!(
        "{} {} {} {}",
        "·".cyan(),
        branch.bold(),
        "→".dimmed(),
        base.bold()
    );

    sync_branch_to_remote(&branch)?;

    if let Some(url) = existing_pr_url() {
        println!("{} Already open: {url}", "·".cyan());
        return Ok(());
    }

    let context = collect_pr_context(branch, base)?;
    announce_guidance(&context);

    let spinner = Spinner::start("Drafting");
    let drafted = draft_with_ai(&context).await;
    spinner.stop();

    let (draft, provider_label) = match drafted {
        Ok(result) => result,
        Err(failures) => {
            print_provider_failures(&failures);
            (fallback_draft(&context), "manual")
        }
    };

    println!("{} Draft ({provider_label}):", "·".cyan());
    print!("{}", render_preview(&draft));

    if !options.yes && !confirm("Create pull request?")? {
        println!("{} Aborted — no pull request created", "·".red());
        return Ok(());
    }

    let url = gh_pr_create(&draft, &context.base, options.draft)?;
    println!("{} {}", "✓".green(), url.trim());
    Ok(())
}

fn ensure_gh_available() -> Result<()> {
    let available = Command::new("gh")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false);

    if !available {
        anyhow::bail!(
            "`kt pr` needs the GitHub CLI. Install it from https://cli.github.com, then run `gh auth login`."
        );
    }
    Ok(())
}

fn gh(args: &[&str]) -> Result<String> {
    let output = Command::new("gh")
        .args(args)
        .current_dir(repo_root()?)
        .output()
        .with_context(|| format!("Failed 'gh {}'", args.join(" ")))?;

    if !output.status.success() {
        anyhow::bail!(
            "GitHub CLI error: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

/// Pushes the branch when the remote is missing it or behind it, so `gh pr
/// create` always sees the commits we are describing.
fn sync_branch_to_remote(branch: &str) -> Result<()> {
    let head = execute_git(&["rev-parse", "HEAD"])?;
    let remote_ref = format!("refs/remotes/origin/{branch}");

    if check_ref(&remote_ref).as_deref() != Some(head.trim()) {
        publish_current_branch()?;
    }
    Ok(())
}

fn existing_pr_url() -> Option<String> {
    let url = gh(&["pr", "view", "--json", "url", "--jq", ".url"]).ok()?;
    let url = url.trim();
    (!url.is_empty()).then(|| url.to_string())
}

fn collect_pr_context(branch: String, base: String) -> Result<PrContext> {
    // Prefer the remote base so the PR diff matches what GitHub will show.
    let base_ref = check_ref(&format!("refs/remotes/origin/{base}"))
        .map(|_| format!("origin/{base}"))
        .unwrap_or_else(|| base.clone());

    // Two-dot for log (commits unique to this branch), three-dot for diff
    // (changes since the merge base) — matching what the GitHub PR will show.
    let commits: Vec<String> = execute_git(&["log", "--format=%s", &format!("{base_ref}..HEAD")])?
        .lines()
        .map(str::trim)
        .filter(|subject| !subject.is_empty() && !subject.starts_with(SAVE_PREFIX))
        .take(MAX_COMMIT_SUBJECTS)
        .map(ToOwned::to_owned)
        .collect();

    if commits.is_empty() {
        anyhow::bail!(
            "No commits found between `{base_ref}` and this branch. Nothing to open a pull request for."
        );
    }

    let diff = execute_git(&["diff", &format!("{base_ref}...HEAD")])?;
    let root = repo_root()?;

    Ok(PrContext {
        branch,
        base,
        commits,
        diff,
        template: find_pr_template(&root),
        skills: find_pr_skills(&root),
        title_examples: merged_pr_titles(),
    })
}

fn announce_guidance(context: &PrContext) {
    if let Some(template) = &context.template {
        println!("{} Template {}", "·".cyan(), template.label.dimmed());
    }
    if !context.skills.is_empty() {
        let names: Vec<&str> = context.skills.iter().map(|s| s.label.as_str()).collect();
        println!("{} Skills {}", "·".cyan(), names.join(", ").dimmed());
    }
}

/// Finds the repository's pull request template in the places GitHub looks:
/// the root, `.github/`, and `docs/`, plus the `.github/PULL_REQUEST_TEMPLATE/`
/// multi-template directory. Matching is case-insensitive.
fn find_pr_template(root: &Path) -> Option<Guidance> {
    for dir in [root.to_path_buf(), root.join(".github"), root.join("docs")] {
        if let Some(path) = find_entry_case_insensitive(&dir, "pull_request_template.md")
            && path.is_file()
        {
            return read_guidance(root, &path, MAX_TEMPLATE_BYTES);
        }
    }

    let template_dir = find_entry_case_insensitive(&root.join(".github"), "PULL_REQUEST_TEMPLATE")
        .filter(|path| path.is_dir())?;
    let mut templates: Vec<PathBuf> = std::fs::read_dir(template_dir)
        .ok()?
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("md"))
        })
        .collect();
    templates.sort();
    read_guidance(root, templates.first()?, MAX_TEMPLATE_BYTES)
}

/// Scans the machine for agent skills (SKILL.md files) that are about pull
/// requests, so `kt pr` follows the same guidance the user's AI agents do.
/// Project skills win over per-user skills of the same name.
fn find_pr_skills(root: &Path) -> Vec<Guidance> {
    let mut skill_dirs = vec![
        root.join(".claude/skills"),
        root.join(".agents/skills"),
        root.join("skills"),
    ];
    if let Some(home) = home_dir() {
        skill_dirs.extend([
            home.join(".claude/skills"),
            home.join(".codex/skills"),
            home.join(".agents/skills"),
        ]);
    }

    find_pr_skills_in(&skill_dirs)
}

fn find_pr_skills_in(skill_dirs: &[PathBuf]) -> Vec<Guidance> {
    let mut seen = HashSet::new();
    let mut skills = Vec::new();

    for dir in skill_dirs {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };

        let mut skill_homes: Vec<PathBuf> = entries.flatten().map(|entry| entry.path()).collect();
        skill_homes.sort();

        for skill_home in skill_homes {
            let Some(name) = skill_home.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            let Some(manifest) = find_entry_case_insensitive(&skill_home, "SKILL.md") else {
                continue;
            };
            let Ok(content) = std::fs::read_to_string(&manifest) else {
                continue;
            };

            if mentions_pull_requests(name, &content) && seen.insert(name.to_string()) {
                skills.push(Guidance {
                    label: name.to_string(),
                    content: truncate_for_prompt(&content, MAX_SKILL_BYTES).to_string(),
                });
                if skills.len() >= MAX_SKILLS {
                    return skills;
                }
            }
        }
    }

    skills
}

fn mentions_pull_requests(name: &str, content: &str) -> bool {
    let name_says_pr = name
        .split(|c: char| !c.is_ascii_alphanumeric())
        .any(|token| token.eq_ignore_ascii_case("pr") || token.eq_ignore_ascii_case("prs"));

    let content = content.to_ascii_lowercase();
    name_says_pr || content.contains("pull request") || content.contains("pull-request")
}

fn find_entry_case_insensitive(dir: &Path, name: &str) -> Option<PathBuf> {
    std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .map(|entry| entry.path())
        .find(|path| {
            path.file_name()
                .and_then(|file_name| file_name.to_str())
                .is_some_and(|file_name| file_name.eq_ignore_ascii_case(name))
        })
}

fn read_guidance(root: &Path, path: &Path, max_bytes: usize) -> Option<Guidance> {
    let content = std::fs::read_to_string(path).ok()?;
    let label = path
        .strip_prefix(root)
        .unwrap_or(path)
        .display()
        .to_string();

    Some(Guidance {
        label,
        content: truncate_for_prompt(&content, max_bytes).to_string(),
    })
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

fn merged_pr_titles() -> Vec<String> {
    let limit = MAX_PR_TITLE_EXAMPLES.to_string();
    gh(&[
        "pr",
        "list",
        "--state",
        "merged",
        "--limit",
        &limit,
        "--json",
        "title",
        "--jq",
        ".[].title",
    ])
    .map(|output| {
        output
            .lines()
            .map(str::trim)
            .filter(|title| !title.is_empty())
            .map(ToOwned::to_owned)
            .collect()
    })
    .unwrap_or_default()
}

async fn draft_with_ai(
    context: &PrContext,
) -> std::result::Result<(PrDraft, &'static str), Vec<ai::ProviderFailure>> {
    let user = build_pr_input(context);
    let request = ai::Request {
        system: SYSTEM_PROMPT,
        user: &user,
        schema_name: "pull_request",
        schema: draft_schema(),
    };

    ai::complete(&request, parse_draft).await
}

fn draft_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["title", "body"],
        "properties": {
            "title": { "type": "string", "minLength": 1 },
            "body": { "type": "string", "minLength": 1 }
        }
    })
}

fn build_pr_input(context: &PrContext) -> String {
    let mut input = format!(
        "Branch: {}\nBase branch: {}\n\n",
        context.branch, context.base
    );

    if !context.title_examples.is_empty() {
        input.push_str("Recent pull request titles from this repository:\n");
        for title in &context.title_examples {
            input.push_str(&format!("- {title}\n"));
        }
        input.push('\n');
    }

    if let Some(template) = &context.template {
        input.push_str(&format!(
            "Pull request template ({}) — follow its structure exactly:\n{}\n\n",
            template.label, template.content
        ));
    }

    for skill in &context.skills {
        input.push_str(&format!(
            "Guidance from skill {}:\n{}\n\n",
            skill.label, skill.content
        ));
    }

    input.push_str("Commits on this branch:\n");
    for subject in &context.commits {
        input.push_str(&format!("- {subject}\n"));
    }
    input.push('\n');

    input.push_str("Diff (may be truncated):\n");
    input.push_str(truncate_for_prompt(&context.diff, MAX_DIFF_BYTES));
    input
}

fn parse_draft(raw: &str) -> Result<PrDraft> {
    let raw = raw.trim();
    let draft: PrDraft = serde_json::from_str(raw).or_else(|_| {
        let embedded =
            extract_json_block(raw, '{', '}').context("Model reply contained no JSON object")?;
        serde_json::from_str(embedded).context("Model reply JSON did not match {title, body}")
    })?;

    let draft = PrDraft {
        title: draft.title.trim().to_string(),
        body: draft.body.trim().to_string(),
    };
    if draft.title.is_empty() || draft.body.is_empty() {
        anyhow::bail!("Model reply left the title or body empty");
    }
    Ok(draft)
}

/// Builds a serviceable pull request without AI: the template (or a summary
/// heading) plus the branch's commits.
fn fallback_draft(context: &PrContext) -> PrDraft {
    let title = if context.commits.len() == 1 {
        context.commits[0].clone()
    } else {
        humanize_branch(&context.branch)
    };

    let mut body = match &context.template {
        Some(template) => format!("{}\n\n", template.content.trim()),
        None => "## Summary\n\n".to_string(),
    };
    body.push_str("### Commits\n");
    for subject in &context.commits {
        body.push_str(&format!("- {subject}\n"));
    }

    PrDraft { title, body }
}

/// Turns `feat/add-stripe-webhooks` into `Add stripe webhooks`.
fn humanize_branch(branch: &str) -> String {
    let name = branch.rsplit('/').next().unwrap_or(branch);
    let mut words = name.split(['-', '_']).filter(|word| !word.is_empty());

    let mut title = String::new();
    if let Some(first) = words.next() {
        let mut chars = first.chars();
        if let Some(initial) = chars.next() {
            title.extend(initial.to_uppercase());
            title.push_str(chars.as_str());
        }
    }
    for word in words {
        title.push(' ');
        title.push_str(word);
    }

    if title.is_empty() {
        branch.to_string()
    } else {
        title
    }
}

fn render_preview(draft: &PrDraft) -> String {
    let mut preview = format!("\n  {}\n", draft.title.bold());
    preview.push_str(&format!("  {}\n", "─".repeat(40).dimmed()));
    for line in draft.body.lines() {
        preview.push_str(&format!("  {line}\n"));
    }
    preview.push('\n');
    preview
}

fn gh_pr_create(draft: &PrDraft, base: &str, as_draft: bool) -> Result<String> {
    let mut args = vec![
        "pr",
        "create",
        "--title",
        &draft.title,
        "--body",
        &draft.body,
        "--base",
        base,
    ];
    if as_draft {
        args.push("--draft");
    }
    gh(&args)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TempDir;
    use std::fs;

    fn write(path: &Path, contents: &str) {
        fs::create_dir_all(path.parent().expect("parent should exist"))
            .expect("directories should be created");
        fs::write(path, contents).expect("file should be written");
    }

    fn context(template: Option<Guidance>, commits: Vec<&str>) -> PrContext {
        PrContext {
            branch: "feat/add-webhooks".to_string(),
            base: "main".to_string(),
            commits: commits.into_iter().map(ToOwned::to_owned).collect(),
            diff: "diff --git a/src/api.rs b/src/api.rs".to_string(),
            template,
            skills: Vec::new(),
            title_examples: vec!["feat: previous change".to_string()],
        }
    }

    #[test]
    fn find_pr_template_matches_github_locations_case_insensitively() {
        let dir = TempDir::new("kite-pr-template");
        write(
            &dir.path.join(".github/PULL_REQUEST_TEMPLATE.md"),
            "## Summary\n",
        );

        let template = find_pr_template(&dir.path).expect("template should be found");
        assert_eq!(template.label, ".github/PULL_REQUEST_TEMPLATE.md");
        assert_eq!(template.content, "## Summary\n");
    }

    #[test]
    fn find_pr_template_falls_back_to_multi_template_directory() {
        let dir = TempDir::new("kite-pr-template-dir");
        write(
            &dir.path.join(".github/PULL_REQUEST_TEMPLATE/bugfix.md"),
            "## Bugfix\n",
        );
        write(
            &dir.path.join(".github/PULL_REQUEST_TEMPLATE/feature.md"),
            "## Feature\n",
        );

        let template = find_pr_template(&dir.path).expect("template should be found");
        assert!(template.label.ends_with("bugfix.md"));
        assert_eq!(template.content, "## Bugfix\n");
    }

    #[test]
    fn find_pr_template_returns_none_without_templates() {
        let dir = TempDir::new("kite-pr-no-template");
        assert!(find_pr_template(&dir.path).is_none());
    }

    #[test]
    fn find_pr_skills_picks_pull_request_skills_and_dedupes_by_name() {
        let dir = TempDir::new("kite-pr-skills");
        let project = dir.path.join(".claude/skills");
        let user = dir.path.join("home/.claude/skills");

        write(
            &project.join("write-prs/SKILL.md"),
            "---\nname: write-prs\ndescription: Guidance for pull requests.\n---\nAlways link issues.",
        );
        write(
            &user.join("write-prs/SKILL.md"),
            "Stale duplicate that must lose to the project copy.",
        );
        write(
            &user.join("unrelated/SKILL.md"),
            "---\nname: unrelated\ndescription: Formats SQL.\n---",
        );

        let skills = find_pr_skills_in(&[project, user]);

        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].label, "write-prs");
        assert!(skills[0].content.contains("Always link issues."));
    }

    #[test]
    fn mentions_pull_requests_checks_name_tokens_and_content() {
        assert!(mentions_pull_requests("write-prs", ""));
        assert!(mentions_pull_requests("pr-helper", ""));
        assert!(mentions_pull_requests(
            "shipit",
            "Use this when opening a Pull Request."
        ));
        assert!(mentions_pull_requests("shipit", "pull-request etiquette"));
        assert!(!mentions_pull_requests("sprint-notes", "Formats SQL."));
    }

    #[test]
    fn parse_draft_accepts_plain_and_fenced_json() {
        let plain = parse_draft(r#"{"title":"feat: add pr","body":"Adds `kt pr`."}"#)
            .expect("plain JSON should parse");
        assert_eq!(plain.title, "feat: add pr");

        let fenced = parse_draft("```json\n{\"title\":\"feat: add pr\",\"body\":\"Body.\"}\n```")
            .expect("fenced JSON should parse");
        assert_eq!(fenced.body, "Body.");
    }

    #[test]
    fn parse_draft_rejects_empty_fields() {
        let err =
            parse_draft(r#"{"title":"  ","body":"Body."}"#).expect_err("blank title should fail");
        assert!(format!("{err:#}").contains("title or body"));
    }

    #[test]
    fn fallback_draft_uses_single_commit_subject_as_title() {
        let draft = fallback_draft(&context(None, vec!["feat(api): add webhooks"]));

        assert_eq!(draft.title, "feat(api): add webhooks");
        assert!(draft.body.starts_with("## Summary"));
        assert!(draft.body.contains("- feat(api): add webhooks"));
    }

    #[test]
    fn fallback_draft_keeps_template_and_humanizes_branch_for_title() {
        let template = Guidance {
            label: ".github/pull_request_template.md".to_string(),
            content: "## Checklist\n- [ ] Tests\n".to_string(),
        };
        let draft = fallback_draft(&context(Some(template), vec!["feat: one", "fix: two"]));

        assert_eq!(draft.title, "Add webhooks");
        assert!(draft.body.starts_with("## Checklist"));
        assert!(draft.body.contains("- feat: one"));
        assert!(draft.body.contains("- fix: two"));
    }

    #[test]
    fn humanize_branch_strips_prefixes_and_separators() {
        assert_eq!(
            humanize_branch("feat/add-stripe-webhooks"),
            "Add stripe webhooks"
        );
        assert_eq!(humanize_branch("fix_login_bug"), "Fix login bug");
        assert_eq!(humanize_branch("main"), "Main");
    }

    #[test]
    fn build_pr_input_includes_all_gathered_context() {
        let mut ctx = context(
            Some(Guidance {
                label: ".github/pull_request_template.md".to_string(),
                content: "## Summary".to_string(),
            }),
            vec!["feat: add webhooks"],
        );
        ctx.skills.push(Guidance {
            label: "write-prs".to_string(),
            content: "Always link issues.".to_string(),
        });

        let input = build_pr_input(&ctx);

        assert!(input.contains("Branch: feat/add-webhooks"));
        assert!(input.contains("Recent pull request titles"));
        assert!(input.contains("follow its structure exactly:\n## Summary"));
        assert!(input.contains("Guidance from skill write-prs"));
        assert!(input.contains("- feat: add webhooks"));
        assert!(input.contains("Diff (may be truncated):"));
    }
}
