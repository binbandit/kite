use anyhow::{Context, Result};
use fs2::FileExt;
use std::collections::HashSet;
use std::fs::{File, OpenOptions};
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

/// An advisory lock held for one complete Kite command in this worktree.
/// Atomic refs serialize repository-wide marker transitions, while this lock
/// closes the smaller race where a second `kt undo` could otherwise recover a
/// live land owned by the same worktree.
#[derive(Debug)]
pub(crate) struct WorktreeCommandLock(File);

impl Drop for WorktreeCommandLock {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.0);
    }
}

pub(crate) fn lock_current_worktree() -> Result<WorktreeCommandLock> {
    let lock_path = PathBuf::from(current_worktree_key()?).join("kite-command.lock");
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .with_context(|| {
            format!(
                "Could not open Kite's worktree lock at {}",
                lock_path.display()
            )
        })?;
    file.try_lock_exclusive().with_context(|| {
        "Another Kite command is already running in this worktree. Wait for it to finish, then retry."
    })?;
    Ok(WorktreeCommandLock(file))
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
/// stdin, for the ref transactions and pathspec lists that are too large or
/// too literal to pass as arguments.
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

/// Whether a commit runs the repository's Git hooks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Hooks {
    Run,
    Skip,
}

pub(crate) fn commit_git(message: &str, hooks: Hooks) -> Result<()> {
    let mut args = vec!["commit", "-m", message];
    if hooks == Hooks::Skip {
        args.push("--no-verify");
    }

    let output = git_command()?
        .args(&args)
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

    // Deliberately says nothing about what was left behind: the caller undoes
    // the whole attempt and is the only one that knows the resulting state.
    if details.is_empty() {
        return format!("{} for `{}`.", summary, message);
    }

    format!(
        "{} for `{}`.\n\n{}",
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

/// Where `HEAD` points.
///
/// Deliberately the only way to ask. `rev-parse --abbrev-ref HEAD` answers
/// "HEAD" when detached, and callers that took that for a branch name went on
/// to rewrite history and fail at `git branch -f HEAD`, or push a remote
/// branch literally called HEAD. Making the two cases distinct types means a
/// caller has to say which one it can handle: `kt land` and `kt undo` move
/// `HEAD` itself and work either way, while the commands that push ask for a
/// branch with `branch_to_publish`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Head {
    Branch(String),
    Detached(String),
}

/// How a detached `HEAD` is recorded as a land's target. Ref names cannot
/// contain spaces, so this can never collide with a branch name.
pub(crate) const DETACHED_TARGET: &str = "(detached HEAD)";

impl Head {
    /// Stable kind of target recorded by a land. Deliberately excludes the
    /// commit: a land moves `HEAD`, so the sha is not stable. Detached targets
    /// are paired with `current_worktree_key` to identify the exact checkout.
    pub(crate) fn land_key(&self) -> String {
        match self {
            Head::Branch(branch) => branch.clone(),
            Head::Detached(_) => DETACHED_TARGET.to_string(),
        }
    }

    /// How this position reads in a message. A detached `HEAD` has no name of
    /// its own, so it is described by the commit it sits on.
    pub(crate) fn describe(&self) -> String {
        match self {
            Head::Branch(branch) => format!("`{branch}`"),
            Head::Detached(sha) => format!("the detached HEAD at {}", short_sha(sha)),
        }
    }
}

pub(crate) fn short_sha(sha: &str) -> String {
    sha.chars().take(7).collect()
}

/// Where `HEAD` points, or an error when it cannot be resolved at all.
///
/// An unborn branch reads as `Head::Branch`: git knows the name, it just has
/// no commit yet, which is exactly what `kt undo` needs to unmake a root save.
pub(crate) fn head_position() -> Result<Head> {
    if let Some(symbolic_ref) = head_symbolic_ref()
        && let Some(branch) = symbolic_ref.strip_prefix("refs/heads/")
        && !branch.is_empty()
    {
        return Ok(Head::Branch(branch.to_string()));
    }

    let sha = execute_git(&["rev-parse", "HEAD"])
        .context("Could not resolve HEAD. Make an initial commit first.")?;
    Ok(Head::Detached(sha.trim().to_string()))
}

/// The exact ref to which `HEAD` is symbolic, including Kite's temporary
/// transaction branch. Ordinary callers should use `head_position`; recovery
/// needs the full ref so it can prove HEAD is still on the transaction it owns.
pub(crate) fn head_symbolic_ref() -> Option<String> {
    execute_git(&["symbolic-ref", "--quiet", "HEAD"])
        .ok()
        .map(|output| output.trim().to_string())
        .filter(|name| !name.is_empty())
}

/// The checked-out branch, or an error when `HEAD` is detached.
///
/// Only for the commands that push. `git push` has to be told which remote ref
/// to write and a detached `HEAD` supplies no name, so `kt publish` and `kt pr`
/// need a real branch — and finding that out after a rewrite would leave the
/// user stranded. Everything else uses `head_position` and works detached.
pub(crate) fn branch_to_publish() -> Result<String> {
    match head_position()? {
        Head::Branch(branch) => Ok(branch),
        Head::Detached(sha) => anyhow::bail!(
            "HEAD is detached at {}, so there is no branch to push. Create one with `git switch -c <name>`, then try again.",
            short_sha(&sha)
        ),
    }
}

/// Stable identity for this checkout, including linked worktrees.
///
/// Atomic rollback metadata is repository-wide, so a detached marker needs
/// this extra identity to distinguish one linked worktree from another. Git
/// gives each worktree its own administrative directory even when several
/// point at detached commits.
pub(crate) fn current_worktree_key() -> Result<String> {
    let git_dir = execute_git(&["rev-parse", "--absolute-git-dir"])?;
    let git_dir = git_dir.trim();
    if git_dir.is_empty() {
        anyhow::bail!("Could not identify the current Git worktree.");
    }
    Ok(git_dir.to_string())
}

/// Names an unfinished Git operation in this worktree, if there is one.
///
/// A clean index does not mean Git is idle: an interactive rebase, bisect, or
/// no-conflict cherry-pick can all leave `HEAD` detached without any unmerged
/// paths. Kite must not mistake that temporary checkout for an ordinary
/// detached worktree and start its own history rewrite on top of it.
pub(crate) fn active_git_operation() -> Result<Option<&'static str>> {
    let git_dir = PathBuf::from(current_worktree_key()?);
    let markers = [
        ("rebase-merge", "rebase"),
        ("rebase-apply", "rebase or am"),
        ("MERGE_HEAD", "merge"),
        ("CHERRY_PICK_HEAD", "cherry-pick"),
        ("REVERT_HEAD", "revert"),
        ("BISECT_START", "bisect"),
        ("sequencer", "sequenced cherry-pick or revert"),
    ];

    Ok(markers
        .into_iter()
        .find_map(|(path, operation)| git_dir.join(path).exists().then_some(operation)))
}

