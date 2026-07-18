//! `kt pr` — open a GitHub pull request for the current branch with `gh`.
//!
//! The command gathers everything a good pull request needs — the branch's
//! commits and diff, the repository's PR template, recent PR titles for style,
//! and any PR-related agent skills installed on the machine — then asks the AI
//! for a title and body. Without AI it falls back to a
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
use crate::synth::{MAX_DIFF_BYTES, truncate_for_prompt};
use crate::ui::{Spinner, confirm, pluralize, print_ai_unavailable};

const MAX_COMMIT_SUBJECTS: usize = 50;
const MAX_PR_TITLE_EXAMPLES: usize = 8;
const MAX_SKILLS: usize = 3;
const MAX_SKILL_BYTES: usize = 4_000;
const MAX_TEMPLATE_BYTES: usize = 6_000;

const SYSTEM_PROMPT: &str = "\
You write pull requests for software teams. Using the branch's commits and diff, produce a pull request title and body.

Title rules:
1. Match the style of the recent pull request titles when they show a clear pattern; otherwise use Conventional Commit style.
2. Use the imperative, present tense. Keep it concise and specific. No trailing period.

Body rules:
1. If a template is provided, use it as the skeleton: fill each section with real content from the commits and diff, and drop instructional HTML comments.
2. Remove template sections that do not apply to this change or that you cannot fill with real content — no empty sections, no 'N/A', no untouched boilerplate. The final body contains only sections that say something.
3. Without a template, write a short summary paragraph followed by a bulleted list of notable changes.
4. Describe only changes that appear in the commits or diff. Never invent content or leave placeholders.
5. Use GitHub-flavored markdown.

Skill guidance, when provided, is the user's own instructions for how their pull requests must be written. Follow it — it takes precedence over the rules above wherever they disagree.

Update rule: when the current pull request is provided, you are refreshing it. Keep its structure and any human-written notes, and revise only what the commits and diff have made stale or incomplete. If nothing needs to change, return the current title and body verbatim.

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

/// The branch's already-open pull request, as reported by `gh pr view`.
struct ExistingPr {
    url: String,
    title: String,
    body: String,
    base: String,
}

struct PrContext {
    branch: String,
    base: String,
    commits: Vec<String>,
    diff: String,
    template: Option<Guidance>,
    skills: Vec<Guidance>,
}

pub(crate) async fn create_pull_request(options: PrOptions) -> Result<()> {
    ensure_gh_ready()?;
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

    if let Some(existing) = open_pr() {
        return refresh_pull_request(existing, branch, options.yes).await;
    }

    print_flow_header(&branch, &base);
    sync_branch_to_remote(&branch)?;

    let context = collect_pr_context(branch, base)?;
    announce_guidance(&context);

    // The style-example lookup hits the network, so it shares the spinner.
    let spinner = Spinner::start("Drafting");
    let title_examples = merged_pr_titles();
    let drafted = draft_with_ai(&context, &title_examples, None).await;
    spinner.stop();

    let draft = match drafted {
        Ok(draft) => draft,
        Err(error) => {
            print_ai_unavailable(&error);
            fallback_draft(&context)
        }
    };

    println!("{} Draft:", "·".cyan());
    print!("{}", render_preview(&draft));

    if !options.yes && !confirm("Create pull request?")? {
        println!("{} Aborted — no pull request created", "·".red());
        return Ok(());
    }

    let url = gh_pr_create(&draft, &context.base, options.draft)?;
    println!("{} {}", "✓".green(), url.trim());
    Ok(())
}

/// The branch already has an open pull request: push any new commits, then
/// check whether the body still describes the branch and offer a refreshed
/// one when it doesn't. Without AI the existing body is never touched.
async fn refresh_pull_request(
    existing: ExistingPr,
    branch: String,
    auto_confirm: bool,
) -> Result<()> {
    print_flow_header(&branch, &existing.base);
    println!("{} Already open: {}", "·".cyan(), existing.url);

    sync_branch_to_remote(&branch)?;

    let context = collect_pr_context(branch, existing.base.clone())?;
    announce_guidance(&context);

    let spinner = Spinner::start("Checking the body against the branch");
    let title_examples = merged_pr_titles();
    let drafted = draft_with_ai(&context, &title_examples, Some(&existing)).await;
    spinner.stop();

    let draft = match drafted {
        Ok(draft) => draft,
        Err(error) => {
            print_ai_unavailable(&error);
            println!("{} Leaving the pull request as is", "·".yellow());
            return Ok(());
        }
    };

    if drafts_match(&draft, &existing) {
        println!(
            "{} Body already reflects the branch — nothing to update",
            "✓".green()
        );
        return Ok(());
    }

    println!("{} Updated draft:", "·".cyan());
    print!("{}", render_preview(&draft));

    if !auto_confirm && !confirm("Update the pull request?")? {
        println!("{} Aborted — pull request left unchanged", "·".red());
        return Ok(());
    }

    gh(&["pr", "edit", "--title", &draft.title, "--body", &draft.body])?;
    println!("{} Updated {}", "✓".green(), existing.url);
    Ok(())
}

