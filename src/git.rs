use anyhow::{Context, Result};
use std::collections::HashSet;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::{Mutex, OnceLock};

const MAX_COMMIT_FAILURE_LINES: usize = 12;
const LOG_PAGE_SIZE: usize = 100;

pub(crate) const SAVE_PREFIX: &str = "[kite] save";

/// Where the contiguous run of Kite saves at the top of history begins.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum KiteBase {
    Root,
    Commit(String),
}

/// The contiguous `[kite] save` commits at the top of history.
#[derive(Clone, Debug)]
pub(crate) struct SaveStack {
    pub(crate) base: KiteBase,
    pub(crate) count: usize,
}

/// Memoizes the repo root for the current working directory so we don't spawn an
/// extra `git rev-parse --show-toplevel` before every git invocation. The cache is
/// keyed on the cwd, so it stays correct if the process changes directories (e.g.
/// across test repositories). Returns `Ok(None)` outside a repository and `Err`
/// only when git itself could not be run.
fn find_repo_root() -> Result<Option<PathBuf>> {
    static CACHE: OnceLock<Mutex<Option<(PathBuf, PathBuf)>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(None));

    let cwd = std::env::current_dir().context("Failed to resolve current directory")?;

    {
        let guard = cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some((cached_cwd, cached_root)) = guard.as_ref()
            && *cached_cwd == cwd
        {
            return Ok(Some(cached_root.clone()));
        }
    }

    let output = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .context("Failed to run git. Is it installed and on your PATH?")?;

    if !output.status.success() {
        return Ok(None);
    }

    let root = PathBuf::from(String::from_utf8_lossy(&output.stdout).trim());

    let mut guard = cache
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    *guard = Some((cwd, root.clone()));

    Ok(Some(root))
}

pub(crate) fn repo_root() -> Result<PathBuf> {
    find_repo_root()?.context("Kite must be run inside a Git repository")
}

pub(crate) fn is_inside_git_repository() -> Result<bool> {
    Ok(find_repo_root()?.is_some())
}

fn git_command() -> Result<Command> {
    let mut command = Command::new("git");
    command.current_dir(repo_root()?);
    Ok(command)
}

/// Takes ownership of command output without a second full copy. `git diff`
/// output can be tens of megabytes, and `from_utf8_lossy(..).to_string()`
/// would allocate all of it a second time even when it is valid UTF-8.
fn into_string(bytes: Vec<u8>) -> String {
    String::from_utf8(bytes)
        .unwrap_or_else(|error| String::from_utf8_lossy(error.as_bytes()).into_owned())
}