/// The checked-out branch read straight from `.git/HEAD`, with no subprocess.
///
/// `kt` runs constantly, so a spawn on its path is a cost users feel on every
/// save. Returns `None` for a detached HEAD or any layout this cannot read —
/// callers must treat that as "don't know", never as a branch name. Anything
/// that rewrites or publishes history uses `head_position` instead.
pub(crate) fn head_branch_hint() -> Option<String> {
    let root = find_repo_root().ok()??;

    let dot_git = root.join(".git");
    let git_dir = if dot_git.is_dir() {
        dot_git
    } else {
        // Linked worktrees and submodules keep `.git` as a file holding
        // `gitdir: <path>`.
        let pointer = std::fs::read_to_string(&dot_git).ok()?;
        let target = PathBuf::from(pointer.strip_prefix("gitdir:")?.trim());
        if target.is_absolute() {
            target
        } else {
            root.join(target)
        }
    };

    let head = std::fs::read_to_string(git_dir.join("HEAD")).ok()?;
    let branch = head.trim().strip_prefix("ref: refs/heads/")?;
    (!branch.is_empty()).then(|| branch.to_string())
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

/// Reads legacy local marker config written by Kite versions before rollback
/// state moved into one atomic ref-backed object.
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

pub(crate) fn config_unset(key: &str) -> Result<()> {
    if config_get(key).is_none() {
        return Ok(());
    }
    execute_git_quiet(&["config", "--local", "--unset", key])
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

/// Stages exactly these paths, whole, from the worktree.
///
/// The list travels over stdin because an argument list has a size limit a
/// diff touching tens of thousands of files would exceed, and
/// `--literal-pathspecs` stops a path like `a*b.txt` or `:colon.txt` from
/// being read as a glob or as pathspec magic, which would either stage a file
/// the plan never assigned or fail outright. `--force` is safe here because
/// every path came from a save the user already made: one force-added past an
/// ignore rule must still be able to land.
pub(crate) fn stage_paths(paths: &[String]) -> Result<()> {
    let mut pathspecs = String::new();
    for path in paths {
        pathspecs.push_str(path);
        pathspecs.push('\0');
    }

    execute_git_with(
        &[
            "--literal-pathspecs",
            "add",
            "--force",
            "--pathspec-from-file=-",
            "--pathspec-file-nul",
        ],
        &[],
        Some(&pathspecs),
    )
    .map(|_| ())
}

/// The exact paths a set of saves changed. NUL-separated (`-z`) because git
/// otherwise C-quotes any path containing a quote, a backslash, or a newline,
/// and a quoted path no longer names the file it came from.
pub(crate) fn changed_paths_for_base(base: &KiteBase) -> Result<Vec<String>> {
    let listed = match base {
        KiteBase::Commit(hash) => {
            let range = format!("{hash}..HEAD");
            execute_git(&[
                "diff",
                "--no-ext-diff",
                "--no-renames",
                "--name-only",
                "-z",
                &range,
            ])?
        }
        KiteBase::Root => execute_git(&[
            "diff-tree",
            "--root",
            "--no-commit-id",
            "-r",
            "--no-renames",
            "--name-only",
            "-z",
            "HEAD",
        ])?,
    };

    Ok(listed
        .split('\0')
        .filter(|path| !path.is_empty())
        .map(ToOwned::to_owned)
        .collect())
}

/// The diff Kite shows the model. It is only ever displayed, never parsed for
/// paths, so these flags are here to keep it readable and independent of the
/// user's diff configuration. `--no-renames` does more than that: it keeps
/// this diff aligned with `changed_paths_for_base`, reading a rename as a
/// deletion plus an addition rather than one section spanning two paths that
/// could not be staged as a single whole file.
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
                "--src-prefix=a/",
                "--dst-prefix=b/",
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
            "--src-prefix=a/",
            "--dst-prefix=b/",
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
        assert!(rendered.contains("  pre-commit: cargo fmt --check failed"));
        // The caller undoes the attempt, so this must not claim otherwise.
        assert!(!rendered.contains("left in place"));
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
    fn worktree_command_lock_rejects_a_second_live_command() {
        let _lock = acquire_cwd_lock();
        let repo = init_repo();

        let first = with_repo_cwd(&repo.path, lock_current_worktree)
            .expect("first command should acquire the worktree lock");
        let error = with_repo_cwd(&repo.path, lock_current_worktree)
            .expect_err("a second live command must be rejected");
        assert!(format!("{error:#}").contains("Another Kite command"));

        drop(first);
        with_repo_cwd(&repo.path, lock_current_worktree)
            .expect("the lock should be released when the command exits");
    }

    #[test]
    fn head_position_names_a_branch_and_reports_a_detached_commit() {
        let _lock = acquire_cwd_lock();
        let repo = init_repo();
        let branch = git(&repo.path, &["rev-parse", "--abbrev-ref", "HEAD"])
            .trim()
            .to_string();
        let sha = git(&repo.path, &["rev-parse", "HEAD"]).trim().to_string();

        assert_eq!(
            with_repo_cwd(&repo.path, head_position).expect("HEAD should resolve"),
            Head::Branch(branch.clone())
        );
        assert_eq!(
            with_repo_cwd(&repo.path, branch_to_publish).expect("a branch should be publishable"),
            branch
        );

        git(&repo.path, &["checkout", "-q", "--detach"]);

        assert_eq!(
            with_repo_cwd(&repo.path, head_position).expect("a detached HEAD should resolve"),
            Head::Detached(sha.clone())
        );

        // Nothing to name in a `git push`, and the message has to say which
        // commit the user is sitting on to be worth anything.
        let err = with_repo_cwd(&repo.path, branch_to_publish)
            .expect_err("a detached HEAD has no branch to push");
        let rendered = format!("{err:#}");
        assert!(rendered.contains(&short_sha(&sha)));
        assert!(rendered.contains("git switch -c"));
    }

    /// Git knows the branch name before the first commit exists, and `kt undo`
    /// relies on that to make a branch unborn again after a root save.
    #[test]
    fn head_position_reads_an_unborn_branch_as_a_branch() {
        let _lock = acquire_cwd_lock();
        let repo = crate::test_support::TempDir::new("kite-test-unborn");
        git(&repo.path, &["init", "-q"]);

        let expected = git(&repo.path, &["symbolic-ref", "--short", "HEAD"])
            .trim()
            .to_string();

        assert_eq!(
            with_repo_cwd(&repo.path, head_position).expect("an unborn HEAD should resolve"),
            Head::Branch(expected)
        );
    }

    #[test]
    fn a_detached_land_key_cannot_be_mistaken_for_a_branch() {
        assert_eq!(Head::Branch("feat/x".to_string()).land_key(), "feat/x");
        assert_eq!(
            Head::Detached("0123456789abcdef".to_string()).land_key(),
            DETACHED_TARGET
        );
        // Ref names cannot contain spaces, which is the whole reason this marker
        // is safe to store in the same config key as a branch name.
        assert!(DETACHED_TARGET.contains(' '));
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