fn print_flow_header(branch: &str, base: &str) {
    println!(
        "{} {} {} {}",
        "·".cyan(),
        branch.bold(),
        "→".dimmed(),
        base.bold()
    );
}

/// Whitespace-insensitive comparison, so a model that only reflows the text
/// it was told to return verbatim doesn't trigger a pointless update.
fn drafts_match(draft: &PrDraft, existing: &ExistingPr) -> bool {
    let normalize = |text: &str| text.split_whitespace().collect::<Vec<_>>().join(" ");
    normalize(&draft.title) == normalize(&existing.title)
        && normalize(&draft.body) == normalize(&existing.body)
}

/// Checks for a working, authenticated `gh` up front — one offline spawn
/// (`gh auth token`), so a missing install or login fails in milliseconds
/// instead of after the whole draft has been built.
fn ensure_gh_ready() -> Result<()> {
    let status = Command::new("gh")
        .args(["auth", "token"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();

    match status {
        Err(_) => anyhow::bail!(
            "`kt pr` needs the GitHub CLI. Install it from https://cli.github.com, then run `gh auth login`."
        ),
        Ok(status) if !status.success() => {
            anyhow::bail!("GitHub CLI is not authenticated. Run `gh auth login` first.")
        }
        Ok(_) => Ok(()),
    }
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

/// Pushes the branch when the remote is missing it or out of date, so `gh pr
/// create` always sees the commits we are describing. Fetches first so the
/// comparison reflects the actual remote, not a stale tracking ref.
fn sync_branch_to_remote(branch: &str) -> Result<()> {
    let _ = execute_git(&["fetch", "origin", branch]); // branch may not exist remotely yet
    let remote_ref = format!("refs/remotes/origin/{branch}");

    if check_ref("HEAD") != check_ref(&remote_ref) {
        publish_current_branch()?;
    }
    Ok(())
}

/// The branch's open pull request, if any. `gh pr view` also resolves merged
/// and closed PRs, so filter to open ones — a reused branch must still be
/// able to get a fresh pull request.
fn open_pr() -> Option<ExistingPr> {
    let raw = gh(&["pr", "view", "--json", "url,title,body,state,baseRefName"]).ok()?;
    let json: serde_json::Value = serde_json::from_str(&raw).ok()?;

    if json["state"].as_str() != Some("OPEN") {
        return None;
    }

    Some(ExistingPr {
        url: json["url"].as_str()?.to_string(),
        title: json["title"].as_str()?.to_string(),
        body: json["body"].as_str().unwrap_or("").to_string(),
        base: json["baseRefName"].as_str()?.to_string(),
    })
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
    if let Some(home) = std::env::home_dir() {
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

/// A skill is PR-related when its name says so, or its frontmatter (falling
/// back to the opening lines) mentions pull requests. Matching the whole body
/// would drag in every skill that merely references a PR somewhere.
fn mentions_pull_requests(name: &str, content: &str) -> bool {
    let name_words: Vec<String> = name
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|word| !word.is_empty())
        .map(str::to_ascii_lowercase)
        .collect();
    let name_says_pr = name_words.iter().any(|word| word == "pr" || word == "prs")
        || name_words.join(" ").contains("pull request");

    let head = skill_frontmatter(content)
        .unwrap_or_else(|| content.lines().take(10).collect::<Vec<_>>().join("\n"))
        .to_ascii_lowercase();
    name_says_pr || head.contains("pull request") || head.contains("pull-request")
}

fn skill_frontmatter(content: &str) -> Option<String> {
    let rest = content.strip_prefix("---")?;
    let end = rest.find("\n---")?;
    Some(rest[..end].to_string())
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
    title_examples: &[String],
    existing: Option<&ExistingPr>,
) -> Result<PrDraft> {
    let user = build_pr_input(context, title_examples, existing);
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

fn build_pr_input(
    context: &PrContext,
    title_examples: &[String],
    existing: Option<&ExistingPr>,
) -> String {
    let mut input = format!(
        "Branch: {}\nBase branch: {}\n\n",
        context.branch, context.base
    );

    if let Some(pr) = existing {
        input.push_str(&format!(
            "Current pull request (already open — refresh it per the update rule):\nTitle: {}\nBody:\n{}\n\n",
            pr.title,
            truncate_for_prompt(&pr.body, MAX_TEMPLATE_BYTES)
        ));
    }

    if !title_examples.is_empty() {
        input.push_str("Recent pull request titles from this repository:\n");
        for title in title_examples {
            input.push_str(&format!("- {title}\n"));
        }
        input.push('\n');
    }

    for skill in &context.skills {
        input.push_str(&format!(
            "Skill guidance from `{}` — the user's instructions for writing this pull request:\n{}\n\n",
            skill.label, skill.content
        ));
    }

    if let Some(template) = &context.template {
        input.push_str(&format!(
            "Pull request template ({}) — fill it in, dropping sections that don't apply:\n{}\n\n",
            template.label, template.content
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
    use crate::test_support::{TempDir, write_file};

    fn context(template: Option<Guidance>, commits: Vec<&str>) -> PrContext {
        PrContext {
            branch: "feat/add-webhooks".to_string(),
            base: "main".to_string(),
            commits: commits.into_iter().map(ToOwned::to_owned).collect(),
            diff: "diff --git a/src/api.rs b/src/api.rs".to_string(),
            template,
            skills: Vec::new(),
        }
    }

    #[test]
    fn find_pr_template_matches_github_locations_case_insensitively() {
        let dir = TempDir::new("kite-pr-template");
        write_file(
            &dir.path,
            ".github/PULL_REQUEST_TEMPLATE.md",
            "## Summary\n",
        );

        let template = find_pr_template(&dir.path).expect("template should be found");
        assert_eq!(template.label, ".github/PULL_REQUEST_TEMPLATE.md");
        assert_eq!(template.content, "## Summary\n");
    }

    #[test]
    fn find_pr_template_falls_back_to_multi_template_directory() {
        let dir = TempDir::new("kite-pr-template-dir");
        write_file(
            &dir.path,
            ".github/PULL_REQUEST_TEMPLATE/bugfix.md",
            "## Bugfix\n",
        );
        write_file(
            &dir.path,
            ".github/PULL_REQUEST_TEMPLATE/feature.md",
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

        write_file(
            &dir.path,
            ".claude/skills/write-prs/SKILL.md",
            "---\nname: write-prs\ndescription: Guidance for pull requests.\n---\nAlways link issues.",
        );
        write_file(
            &dir.path,
            "home/.claude/skills/write-prs/SKILL.md",
            "Stale duplicate that must lose to the project copy.",
        );
        write_file(
            &dir.path,
            "home/.claude/skills/unrelated/SKILL.md",
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
        assert!(mentions_pull_requests("create-pull-request", ""));
        assert!(mentions_pull_requests("Pull_Requests", ""));
        assert!(mentions_pull_requests(
            "shipit",
            "Use this when opening a Pull Request."
        ));
        assert!(mentions_pull_requests("shipit", "pull-request etiquette"));
        assert!(!mentions_pull_requests("sprint-notes", "Formats SQL."));
        assert!(!mentions_pull_requests("prettier-config", ""));
    }

    #[test]
    fn mentions_pull_requests_ignores_body_only_mentions() {
        let body_only = "---\nname: shipit\ndescription: Release automation.\n---\nStep 9: also open a pull request.";
        assert!(!mentions_pull_requests("shipit", body_only));
    }

    #[test]
    fn drafts_match_ignores_whitespace_reflow_only() {
        let existing = ExistingPr {
            url: "https://example.com/pull/1".to_string(),
            title: "feat: add gadgets".to_string(),
            body: "## Summary\n\nAdds gadgets.".to_string(),
            base: "main".to_string(),
        };

        let reflowed = PrDraft {
            title: "feat:  add gadgets".to_string(),
            body: "## Summary\nAdds gadgets.".to_string(),
        };
        assert!(drafts_match(&reflowed, &existing));

        let revised = PrDraft {
            title: "feat: add gadgets".to_string(),
            body: "## Summary\n\nAdds gadgets and widgets.".to_string(),
        };
        assert!(!drafts_match(&revised, &existing));
    }

    #[test]
    fn build_pr_input_leads_with_the_existing_pull_request_when_refreshing() {
        let ctx = context(None, vec!["feat: add webhooks"]);
        let existing = ExistingPr {
            url: "https://example.com/pull/1".to_string(),
            title: "feat: old title".to_string(),
            body: "Old body.".to_string(),
            base: "main".to_string(),
        };

        let input = build_pr_input(&ctx, &[], Some(&existing));

        assert!(
            input.contains("Current pull request (already open — refresh it per the update rule):")
        );
        assert!(input.contains("Title: feat: old title"));
        assert!(input.contains("Old body."));
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

        let input = build_pr_input(&ctx, &["feat: previous change".to_string()], None);

        assert!(input.contains("Branch: feat/add-webhooks"));
        assert!(input.contains("Recent pull request titles"));
        assert!(input.contains("dropping sections that don't apply:\n## Summary"));
        assert!(input.contains("Skill guidance from `write-prs`"));

        let skill_index = input
            .find("Skill guidance from")
            .expect("skill section should exist");
        let template_index = input
            .find("Pull request template")
            .expect("template section should exist");
        assert!(
            skill_index < template_index,
            "the user's skill guidance should lead the prompt"
        );
        assert!(input.contains("- feat: add webhooks"));
        assert!(input.contains("Diff (may be truncated):"));
    }
}
