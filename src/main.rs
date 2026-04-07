use anyhow::{Context, Result};
use chrono::Local;
use clap::{Parser, Subcommand};
use colored::*;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::env;
use std::io::{self, Write};
use std::process::{Command, Stdio};
use std::time::Duration;

const MAX_COMMIT_FAILURE_LINES: usize = 12;
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

Return ONLY a valid JSON array of objects. Absolutely no markdown or conversational text.
Schema: [ { \"message\": \"feat(auth): implement JWT validation\", \"files\": [\"src/auth.rs\"] } ]";

#[derive(Parser)]
#[command(
    name = "kt",
    about = "Fast quicksaves and inspectable AI-assisted landing",
    version
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// Start a new flow
    Go { name: String },
    /// Intelligently chunk Kite saves into local commits
    Land {
        /// Publish the rewritten branch after landing
        #[arg(long)]
        push: bool,
        /// Skip the confirmation prompt
        #[arg(short = 'y', long)]
        yes: bool,
    },
    /// Publish the current branch after reviewing local history
    Publish,
    /// Instantly revert the last land operation
    Undo,
}

#[derive(Serialize, Deserialize, Debug)]
struct CommitGroup {
    message: String,
    files: Vec<String>,
}

#[derive(Deserialize)]
struct CommitGroupsEnvelope {
    groups: Vec<CommitGroup>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum KiteBase {
    Root,
    Commit(String),
}

#[derive(Clone, Debug)]
struct LandScope {
    base: KiteBase,
    diff: String,
    actual_files: HashSet<String>,
}

#[derive(Clone, Debug)]
struct ProviderFailure {
    provider: &'static str,
    error: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    match &cli.command {
        Some(Commands::Go { name }) => go(name),
        Some(Commands::Land { push, yes }) => land(*push, *yes).await,
        Some(Commands::Publish) => publish_current_branch(),
        Some(Commands::Undo) => undo(),
        None => save(),
    }
}

// ==========================================
// THE QUICKSAVE
// ==========================================
fn save() -> Result<()> {
    let status = execute_git(&["status", "--porcelain"])?;
    if status.trim().is_empty() {
        return Ok(());
    }

    if !has_staged_changes(&status) {
        execute_git(&["add", "-A"])?;
    }
    let msg = format!("[kite] save {}", Local::now().format("%H:%M:%S"));
    execute_git(&["commit", "-m", &msg, "--no-verify"])?;

    println!("{} {}", "·".dimmed(), "saved".dimmed());
    Ok(())
}

fn go(name: &str) -> Result<()> {
    let default_branch = get_default_branch()?;

    if has_remote() {
        let _ = execute_git(&["fetch", "origin", &default_branch]);
        execute_git(&[
            "checkout",
            "-b",
            name,
            &format!("origin/{}", default_branch),
        ])
        .or_else(|_| execute_git(&["checkout", "-b", name, &default_branch]))?;
    } else {
        execute_git(&["checkout", "-b", name, &default_branch])?;
    }

    println!("{} Flow started: {}", "·".cyan(), name.bold());
    Ok(())
}

// ==========================================
// SYNTHESIS & LANDING
// ==========================================
async fn land(push: bool, auto_confirm: bool) -> Result<()> {
    let Some(scope) = collect_land_scope()? else {
        return Ok(());
    };

    print!("{} Synthesizing... ", "·".cyan());
    io::stdout().flush()?;

    let (raw_groups, provider_label) = match synthesize_groups(&scope.diff).await {
        Ok((groups, provider_label)) => {
            println!("({provider_label})");
            (groups, provider_label)
        }
        Err(failures) => {
            println!("{}", "unavailable".dimmed());
            let Some(message) = prompt_manual_commit_message(&failures)? else {
                println!("{} Aborted. No history changed.", "·".red());
                return Ok(());
            };
            (
                vec![CommitGroup {
                    message,
                    files: sorted_files(&scope.actual_files),
                }],
                "manual",
            )
        }
    };

    let groups = normalize_groups(raw_groups, &scope.actual_files);
    if groups.is_empty() {
        anyhow::bail!("No changed files were assigned to landed commit groups.");
    }

    println!();
    print_land_plan(&groups, provider_label);

    if !auto_confirm && !confirm_land(push)? {
        println!("{} Aborted. No history changed.", "·".red());
        return Ok(());
    }

    execute_land(&scope.base, &groups)?;

    if push {
        publish_current_branch()?;
        println!("{}\n", render_tree_tail("Landed and published").green());
    } else {
        if has_remote() {
            let current_branch = get_current_branch()?;
            println!(
                "{} Review the rewritten history, then run `kt publish` or `git push --force-with-lease origin {}`.",
                "·".dimmed(),
                current_branch
            );
        }
        println!("{}\n", render_tree_tail("Landed locally").green());
    }

    Ok(())
}

fn collect_land_scope() -> Result<Option<LandScope>> {
    if !has_head_commit() {
        println!(
            "{} Repository has no commits yet. Create an initial commit before running `kt land`.",
            "·".yellow()
        );
        return Ok(None);
    }

    let status = execute_git(&["status", "--porcelain"])?;
    if !status.trim().is_empty() {
        anyhow::bail!(
            "Working directory must be clean before `kt land`. Run `kt` to snapshot current work or stash unrelated changes first."
        );
    }

    let Some(base) = get_kite_base()? else {
        println!(
            "{} Nothing to land. Create one or more contiguous `[kite] save` commits first.",
            "·".dimmed()
        );
        return Ok(None);
    };

    let diff = diff_for_base(&base)?;
    let actual_files = changed_files_for_base(&base)?;

    if actual_files.is_empty() {
        println!(
            "{} Nothing to land. No file changes were found.",
            "·".dimmed()
        );
        return Ok(None);
    }

    Ok(Some(LandScope {
        base,
        diff,
        actual_files,
    }))
}

async fn synthesize_groups(
    diff: &str,
) -> std::result::Result<(Vec<CommitGroup>, &'static str), Vec<ProviderFailure>> {
    match try_local_ollama(diff).await {
        Ok(groups) => return Ok((groups, "local")),
        Err(local_error) => match try_openai(diff).await {
            Ok(groups) => return Ok((groups, "cloud")),
            Err(openai_error) => {
                return Err(vec![
                    ProviderFailure {
                        provider: "local",
                        error: format!("{local_error:#}"),
                    },
                    ProviderFailure {
                        provider: "cloud",
                        error: format!("{openai_error:#}"),
                    },
                ]);
            }
        },
    }
}

fn normalize_groups(groups: Vec<CommitGroup>, actual_files: &HashSet<String>) -> Vec<CommitGroup> {
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

fn print_land_plan(groups: &[CommitGroup], provider_label: &str) {
    println!(
        "{} Proposed history using {} synthesis:",
        "·".cyan(),
        provider_label
    );

    for group in groups {
        println!(
            "{}",
            render_tree_line(
                &format!("{}", "│".dimmed()),
                &format!(
                    "{} ({})",
                    group.message,
                    pluralize(group.files.len(), "file")
                ),
            )
        );
        for file in &group.files {
            println!(
                "{}",
                render_tree_line(&format!("{} {}", "│".dimmed(), "├─".dimmed()), file)
            );
        }
    }
}

fn confirm_land(push: bool) -> Result<bool> {
    let action = if push {
        "rewrite local history and publish it"
    } else {
        "rewrite local history"
    };

    print!("{} Proceed and {}? [y/N]: ", "·".cyan(), action);
    io::stdout().flush()?;

    let mut response = String::new();
    io::stdin().read_line(&mut response)?;

    Ok(matches!(response.trim(), "y" | "Y" | "yes" | "YES"))
}

fn prompt_manual_commit_message(failures: &[ProviderFailure]) -> Result<Option<String>> {
    println!();
    println!("{} Automatic synthesis was unavailable:", "·".yellow());

    for failure in failures {
        println!(
            "{}",
            render_tree_line(
                &format!("{}", "│".dimmed()),
                &format!("{}: {}", failure.provider, flatten_error(&failure.error)),
            )
        );
    }

    print!(
        "{} Single commit message (leave blank to abort): ",
        "·".cyan()
    );
    io::stdout().flush()?;

    let mut msg = String::new();
    io::stdin().read_line(&mut msg)?;

    let msg = msg.trim();
    if msg.is_empty() {
        return Ok(None);
    }

    Ok(Some(msg.to_string()))
}

fn execute_land(base: &KiteBase, groups: &[CommitGroup]) -> Result<()> {
    let original_branch = get_current_branch()?;
    let pre_land_sha = execute_git(&["rev-parse", "HEAD"])?;
    let temp_branch = format!("kite-land-{}", Local::now().format("%Y%m%d%H%M%S"));

    execute_git(&["update-ref", "refs/kite/pre_land", pre_land_sha.trim()])?;

    if let Err(err) = prepare_landing_branch(base, &temp_branch).and_then(|_| {
        commit_groups(groups)?;
        finalize_landed_branch(&original_branch, &temp_branch)
    }) {
        anyhow::bail!(
            "{}\n\nOriginal branch `{}` still points at the pre-land history. Temporary branch `{}` contains the in-progress landing state. Recovery ref: `refs/kite/pre_land`.",
            err,
            original_branch,
            temp_branch
        );
    }

    Ok(())
}

fn prepare_landing_branch(base: &KiteBase, temp_branch: &str) -> Result<()> {
    match base {
        KiteBase::Commit(base_sha) => {
            execute_git(&["checkout", "-b", temp_branch])?;
            execute_git(&["reset", "--soft", base_sha])?;
            execute_git(&["reset"])?;
        }
        KiteBase::Root => {
            execute_git(&["checkout", "--orphan", temp_branch, "HEAD"])?;
            execute_git(&["read-tree", "--empty"])?;
        }
    }

    Ok(())
}

fn commit_groups(groups: &[CommitGroup]) -> Result<()> {
    for group in groups {
        for file in &group.files {
            execute_git(&["add", "--", file])?;
        }
        commit_git(&group.message)?;
        println!(
            "{}",
            render_tree_line(&format!("{}", "│".dimmed()), &group.message)
        );
    }

    Ok(())
}

fn finalize_landed_branch(original_branch: &str, temp_branch: &str) -> Result<()> {
    let new_head = execute_git(&["rev-parse", "HEAD"])?;
    execute_git(&["branch", "-f", original_branch, new_head.trim()])?;
    execute_git(&["checkout", original_branch])?;
    execute_git(&["branch", "-D", temp_branch])?;
    Ok(())
}

fn publish_current_branch() -> Result<()> {
    if !has_remote() {
        println!(
            "{} No remote configured. Landed history is local only.",
            "·".dimmed()
        );
        return Ok(());
    }

    let current_branch = get_current_branch()?;

    print!(
        "{} ",
        render_tree_line(&format!("{}", "│".dimmed()), "Pulling remote changes...")
    );
    io::stdout().flush()?;
    match execute_git(&["pull", "--rebase", "origin", &current_branch]) {
        Ok(_) => println!("Done"),
        Err(_) => println!("{}", "Skipped (no upstream or nothing to pull)".dimmed()),
    }

    print!(
        "{} ",
        render_tree_line(&format!("{}", "│".dimmed()), "Publishing to remote...")
    );
    io::stdout().flush()?;

    match execute_git(&[
        "push",
        "--set-upstream",
        "origin",
        &current_branch,
        "--force-with-lease",
    ]) {
        Ok(_) => println!("Done"),
        Err(_) => println!("{}", "Failed (You may need to push manually)".yellow()),
    }

    Ok(())
}

// ==========================================
// THE ESCAPE HATCH (UNDO)
// ==========================================
fn undo() -> Result<()> {
    let pre_land_sha = match check_ref("refs/kite/pre_land") {
        Some(sha) => sha,
        None => {
            println!(
                "{} Nothing to undo. No previous land operation found.",
                "·".yellow()
            );
            return Ok(());
        }
    };

    // Safety check: ensure the working directory is clean so we don't nuke post-land work
    let status = execute_git(&["status", "--porcelain"])?;
    if !status.trim().is_empty() {
        anyhow::bail!(
            "Working directory is not clean. Please `kt save` or stash your changes before undoing."
        );
    }

    print!("{} Rewinding timeline... ", "·".cyan());
    io::stdout().flush()?;

    // Instantly teleport back to the exact messy saves
    execute_git(&["reset", "--hard", &pre_land_sha])?;

    // Clear the marker so we don't accidentally double-undo later
    execute_git(&["update-ref", "-d", "refs/kite/pre_land"])?;
    println!("Done");

    if has_remote() {
        let current_branch = get_current_branch()?;
        print!("{} Reverting remote... ", "·".cyan());
        io::stdout().flush()?;

        // Force push the messy state back up to synchronize the remote
        match Command::new("git")
            .args(["push", "--force-with-lease", "origin", &current_branch])
            .stderr(Stdio::null())
            .stdout(Stdio::null())
            .status()
        {
            Ok(status) if status.success() => println!("Done"),
            _ => println!("{}", "Failed (Rmote may have diverged)".yellow()),
        }
    }

    println!("  {}\n", "└─ Restored previous saves".green());
    Ok(())
}

// ==========================================
// AI PROVIDERS
// ==========================================
async fn try_local_ollama(diff: &str) -> Result<Vec<CommitGroup>> {
    let prompt = build_synthesis_input(diff, 15_000)?;
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

    parse_json(content)
}

async fn try_openai(diff: &str) -> Result<Vec<CommitGroup>> {
    let (base_url, model, api_key) = get_openai_env_config()?;
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(30))
        .build()?;

    let prompt = format!(
        "{}\nFor this provider, return an object exactly like: {{ \"groups\": [{{\"message\":\"...\",\"files\":[\"...\"]}}] }}\n\n{}",
        SYSTEM_PROMPT,
        build_synthesis_input(diff, 20_000)?
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
    parse_openai_groups(&json)
}

fn build_synthesis_input(diff: &str, max_diff_bytes: usize) -> Result<String> {
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

    prompt.push_str("Diff:\n");
    prompt.push_str(truncate_for_prompt(diff, max_diff_bytes));
    Ok(prompt)
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

// ==========================================
// GIT HELPERS
// ==========================================
fn execute_git(args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .args(args)
        .output()
        .with_context(|| format!("Failed 'git {}'", args.join(" ")))?;

    if !output.status.success() {
        anyhow::bail!(
            "Git error: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

fn has_staged_changes(status: &str) -> bool {
    status.lines().any(|line| {
        line.chars()
            .next()
            .is_some_and(|status_code| status_code != ' ' && status_code != '?')
    })
}

fn commit_git(message: &str) -> Result<()> {
    let output = Command::new("git")
        .args(["commit", "-m", message])
        .stdin(Stdio::inherit())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("Failed 'git commit -m {}'", message))?;

    let output = output
        .wait_with_output()
        .with_context(|| format!("Failed while waiting on 'git commit -m {}'", message))?;

    if !output.status.success() {
        let rendered_output = compact_command_output(
            &String::from_utf8_lossy(&output.stdout),
            &String::from_utf8_lossy(&output.stderr),
        );
        anyhow::bail!("{}", render_commit_failure(message, &rendered_output));
    }

    Ok(())
}

fn compact_command_output(stdout: &str, stderr: &str) -> String {
    let lines: Vec<String> = stderr
        .lines()
        .chain(stdout.lines())
        .map(str::trim_end)
        .filter(|line| !line.trim().is_empty())
        .map(ToOwned::to_owned)
        .collect();

    if lines.is_empty() {
        return String::new();
    }

    let visible_lines = if lines.len() > MAX_COMMIT_FAILURE_LINES {
        let omitted = lines.len() - MAX_COMMIT_FAILURE_LINES;
        let mut trimmed = vec![format!("... {} earlier line(s) omitted", omitted)];
        trimmed.extend(
            lines[lines.len() - MAX_COMMIT_FAILURE_LINES..]
                .iter()
                .cloned(),
        );
        trimmed
    } else {
        lines
    };

    visible_lines.join("\n")
}

fn render_commit_failure(message: &str, details: &str) -> String {
    let details_lower = details.to_ascii_lowercase();
    let summary = if ["hook", "pre-commit", "commit-msg", "pre-push"]
        .iter()
        .any(|marker| details_lower.contains(marker))
    {
        "Git hook blocked the commit"
    } else {
        "Git rejected the commit"
    };

    if details.is_empty() {
        return format!(
            "{} for `{}`. Staged changes were left in place.",
            summary, message
        );
    }

    format!(
        "{} for `{}`. Staged changes were left in place.\n\n{}",
        summary,
        message,
        indent_block(details)
    )
}

fn indent_block(text: &str) -> String {
    text.lines()
        .map(|line| format!("  {}", line))
        .collect::<Vec<_>>()
        .join("\n")
}

fn render_tree_line(prefix: &str, message: &str) -> String {
    format!("  {} {}", prefix, message)
}

fn render_tree_tail(message: &str) -> String {
    format!("  └─ {}", message)
}

fn get_default_branch() -> Result<String> {
    if has_remote() {
        if let Ok(output) = execute_git(&[
            "symbolic-ref",
            "--quiet",
            "--short",
            "refs/remotes/origin/HEAD",
        ]) {
            if let Some(branch) = output.trim().rsplit('/').next() {
                if !branch.is_empty() {
                    return Ok(branch.to_string());
                }
            }
        }
    }

    let output = execute_git(&["branch", "--list", "main", "master"])?;
    if output.contains("main") {
        return Ok("main".to_string());
    }
    if output.contains("master") {
        return Ok("master".to_string());
    }

    let current = execute_git(&["rev-parse", "--abbrev-ref", "HEAD"])?;
    let current = current.trim();
    if !current.is_empty() && current != "HEAD" {
        Ok(current.to_string())
    } else {
        anyhow::bail!(
            "Could not determine a default branch. Expected origin/HEAD, `main`, or `master`."
        )
    }
}

fn has_head_commit() -> bool {
    Command::new("git")
        .args(["rev-parse", "--verify", "HEAD"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

fn get_current_branch() -> Result<String> {
    let output = execute_git(&["rev-parse", "--abbrev-ref", "HEAD"])?;
    Ok(output.trim().to_string())
}

fn has_remote() -> bool {
    Command::new("git")
        .args(["remote"])
        .output()
        .map(|o| !String::from_utf8_lossy(&o.stdout).trim().is_empty())
        .unwrap_or(false)
}

fn check_ref(ref_name: &str) -> Option<String> {
    let output = Command::new("git")
        .args(["rev-parse", "--verify", ref_name])
        .stderr(Stdio::null())
        .output()
        .ok()?;

    if output.status.success() {
        Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
    } else {
        None
    }
}

fn diff_for_base(base: &KiteBase) -> Result<String> {
    match base {
        KiteBase::Commit(hash) => {
            let range = format!("{hash}..HEAD");
            execute_git(&["diff", &range])
        }
        KiteBase::Root => {
            execute_git(&["diff-tree", "--root", "--no-commit-id", "-r", "-p", "HEAD"])
        }
    }
}

fn changed_files_for_base(base: &KiteBase) -> Result<HashSet<String>> {
    let output = match base {
        KiteBase::Commit(hash) => {
            let range = format!("{hash}..HEAD");
            execute_git(&["diff", "--name-only", &range])?
        }
        KiteBase::Root => execute_git(&[
            "diff-tree",
            "--root",
            "--no-commit-id",
            "-r",
            "--name-only",
            "HEAD",
        ])?,
    };

    Ok(output
        .lines()
        .map(|line| line.trim().to_string())
        .filter(|line| !line.is_empty())
        .collect())
}

fn recent_commit_style_examples(limit: usize) -> Result<Vec<String>> {
    let output = match execute_git(&["log", "--format=%s", "-n", "30"]) {
        Ok(output) => output,
        Err(_) => return Ok(Vec::new()),
    };

    let mut seen = HashSet::new();
    let mut examples = Vec::new();

    for line in output.lines() {
        let message = line.trim();
        if message.is_empty() || message.starts_with("[kite] save") || message.starts_with("Merge ")
        {
            continue;
        }

        let owned = message.to_string();
        if seen.insert(owned.clone()) {
            examples.push(owned);
        }

        if examples.len() >= limit {
            break;
        }
    }

    Ok(examples)
}

fn sorted_files(files: &HashSet<String>) -> Vec<String> {
    let mut files: Vec<String> = files.iter().cloned().collect();
    files.sort();
    files
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

fn flatten_error(error: &str) -> String {
    error
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join(" | ")
}

fn pluralize(count: usize, singular: &str) -> String {
    if count == 1 {
        format!("1 {singular}")
    } else {
        format!("{count} {singular}s")
    }
}

fn get_kite_base() -> Result<Option<KiteBase>> {
    let log_output = match execute_git(&["log", "--format=%H %s"]) {
        Ok(out) => out,
        Err(_) => return Ok(None),
    };

    let mut save_count = 0;
    let mut base_hash = None;

    for line in log_output.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        let Some((hash, msg)) = line.split_once(' ') else {
            continue;
        };

        if msg.starts_with("[kite] save") {
            save_count += 1;
        } else {
            base_hash = Some(hash.to_string());
            break;
        }
    }

    if save_count > 0 {
        Ok(Some(match base_hash {
            Some(hash) => KiteBase::Commit(hash),
            None => KiteBase::Root,
        }))
    } else {
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::{Mutex, OnceLock};
    use std::time::{SystemTime, UNIX_EPOCH};

    fn assert_single_group(groups: Vec<CommitGroup>, message: &str, file: &str) {
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].message, message);
        assert_eq!(groups[0].files, vec![file.to_string()]);
    }

    fn cwd_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    fn acquire_cwd_lock() -> std::sync::MutexGuard<'static, ()> {
        cwd_lock()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    struct TempRepo {
        path: PathBuf,
    }

    impl TempRepo {
        fn new() -> Self {
            let unique = format!(
                "kite-test-{}-{}",
                std::process::id(),
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .expect("system time should be after unix epoch")
                    .as_nanos()
            );
            let path = std::env::temp_dir().join(unique);
            fs::create_dir_all(&path).expect("temp repo directory should be created");
            Self { path }
        }
    }

    impl Drop for TempRepo {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    fn git(repo: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .args(args)
            .current_dir(repo)
            .output()
            .expect("git command should run");

        assert!(
            output.status.success(),
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );

        String::from_utf8_lossy(&output.stdout).to_string()
    }

    fn write_file(repo: &Path, path: &str, contents: &str) {
        fs::write(repo.join(path), contents).expect("test file should be written");
    }

    fn with_repo_cwd<T>(repo: &Path, f: impl FnOnce() -> T) -> T {
        let original_dir = std::env::current_dir().expect("current dir should resolve");
        std::env::set_current_dir(repo).expect("should enter temp repo");
        let result = f();
        std::env::set_current_dir(&original_dir).expect("should restore original cwd");
        result
    }

    fn run_save_in_repo(repo: &Path) -> Result<()> {
        with_repo_cwd(repo, save)
    }

    fn collect_land_scope_in_repo(repo: &Path) -> Result<Option<LandScope>> {
        with_repo_cwd(repo, collect_land_scope)
    }

    fn execute_land_in_repo(repo: &Path, base: &KiteBase, groups: &[CommitGroup]) -> Result<()> {
        with_repo_cwd(repo, || execute_land(base, groups))
    }

    fn undo_in_repo(repo: &Path) -> Result<()> {
        with_repo_cwd(repo, undo)
    }

    fn init_repo() -> TempRepo {
        let repo = TempRepo::new();
        git(&repo.path, &["init"]);
        git(&repo.path, &["config", "user.name", "Kite Test"]);
        git(&repo.path, &["config", "user.email", "kite@example.com"]);

        write_file(&repo.path, "tracked.txt", "base\n");
        write_file(&repo.path, "other.txt", "base\n");
        git(&repo.path, &["add", "tracked.txt", "other.txt"]);
        git(&repo.path, &["commit", "-m", "chore: initial"]);

        repo
    }

    fn init_root_kite_repo() -> TempRepo {
        let repo = TempRepo::new();
        git(&repo.path, &["init"]);
        git(&repo.path, &["config", "user.name", "Kite Test"]);
        git(&repo.path, &["config", "user.email", "kite@example.com"]);

        write_file(&repo.path, "tracked.txt", "base\n");
        git(&repo.path, &["add", "tracked.txt"]);
        git(&repo.path, &["commit", "-m", "[kite] save 12:00:00"]);

        repo
    }

    #[test]
    fn compact_command_output_keeps_last_lines_and_truncates_noise() {
        let stderr = (1..=14)
            .map(|line| format!("stderr line {}", line))
            .collect::<Vec<_>>()
            .join("\n");

        let compacted = compact_command_output("", &stderr);

        assert!(compacted.contains("... 2 earlier line(s) omitted"));
        assert!(compacted.contains("stderr line 14"));
        assert!(!compacted.contains("stderr line 1\n"));
    }

    #[test]
    fn render_commit_failure_marks_hook_rejections() {
        let rendered = render_commit_failure(
            "feat(cli): tighten hooks",
            "pre-commit: cargo fmt --check failed",
        );

        assert!(rendered.contains("Git hook blocked the commit"));
        assert!(rendered.contains("Staged changes were left in place."));
        assert!(rendered.contains("  pre-commit: cargo fmt --check failed"));
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
    fn render_tree_lines_match_land_summary_layout() {
        assert_eq!(
            render_tree_line("│", "Publishing to remote... Done"),
            "  │ Publishing to remote... Done"
        );
        assert_eq!(render_tree_tail("Landed"), "  └─ Landed");
    }

    #[test]
    fn has_staged_changes_detects_index_entries() {
        assert!(has_staged_changes("M  src/main.rs\n"));
        assert!(has_staged_changes("A  new.rs\n"));
        assert!(has_staged_changes("MM src/main.rs\n"));
    }

    #[test]
    fn has_staged_changes_ignores_only_unstaged_and_untracked_entries() {
        assert!(!has_staged_changes(" M src/main.rs\n"));
        assert!(!has_staged_changes("?? scratch.txt\n"));
        assert!(!has_staged_changes(" M src/main.rs\n?? scratch.txt\n"));
    }

    #[test]
    fn save_commits_only_pre_staged_changes() {
        let _lock = acquire_cwd_lock();
        let repo = init_repo();

        write_file(&repo.path, "tracked.txt", "staged change\n");
        write_file(&repo.path, "other.txt", "left unstaged\n");
        git(&repo.path, &["add", "tracked.txt"]);

        run_save_in_repo(&repo.path).expect("save should succeed");

        let saved_files = git(
            &repo.path,
            &["show", "--name-only", "--pretty=format:", "HEAD"],
        );
        assert_eq!(saved_files.trim(), "tracked.txt");

        let status = git(&repo.path, &["status", "--porcelain"]);
        assert!(status.contains(" M other.txt"));
    }

    #[test]
    fn save_stages_everything_when_index_is_empty() {
        let _lock = acquire_cwd_lock();
        let repo = init_repo();

        write_file(&repo.path, "tracked.txt", "modified without staging\n");
        write_file(&repo.path, "new.txt", "brand new file\n");

        run_save_in_repo(&repo.path).expect("save should succeed");

        let saved_files = git(
            &repo.path,
            &["show", "--name-only", "--pretty=format:", "HEAD"],
        );
        assert!(saved_files.lines().any(|line| line == "tracked.txt"));
        assert!(saved_files.lines().any(|line| line == "new.txt"));

        let status = git(&repo.path, &["status", "--porcelain"]);
        assert!(
            status.trim().is_empty(),
            "expected clean status, got: {status}"
        );
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

    #[test]
    fn collect_land_scope_rejects_dirty_worktree() {
        let _lock = acquire_cwd_lock();
        let repo = init_repo();

        write_file(&repo.path, "tracked.txt", "dirty\n");

        let err = collect_land_scope_in_repo(&repo.path).expect_err("dirty repos should fail");
        assert!(format!("{err:#}").contains("Working directory must be clean"));
    }

    #[test]
    fn execute_land_records_pre_land_ref_and_rewrites_non_root_history() {
        let _lock = acquire_cwd_lock();
        let repo = init_repo();

        write_file(&repo.path, "tracked.txt", "saved change\n");
        git(&repo.path, &["add", "tracked.txt"]);
        git(&repo.path, &["commit", "-m", "[kite] save 12:00:00"]);

        let original_branch = git(&repo.path, &["rev-parse", "--abbrev-ref", "HEAD"]);
        let original_branch = original_branch.trim().to_string();
        let pre_land_sha = git(&repo.path, &["rev-parse", "HEAD"]);
        let pre_land_sha = pre_land_sha.trim().to_string();

        let scope = collect_land_scope_in_repo(&repo.path)
            .expect("land scope should collect")
            .expect("kites saves should be landable");
        assert!(matches!(scope.base, KiteBase::Commit(_)));

        execute_land_in_repo(
            &repo.path,
            &scope.base,
            &[CommitGroup {
                message: "feat: land tracked change".to_string(),
                files: vec!["tracked.txt".to_string()],
            }],
        )
        .expect("land should succeed");

        let head_message = git(&repo.path, &["log", "-1", "--pretty=%s"]);
        assert_eq!(head_message.trim(), "feat: land tracked change");

        let recorded_pre_land = git(&repo.path, &["rev-parse", "refs/kite/pre_land"]);
        assert_eq!(recorded_pre_land.trim(), pre_land_sha);

        let current_branch = git(&repo.path, &["rev-parse", "--abbrev-ref", "HEAD"]);
        assert_eq!(current_branch.trim(), original_branch);

        let temp_branches = git(&repo.path, &["branch", "--list", "kite-land-*"]);
        assert!(temp_branches.trim().is_empty());
    }

    #[test]
    fn undo_restores_the_previous_kite_saves_after_land() {
        let _lock = acquire_cwd_lock();
        let repo = init_repo();

        write_file(&repo.path, "tracked.txt", "saved change\n");
        git(&repo.path, &["add", "tracked.txt"]);
        git(&repo.path, &["commit", "-m", "[kite] save 12:00:00"]);

        let pre_land_sha = git(&repo.path, &["rev-parse", "HEAD"]);
        let pre_land_sha = pre_land_sha.trim().to_string();

        let scope = collect_land_scope_in_repo(&repo.path)
            .expect("land scope should collect")
            .expect("kites saves should be landable");

        execute_land_in_repo(
            &repo.path,
            &scope.base,
            &[CommitGroup {
                message: "feat: land tracked change".to_string(),
                files: vec!["tracked.txt".to_string()],
            }],
        )
        .expect("land should succeed");

        undo_in_repo(&repo.path).expect("undo should succeed");

        let restored_head = git(&repo.path, &["rev-parse", "HEAD"]);
        assert_eq!(restored_head.trim(), pre_land_sha);

        let status = git(&repo.path, &["status", "--porcelain"]);
        assert!(status.trim().is_empty());
    }

    #[test]
    fn execute_land_supports_root_only_kite_history() {
        let _lock = acquire_cwd_lock();
        let repo = init_root_kite_repo();

        let original_branch = git(&repo.path, &["rev-parse", "--abbrev-ref", "HEAD"]);
        let original_branch = original_branch.trim().to_string();
        let pre_land_sha = git(&repo.path, &["rev-parse", "HEAD"]);
        let pre_land_sha = pre_land_sha.trim().to_string();

        let scope = collect_land_scope_in_repo(&repo.path)
            .expect("land scope should collect")
            .expect("root kite save should be landable");
        assert!(matches!(scope.base, KiteBase::Root));

        execute_land_in_repo(
            &repo.path,
            &scope.base,
            &[CommitGroup {
                message: "feat: bootstrap project".to_string(),
                files: vec!["tracked.txt".to_string()],
            }],
        )
        .expect("root land should succeed");

        let head_message = git(&repo.path, &["log", "-1", "--pretty=%s"]);
        assert_eq!(head_message.trim(), "feat: bootstrap project");

        let recorded_pre_land = git(&repo.path, &["rev-parse", "refs/kite/pre_land"]);
        assert_eq!(recorded_pre_land.trim(), pre_land_sha);

        let current_branch = git(&repo.path, &["rev-parse", "--abbrev-ref", "HEAD"]);
        assert_eq!(current_branch.trim(), original_branch);
    }

    #[test]
    fn recent_commit_style_examples_skip_kite_saves_and_deduplicate() {
        let _lock = acquire_cwd_lock();
        let repo = init_repo();

        write_file(&repo.path, "tracked.txt", "first\n");
        git(&repo.path, &["add", "tracked.txt"]);
        git(&repo.path, &["commit", "-m", "fix(cli): tighten landing"]);

        write_file(&repo.path, "tracked.txt", "second\n");
        git(&repo.path, &["add", "tracked.txt"]);
        git(&repo.path, &["commit", "-m", "[kite] save 12:00:00"]);

        write_file(&repo.path, "tracked.txt", "third\n");
        git(&repo.path, &["add", "tracked.txt"]);
        git(&repo.path, &["commit", "-m", "fix(cli): tighten landing"]);

        write_file(&repo.path, "tracked.txt", "fourth\n");
        git(&repo.path, &["add", "tracked.txt"]);
        git(&repo.path, &["commit", "-m", "docs: refresh usage"]);

        let examples = with_repo_cwd(&repo.path, || {
            recent_commit_style_examples(MAX_COMMIT_STYLE_EXAMPLES)
        })
        .expect("examples should load");

        assert_eq!(
            examples,
            vec![
                "docs: refresh usage".to_string(),
                "fix(cli): tighten landing".to_string(),
                "chore: initial".to_string()
            ]
        );
    }
}