pub(crate) fn execute_git(args: &[&str]) -> Result<String> {
    let output = git_command()?
        .args(args)
        .output()
        .with_context(|| format!("Failed 'git {}'", args.join(" ")))?;

    if !output.status.success() {
        anyhow::bail!(
            "Git error: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    Ok(into_string(output.stdout))
}

/// Like `execute_git`, but with extra environment variables and optional
/// stdin — used for `git apply --cached` and temporary-index dry runs.
pub(crate) fn execute_git_with(
    args: &[&str],
    envs: &[(&str, &str)],
    stdin_data: Option<&str>,
) -> Result<String> {
    let mut command = git_command()?;
    command
        .args(args)
        .envs(envs.iter().copied())
        .stdin(if stdin_data.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = command
        .spawn()
        .with_context(|| format!("Failed 'git {}'", args.join(" ")))?;

    // Feed stdin from a scoped thread. Writing a multi-megabyte patch inline
    // and only then waiting would deadlock against any command that fills its
    // stdout or stderr pipe while we are still writing. Scoped so the patch is
    // borrowed rather than copied.
    let output = std::thread::scope(|scope| -> Result<std::process::Output> {
        let writer = stdin_data.map(|data| {
            use std::io::Write;
            let mut stdin = child.stdin.take().expect("stdin should be piped");
            scope.spawn(move || stdin.write_all(data.as_bytes()))
        });

        let output = child
            .wait_with_output()
            .with_context(|| format!("Failed 'git {}'", args.join(" ")))?;

        // A failed write almost always means git exited early; its stderr says
        // why, and that beats reporting a broken pipe. Only surface the write
        // error when git itself claims success.
        if let Some(writer) = writer
            && output.status.success()
            && let Ok(Err(error)) = writer.join()
        {
            return Err(error)
                .with_context(|| format!("Failed writing stdin to 'git {}'", args.join(" ")));
        }

        Ok(output)
    })?;

    if !output.status.success() {
        anyhow::bail!(
            "Git error: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    Ok(into_string(output.stdout))
}

/// Stages a patch into the index without touching the worktree.
pub(crate) fn apply_cached_patch(patch: &str) -> Result<()> {
    execute_git_with(
        &["apply", "--cached", "--whitespace=nowarn"],
        &[],
        Some(patch),
    )
    .map(|_| ())
}

pub(crate) fn execute_git_quiet(args: &[&str]) -> Result<()> {
    let output = git_command()?
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .with_context(|| format!("Failed 'git {}'", args.join(" ")))?;

    if !output.status.success() {
        anyhow::bail!(
            "Git error: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    Ok(())
}

pub(crate) fn has_staged_changes(status: &str) -> bool {
    status.lines().any(is_staged_status_line)
}

pub(crate) fn is_staged_status_line(line: &str) -> bool {
    line.chars()
        .next()
        .is_some_and(|status_code| status_code != ' ' && status_code != '?')
}

pub(crate) fn commit_git(message: &str) -> Result<()> {
    let output = git_command()?
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

pub(crate) fn get_default_branch() -> Result<String> {
    if has_remote()
        && let Ok(output) = execute_git(&[
            "symbolic-ref",
            "--quiet",
            "--short",
            "refs/remotes/origin/HEAD",
        ])
        && let Some(branch) = output.trim().rsplit('/').next()
        && !branch.is_empty()
    {
        return Ok(branch.to_string());
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

pub(crate) fn has_head_commit() -> bool {
    check_ref("HEAD").is_some()
}

pub(crate) fn get_current_branch() -> Result<String> {
    let output = execute_git(&["rev-parse", "--abbrev-ref", "HEAD"])?;
    Ok(output.trim().to_string())
}

/// The checked-out branch, or an error when HEAD is detached. Every command
/// that rewrites or publishes history needs a branch to update, and finding
/// out after the rewrite has started leaves the user stranded.
pub(crate) fn current_branch_name() -> Result<String> {
    let branch = execute_git(&["symbolic-ref", "--quiet", "--short", "HEAD"])
        .map(|output| output.trim().to_string())
        .unwrap_or_default();

    if branch.is_empty() {
        anyhow::bail!(
            "HEAD is detached, so there is no branch to update. Switch to a branch first with `git switch <branch>`."
        );
    }

    Ok(branch)
}

/// True when the repository is mid-merge, mid-rebase, or otherwise holding
/// unmerged index entries. Git's own message for this is a wall of hints.
pub(crate) fn has_unmerged_paths(status: &str) -> bool {
    status.lines().any(|line| {
        let mut codes = line.chars();
        let (Some(index), Some(worktree)) = (codes.next(), codes.next()) else {
            return false;
        };
        index == 'U'
            || worktree == 'U'
            || (index == 'A' && worktree == 'A')
            || (index == 'D' && worktree == 'D')
    })
}

pub(crate) fn is_save_subject(subject: &str) -> bool {
    subject.trim_start().starts_with(SAVE_PREFIX)
}

/// Local repository config, used to remember which branch a land belongs to.
/// Config handles every branch name; a `refs/kite/pre_land/<branch>` ref
/// would hit directory/file conflicts for names like `feat` and `feat/x`.
pub(crate) fn config_get(key: &str) -> Option<String> {
    let output = git_command()
        .ok()?
        .args(["config", "--local", "--get", key])
        .stderr(Stdio::null())
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let value = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!value.is_empty()).then_some(value)
}

pub(crate) fn config_set(key: &str, value: &str) -> Result<()> {
    execute_git_quiet(&["config", "--local", key, value])
}

pub(crate) fn config_unset(key: &str) {
    let _ = execute_git_quiet(&["config", "--local", "--unset", key]);
}

/// True when `ancestor` is reachable from `descendant`, i.e. pushing
/// `descendant` would fast-forward past `ancestor` rather than discard it.
pub(crate) fn is_ancestor(ancestor: &str, descendant: &str) -> bool {
    git_command()
        .and_then(|mut command| {
            command
                .args(["merge-base", "--is-ancestor", ancestor, descendant])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .context("Failed 'git merge-base'")
        })
        .map(|status| status.success())
        .unwrap_or(false)
}

/// Subjects of the commits `reference` has that `HEAD` does not — exactly
/// what a force-push would drop.
pub(crate) fn subjects_missing_from_head(reference: &str) -> Vec<String> {
    let Ok(output) = execute_git(&["log", "--format=%s", &format!("HEAD..{reference}")]) else {
        return Vec::new();
    };

    output
        .lines()
        .map(str::trim)
        .filter(|subject| !subject.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

pub(crate) fn has_remote() -> bool {
    git_command()
        .and_then(|mut command| {
            command
                .args(["remote"])
                .output()
                .context("Failed 'git remote'")
        })
        .map(|output| !String::from_utf8_lossy(&output.stdout).trim().is_empty())
        .unwrap_or(false)
}

pub(crate) fn check_ref(ref_name: &str) -> Option<String> {
    let output = git_command()
        .ok()?
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

/// Explicit prefixes, context width, and binary payloads keep the diff
/// parseable into hunks and replayable with `git apply` regardless of the
/// user's diff configuration. One line of context (`-U1`) keeps nearby edits
/// in separate hunks so they can land in different commits; `git apply`
/// still anchors reliably because it matches the removed lines as well, and
/// the pre-land tree verification catches any misapplication.
pub(crate) fn diff_for_base(base: &KiteBase) -> Result<String> {
    match base {
        KiteBase::Commit(hash) => {
            let range = format!("{hash}..HEAD");
            execute_git(&[
                "-c",
                "core.quotepath=false",
                "diff",
                "--no-color",
                "--no-ext-diff",
                "--no-renames",
                "--binary",
                "--src-prefix=a/",
                "--dst-prefix=b/",
                "-U1",
                &range,
            ])
        }
        KiteBase::Root => execute_git(&[
            "-c",
            "core.quotepath=false",
            "diff-tree",
            "--root",
            "--no-commit-id",
            "-r",
            "-p",
            "--no-renames",
            "--binary",
            "--src-prefix=a/",
            "--dst-prefix=b/",
            "-U1",
            "HEAD",
        ]),
    }
}

/// Best-effort style examples; an unreadable log just means no examples.
pub(crate) fn recent_commit_style_examples(limit: usize) -> Vec<String> {
    let Ok(output) = execute_git(&["log", "--format=%s", "-n", "30"]) else {
        return Vec::new();
    };

    let mut seen = HashSet::new();
    let mut examples = Vec::new();

    for line in output.lines() {
        let message = line.trim();
        if message.is_empty() || message.starts_with(SAVE_PREFIX) || message.starts_with("Merge ") {
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

    examples
}

/// Walks history from `HEAD` in pages until the first non-Kite commit, so huge
/// repositories never pay for a full `git log`. `--first-parent` keeps the walk
/// on the branch's own line of development: without it a merged-in side branch
/// can interleave by commit date and move the base onto an unrelated commit,
/// which would make `kt land` rewrite more history than the user's saves.
pub(crate) fn kite_save_stack() -> Result<Option<SaveStack>> {
    let mut count = 0;

    for page in 0.. {
        let skip = (page * LOG_PAGE_SIZE).to_string();
        let max = LOG_PAGE_SIZE.to_string();
        let output = match execute_git(&[
            "log",
            "--first-parent",
            "--format=%H %s",
            "-n",
            &max,
            "--skip",
            &skip,
        ]) {
            Ok(output) => output,
            Err(_) if page == 0 => return Ok(None), // no commits yet
            Err(error) => return Err(error),
        };

        let mut page_lines = 0;
        for line in output.lines().filter(|line| !line.trim().is_empty()) {
            page_lines += 1;
            let Some((hash, subject)) = line.split_once(' ') else {
                continue;
            };

            if subject.starts_with(SAVE_PREFIX) {
                count += 1;
            } else if count == 0 {
                return Ok(None);
            } else {
                return Ok(Some(SaveStack {
                    base: KiteBase::Commit(hash.to_string()),
                    count,
                }));
            }
        }

        if page_lines < LOG_PAGE_SIZE {
            break; // reached the root commit
        }
    }

    if count > 0 {
        Ok(Some(SaveStack {
            base: KiteBase::Root,
            count,
        }))
    } else {
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{acquire_cwd_lock, git, init_repo, with_repo_cwd, write_file};

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
    fn kite_save_stack_counts_contiguous_saves_above_base() {
        let _lock = acquire_cwd_lock();
        let repo = init_repo();
        let base_sha = git(&repo.path, &["rev-parse", "HEAD"]);

        write_file(&repo.path, "tracked.txt", "first\n");
        git(&repo.path, &["add", "tracked.txt"]);
        git(&repo.path, &["commit", "-m", "[kite] save 12:00:00"]);

        write_file(&repo.path, "tracked.txt", "second\n");
        git(&repo.path, &["add", "tracked.txt"]);
        git(&repo.path, &["commit", "-m", "[kite] save 12:00:01"]);

        let stack = with_repo_cwd(&repo.path, kite_save_stack)
            .expect("stack should resolve")
            .expect("saves should be found");

        assert_eq!(stack.count, 2);
        assert_eq!(stack.base, KiteBase::Commit(base_sha.trim().to_string()));
    }

    #[test]
    fn kite_save_stack_is_none_without_saves_on_top() {
        let _lock = acquire_cwd_lock();
        let repo = init_repo();

        let stack = with_repo_cwd(&repo.path, kite_save_stack).expect("stack should resolve");
        assert!(stack.is_none());
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

        let examples = with_repo_cwd(&repo.path, || recent_commit_style_examples(6));

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
