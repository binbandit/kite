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

const SYSTEM_PROMPT: &str = "\
You are an expert version control synthesis engine. Analyze the git diff.
Group the changed files into distinct, atomic commits based on logical purpose.
Write a strict Conventional Commit message for each group.

Rules for commit messages:
1. Format: <type>(<optional scope>): <description>
2. Types allowed: feat, fix, docs, style, refactor, perf, test, chore.
3. Use the imperative, present tense: 'add' not 'added' or 'adds'.
4. Do not capitalize the first letter of the description.
5. No trailing periods.
6. Be highly specific about the technical intent (e.g., 'feat(api): add stripe webhook endpoint', not 'feat: update api').

Return ONLY a valid JSON array of objects. Absolutely no markdown or conversational text.
Schema: [ { \"message\": \"feat(auth): implement JWT validation\", \"files\": [\"src/auth.rs\"] } ]";

#[derive(Parser)]
#[command(name = "kt", about = "Zero-thought continuous synthesis", version)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// Start a new flow
    Go { name: String },
    /// Intelligently chunk and land your work
    Land,
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

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    match &cli.command {
        Some(Commands::Go { name }) => go(name),
        Some(Commands::Land) => land().await,
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
async fn land() -> Result<()> {
    if !has_head_commit() {
        println!(
            "{} Repository has no commits yet. Create an initial commit before running `kt land`.",
            "·".yellow()
        );
        return Ok(());
    }

    // 1. unwind ONLY contiguous kite saves
    if let Some(base) = get_kite_base()? {
        if base == "root" {
            // Edge case: Unwinding all the way to before the very first commit
            execute_git(&["update-ref", "-d", "HEAD"])?;
        } else {
            execute_git(&["reset", "--soft", &base])?;
        }
    }

    // Stage everything (from the squashed saves + any uncommitted working directory changes)
    execute_git(&["add", "-A"])?;

    let status_output = execute_git(&["diff", "--cached", "--name-only"])?;
    let mut actual_files: HashSet<String> = status_output
        .lines()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();

    if actual_files.is_empty() {
        println!("{} Working directory clean.", "·".dimmed());
        return Ok(());
    }

    let diff = execute_git(&["diff", "--cached"])?;
    execute_git(&["reset"])?; // Unstage for surgical commits

    print!("{} Synthesizing... ", "·".cyan());
    io::stdout().flush()?;

    // 2. Cascade through AI providers
    let groups = match try_local_ollama(&diff).await {
        Ok(g) => {
            println!("(local)");
            g
        }
        Err(local_err) => match try_openai(&diff).await {
            Ok(g) => {
                println!("(cloud)");
                g
            }
            Err(openai_err) => {
                println!("(manual)");
                println!("{} Local provider failed: {}", "·".yellow(), local_err);
                println!("{} OpenAI provider failed: {:#}", "·".yellow(), openai_err);
                return manual_fallback(actual_files);
            }
        },
    };

    println!(); // Spacing

    // 3. Execution
    for group in groups {
        let mut staged_any = false;
        for file in group.files {
            if actual_files.contains(&file) {
                execute_git(&["add", &file])?;
                actual_files.remove(&file);
                staged_any = true;
            }
        }

        if staged_any {
            commit_git(&group.message)?;
            println!(
                "{}",
                render_tree_line(&format!("{}", "│".dimmed()), &group.message)
            );
        }
    }

    // 4. Catch remaining unclassified files
    if !actual_files.is_empty() {
        for file in &actual_files {
            execute_git(&["add", file])?;
        }
        commit_git("chore: unclassified updates")?;
        println!(
            "{}",
            render_tree_line(&format!("{}", "│".dimmed()), "chore: unclassified updates")
        );
    }

    // 5. Publish the history to remote
    if has_remote() {
        let current_branch = get_current_branch()?;
        print!(
            "{} ",
            render_tree_line(&format!("{}", "│".dimmed()), "Publishing to remote...")
        );
        io::stdout().flush()?;

        // We use force-with-lease because `land` rewrites the local history.
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
    }

    println!("{}\n", render_tree_tail("Landed").green());
    Ok(())
}

fn manual_fallback(files: HashSet<String>) -> Result<()> {
    println!(
        "\n{} AI synthesis unavailable. Performing manual squash.",
        "·".yellow()
    );
    for file in files {
        execute_git(&["add", &file])?;
    }

    print!("{} Commit message: ", "·".cyan());
    io::stdout().flush()?;
    let mut msg = String::new();
    io::stdin().read_line(&mut msg)?;

    let msg = msg.trim();
    if msg.is_empty() {
        println!("{} Aborted. Files left staged.", "·".red());
        return Ok(());
    }

    commit_git(msg)?;
    println!("{}\n", render_tree_tail("Landed").green());
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
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(3))
        .build()?;

    let body = serde_json::json!({
        "model": env::var("KITE_LOCAL_MODEL").unwrap_or_else(|_| "llama3".to_string()),
        "messages": [
            { "role": "system", "content": SYSTEM_PROMPT },
            { "role": "user", "content": format!("Diff:\n{}", &diff[..diff.len().min(15000)]) }
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
        "{}\nFor this provider, return an object exactly like: {{ \"groups\": [{{\"message\":\"...\",\"files\":[\"...\"]}}] }}\n\nDiff:\n{}",
        SYSTEM_PROMPT,
        &diff[..diff.len().min(20000)]
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

fn get_openai_env_config() -> Result<(String, String, String)> {
    let base_url = first_non_empty_env(&[
        "KITE_OPENAI_URL",
        "KITE_OPENAI_BASE_URL",
        "OPENAI_URL",
        "OPENAI_BASE_URL",
    ])
    .unwrap_or_else(|| "https://api.openai.com/v1".to_string());

    let model = first_non_empty_env(&["KITE_OPENAI_MODEL", "OPENAI_MODEL"])
        .unwrap_or_else(|| "gpt-5-nano".to_string());

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
    let output = execute_git(&["branch", "--list", "main", "master"])?;
    if output.contains("main") {
        Ok("main".to_string())
    } else {
        Ok("master".to_string())
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

fn get_kite_base() -> Result<Option<String>> {
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
        Ok(Some(base_hash.unwrap_or_else(|| "root".to_string())))
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

    fn run_save_in_repo(repo: &Path) -> Result<()> {
        let original_dir = std::env::current_dir().expect("current dir should resolve");
        std::env::set_current_dir(repo).expect("should enter temp repo");
        let result = save();
        std::env::set_current_dir(&original_dir).expect("should restore original cwd");
        result
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
        let _lock = cwd_lock().lock().expect("cwd lock should be acquired");
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
        let _lock = cwd_lock().lock().expect("cwd lock should be acquired");
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
}
