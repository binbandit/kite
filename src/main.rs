use anyhow::{Context, Result};
use chrono::Local;
use clap::{Parser, Subcommand};
use colored::*;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::env;
use std::io::{self, Write};
use std::process::{Command, Stdio};
use std::thread::current;
use std::time::Duration;

const SYSTEM_PROMPT: &str = "\
You are a version control synthesis engine. Analyze the git diff. 
Group the changed files into distinct, atomic commits based on logical purpose.
Write a Conventional Commit message for each group.
Return ONLY a valid JSON array of objects. Absolutely no markdown or conversational text.
Schema: [ { \"message\": \"feat(core): add routing logic\", \"files\": [\"src/router.rs\"] } ]";

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
        None => save(),
    }
}

// ==========================================
// THE QUICKSAVE
// ==========================================
fn save() -> Result<()> {
    let status = execute_git(&["status", "--porcelain"], false)?;
    if status.trim().is_empty() {
        return Ok(());
    }

    execute_git(&["add", "-A"], false)?;
    let msg = format!("[kite] save {}", Local::now().format("%H:%M:%S"));
    execute_git(&["commit", "-m", &msg, "--no-verify"], false)?;

    println!("{} {}", "·".dimmed(), "saved".dimmed());
    Ok(())
}

fn go(name: &str) -> Result<()> {
    let default_branch = get_default_branch()?;
    let _ = execute_git(&["fetch", "origin", &default_branch], true);

    execute_git(
        &[
            "checkout",
            "-b",
            name,
            &format!("origin/{}", default_branch),
        ],
        false,
    )
    .or_else(|_| execute_git(&["checkout", "-b", name, &default_branch], false))?;

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

    let default_branch = get_default_branch()?;

    // 1. Soft reset to merge base to squash saves
    let merge_base = execute_git(&["merge-base", "HEAD", &default_branch], false)?;
    execute_git(&["reset", "--soft", merge_base.trim()], false)?;
    execute_git(&["add", "-A"], false)?;

    let status_output = execute_git(&["diff", "--cached", "--name-only"], false)?;
    let mut actual_files: HashSet<String> = status_output
        .lines()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();

    if actual_files.is_empty() {
        println!("{} Working directory clean.", "·".dimmed());
        return Ok(());
    }

    let diff = execute_git(&["diff", "--cached"], false)?;
    execute_git(&["reset"], false)?; // Unstage for surgical commits

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
                println!(
                    "{} OpenAI provider failed: {:#}",
                    "·".yellow(),
                    openai_err
                );
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
                execute_git(&["add", &file], false)?;
                actual_files.remove(&file);
                staged_any = true;
            }
        }

        if staged_any {
            execute_git(&["commit", "-m", &group.message], false)?;
            println!("  {} {}", "│".dimmed(), group.message);
        }
    }

    // 4. Catch remaining unclassified files
    if !actual_files.is_empty() {
        for file in &actual_files {
            execute_git(&["add", file], false)?;
        }
        execute_git(&["commit", "-m", "chore: unclassified updates"], false)?;
        println!("  {} chore: unclassified updates", "│".dimmed());
    }

    // 5. Publish the history to remote
    let current_branch = get_current_branch()?;
    print!("{} Publishing to remote... ", "·".cyan());
    io::stdout().flush()?;

    // We use force-with-lease because `land` rewrites the local history.
    // over the top of any previous messy pushes (if you ever pushed your saves).
    match execute_git(&["push", "--set-upstream", "origin", &current_branch, "--force-with-lease"], false) {
        Ok(_) => println!("Done"),
        Err(_) => println!("{}", "Failed (You may need to push manually)".yellow())
    }

    println!("  {}\n", "└─ Landed".green());
    Ok(())
}

fn manual_fallback(files: HashSet<String>) -> Result<()> {
    println!(
        "\n{} AI synthesis unavailable. Performing manual squash.",
        "·".yellow()
    );
    for file in files {
        execute_git(&["add", &file], false)?;
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

    execute_git(&["commit", "-m", msg], false)?;
    println!("  {}\n", "└─ Landed".green());
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
                        let parsed: CommitGroupsEnvelope = serde_json::from_value(structured.clone())?;
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
fn execute_git(args: &[&str], silent: bool) -> Result<String> {
    let mut command = Command::new("git");
    command.args(args);
    if silent {
        command.stderr(Stdio::null()).stdout(Stdio::null());
        let _ = command.status();
        return Ok("".to_string());
    }
    let output = command
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

fn get_default_branch() -> Result<String> {
    let output = execute_git(&["branch", "--list", "main", "master"], false)?;
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
    let output = execute_git(&["rev-parse", "--abbrev-ref", "HEAD"], false)?;
    Ok(output.trim().to_string())
}
