use anyhow::{Context, Result, anyhow};
use chrono::Local;
use colored::*;
use std::collections::HashSet;

use crate::ai::flatten_error;
use crate::git::{
    KiteBase, apply_cached_patch, check_ref, commit_git, config_get, config_set, config_unset,
    current_branch_name, diff_for_base, execute_git, execute_git_with, has_head_commit, has_remote,
    has_unmerged_paths, is_ancestor, is_save_subject, kite_save_stack, subjects_missing_from_head,
};
use crate::hunks::{DiffUnits, FileStat, parse_diff};
use crate::synth::{
    CommitGroup, MAX_PLANNABLE_UNITS, normalize_groups, sanitize_commit_message, synthesize_groups,
};
use crate::ui::{Spinner, confirm, pluralize, print_ai_unavailable, prompt_line};

/// Where the pre-land `HEAD` is parked so `kt undo` can restore it, plus the
/// local config keys recording which branch that land belongs to and where it
/// left `HEAD`.
const PRE_LAND_REF: &str = "refs/kite/pre_land";
const PRE_LAND_BRANCH_KEY: &str = "kite.preland.branch";
const PRE_LAND_HEAD_KEY: &str = "kite.preland.head";

/// How many about-to-be-discarded remote commits to list before summarizing.
const MAX_DROPPED_COMMITS_SHOWN: usize = 10;

#[derive(Clone, Debug)]
struct LandScope {
    base: KiteBase,
    save_count: usize,
    units: DiffUnits,
}

/// One planned commit and how its content gets staged: an exact patch of the
/// assigned hunks, or whole files when hunk-level staging is unavailable.
#[derive(Clone, Debug)]
struct LandCommit {
    message: String,
    files: Vec<FileStat>,
    stage: StageOp,
}

#[derive(Clone, Debug)]
enum StageOp {
    Patch(String),
    WholeFiles(Vec<String>),
}

pub(crate) async fn land(push: bool, auto_confirm: bool, allow_dirty: bool) -> Result<()> {
    if !land_preflight()? {
        return Ok(());
    }

    let stashed = if allow_dirty {
        stash_dirty_worktree_for_land()?
    } else {
        false
    };

    let land_result = (async {
        let Some(scope) = collect_land_scope(allow_dirty)? else {
            return Ok(());
        };

        let spinner = Spinner::start("Synthesizing");
        let synthesized = synthesize_groups(&scope.units).await;
        spinner.stop();

        let commits = match synthesized {
            Ok(raw_groups) => {
                let groups = normalize_groups(raw_groups, &scope.units);
                if groups.is_empty() {
                    anyhow::bail!("No changes were assigned to landed commit groups.");
                }
                plan_commits(&scope.base, &scope.units, &groups)
            }
            Err(error) => {
                print_ai_unavailable(&error);
                if auto_confirm {
                    anyhow::bail!(
                        "AI synthesis is unavailable. Rerun `kt land` without --yes to enter a commit message manually."
                    );
                }
                let Some(message) = prompt_line("One commit message (blank to abort)")? else {
                    println!("{} Aborted — no history changed", "·".red());
                    return Ok(());
                };
                vec![whole_files_commit(
                    sanitize_commit_message(&message),
                    scope.units.all_files(),
                )]
            }
        };

        print!("{}", render_land_plan(&commits, scope.save_count));

        let question = if push {
            "Rewrite history and publish?"
        } else {
            "Rewrite history?"
        };
        if !auto_confirm && !confirm(question)? {
            println!("{} Aborted — no history changed", "·".red());
            return Ok(());
        }

        execute_land(&scope.base, &commits)?;

        if push {
            println!("{} Landed", "✓".green());
            publish_current_branch().context("Landed locally, but publishing failed")?;
        } else if has_remote() {
            println!(
                "{} Landed — review, then {} or {}",
                "✓".green(),
                "kt publish".bold(),
                "kt pr".bold()
            );
        } else {
            println!("{} Landed", "✓".green());
        }

        Ok(())
    })
    .await;

    if stashed && let Err(restore_error) = restore_dirty_worktree_for_land() {
        return match land_result {
            Ok(_) => Err(restore_error),
            Err(land_error) => Err(anyhow!(
                "{land_error}\n\nIn addition, restoring your stashed changes failed: {restore_error}"
            )),
        };
    }

    land_result
}

/// Publishes the branch, discarding remote commits only when they are the
/// Kite saves this branch just rewrote. Deliberately no `pull --rebase` first:
/// after a land the remote still holds the old saves, and rebasing onto them
/// would resurrect the history we just rewrote.
///
/// A bare `--force-with-lease` is not enough. The lease compares against the
/// local remote-tracking ref, and when that ref does not exist — the usual
/// case for a branch someone else created — git has nothing to compare and
/// lets the push through, silently destroying their work. So we fetch, ask
/// what would actually be dropped, and only decide from that.
pub(crate) fn publish_current_branch() -> Result<()> {
    if !has_remote() {
        println!("{} No remote — history stays local", "·".dimmed());
        return Ok(());
    }

    let branch = current_branch_name()?;
    let remote_ref = format!("refs/remotes/origin/{branch}");

    let spinner = Spinner::start(format!("Publishing {branch}"));
    // Refresh the tracking ref so the decision reflects the real remote.
    let _ = execute_git(&["fetch", "origin", &branch]);
    let remote_sha = check_ref(&remote_ref);
    spinner.stop();

    let force = match &remote_sha {
        // Nothing on the remote yet, or the remote is already an ancestor:
        // a plain push is enough and forcing would be wrong.
        None => None,
        Some(sha) if is_ancestor(sha, "HEAD") => None,
        Some(sha) => {
            confirm_discarding_remote_commits(&branch, sha)?;
            Some(format!("--force-with-lease={branch}:{sha}"))
        }
    };

    let mut args = vec!["push", "--set-upstream", "origin", &branch];
    if let Some(force) = &force {
        args.push(force);
    }

    let spinner = Spinner::start(format!("Publishing {branch}"));
    let pushed = execute_git(&args);
    spinner.stop();

    pushed.with_context(|| {
        format!(
            "Push rejected. `origin/{branch}` moved while Kite was publishing — rerun `kt publish` to see the new state."
        )
    })?;
    println!("{} Published {}", "✓".green(), branch.bold());
    Ok(())
}

/// The remote has commits this branch does not. When they are all Kite saves
/// they are the ones we just landed, so replacing them is the whole point.
/// Anything else is someone's work and needs an explicit yes.
fn confirm_discarding_remote_commits(branch: &str, remote_sha: &str) -> Result<()> {
    let dropped = subjects_missing_from_head(remote_sha);
    if dropped.iter().all(|subject| is_save_subject(subject)) {
        return Ok(());
    }

    println!(
        "{} `origin/{branch}` has {} that {} does not:",
        "!".yellow(),
        pluralize(dropped.len(), "commit"),
        "your branch".bold()
    );
    for subject in dropped.iter().take(MAX_DROPPED_COMMITS_SHOWN) {
        println!("     {} {}", "-".dimmed(), subject);
    }
    if dropped.len() > MAX_DROPPED_COMMITS_SHOWN {
        println!(
            "     {}",
            format!("… and {} more", dropped.len() - MAX_DROPPED_COMMITS_SHOWN).dimmed()
        );
    }

    if confirm("Publishing will discard them from the remote. Continue?")? {
        return Ok(());
    }

    anyhow::bail!(
        "Publish cancelled — `origin/{branch}` untouched. Run `git fetch origin {branch}` and reconcile if you want to keep those commits."
    )
}

pub(crate) fn undo() -> Result<()> {
    let Some(pre_land_sha) = check_ref(PRE_LAND_REF) else {
        println!("{} Nothing to undo — no land recorded", "·".yellow());
        return Ok(());
    };

    let branch = current_branch_name()?;
    guard_undo_target(&branch)?;

    let status = execute_git(&["status", "--porcelain"])?;
    if !status.trim().is_empty() {
        anyhow::bail!(
            "Working directory is not clean. Please `kt` your changes or stash them before undoing."
        );
    }

    execute_git(&["reset", "--hard", &pre_land_sha])?;
    execute_git(&["update-ref", "-d", PRE_LAND_REF])?;
    config_unset(PRE_LAND_BRANCH_KEY);
    config_unset(PRE_LAND_HEAD_KEY);

    if has_remote() {
        let spinner = Spinner::start("Reverting remote");
        let reverted = execute_git(&[
            "push",
            "--force-with-lease",
            "origin",
            &format!("{pre_land_sha}:refs/heads/{branch}"),
        ]);
        spinner.stop();
        if reverted.is_err() {
            println!(
                "{} Remote not reverted — it may have diverged",
                "·".yellow()
            );
        }
    }

    println!("{} Restored pre-land saves", "✓".green());
    Ok(())
}

/// `refs/kite/pre_land` is a single repo-wide ref, so on its own it says
/// nothing about which branch it belongs to. Without this guard, landing on
/// one branch and undoing on another hard-resets — and force-pushes — the
/// wrong branch to an unrelated commit.
fn guard_undo_target(branch: &str) -> Result<()> {
    let Some(landed_branch) = config_get(PRE_LAND_BRANCH_KEY) else {
        // Recorded by a Kite that predates this guard: we cannot tell which
        // branch it belongs to, so make the user look before it resets.
        if confirm(&format!(
            "The recorded land has no branch attached. Reset `{branch}` to it anyway?"
        ))? {
            return Ok(());
        }
        anyhow::bail!("Undo cancelled — nothing changed");
    };

    if landed_branch != branch {
        anyhow::bail!(
            "The last land was on `{landed_branch}`, but you are on `{branch}`. Run `git switch {landed_branch}` first — undoing here would reset `{branch}` to unrelated history."
        );
    }

    // Landing recorded where it left HEAD. If HEAD moved since, undo would
    // throw away whatever was committed on top of the landed commits.
    let Some(landed_head) = config_get(PRE_LAND_HEAD_KEY) else {
        return Ok(());
    };
    let current_head = execute_git(&["rev-parse", "HEAD"])?;
    if current_head.trim() == landed_head {
        return Ok(());
    }

    let added = execute_git(&["log", "--format=%s", &format!("{landed_head}..HEAD")])
        .map(|output| {
            output
                .lines()
                .filter(|line| !line.trim().is_empty())
                .count()
        })
        .unwrap_or(0);

    if confirm(&format!(
        "`{branch}` has moved since that land. Undo will discard {} made since. Continue?",
        pluralize(added, "commit")
    ))? {
        return Ok(());
    }
    anyhow::bail!("Undo cancelled — nothing changed")
}

pub(crate) const RECOVERY_BRANCH_PREFIX: &str = "kite-recovery-";

/// A land that was interrupted — Ctrl-C, a crash, a terminal closed — leaves
/// the user parked on the temporary branch with no explanation, and the next
/// command they run behaves oddly for reasons they cannot see. Every command
/// says so up front instead.
pub(crate) fn warn_if_on_recovery_branch() {
    let Ok(branch) = current_branch_name() else {
        return;
    };
    if !branch.starts_with(RECOVERY_BRANCH_PREFIX) {
        return;
    }

    println!(
        "{} You are on {}, left by an interrupted `kt land`.",
        "!".yellow(),
        branch.bold()
    );
    match config_get(PRE_LAND_BRANCH_KEY) {
        Some(landed_branch) => println!(
            "  {}",
            format!(
                "Run `git switch {landed_branch}` to abandon it, or `git reset --hard {PRE_LAND_REF}` there to get your saves back."
            )
            .dimmed()
        ),
        None => println!(
            "  {}",
            format!("Your pre-land state is at `{PRE_LAND_REF}`.").dimmed()
        ),
    }
}

/// Everything that must hold before Kite touches the worktree. Run before
/// `--allow-dirty` stashes anything, so a repository that cannot be landed
/// never gets its work put away first and its diagnosis second.
fn land_preflight() -> Result<bool> {
    if !has_head_commit() {
        println!(
            "{} No commits yet — make an initial commit before landing",
            "·".yellow()
        );
        return Ok(false);
    }

    current_branch_name()?;

    let status = execute_git(&["status", "--porcelain"])?;
    if has_unmerged_paths(&status) {
        anyhow::bail!(
            "This repository has unresolved merge conflicts. Resolve them and commit the merge before landing."
        );
    }

    Ok(true)
}

fn collect_land_scope(allow_dirty: bool) -> Result<Option<LandScope>> {
    if !has_head_commit() {
        println!(
            "{} No commits yet — make an initial commit before landing",
            "·".yellow()
        );
        return Ok(None);
    }

    // A detached HEAD has no branch to move onto the landed commits. Checked
    // here so it costs nothing and reports before any work is done.
    current_branch_name()?;

    if !allow_dirty {
        let status = execute_git(&["status", "--porcelain"])?;
        if !status.trim().is_empty() {
            anyhow::bail!(
                "Working directory must be clean before `kt land`. Run `kt` to snapshot current work, or use `kt land --allow-dirty` to stash it temporarily."
            );
        }
    }

    let Some(stack) = kite_save_stack()? else {
        println!(
            "{} {}",
            "·".dimmed(),
            "nothing to land — create saves with `kt` first".dimmed()
        );
        return Ok(None);
    };

    let diff = diff_for_base(&stack.base)?;
    let mut units = parse_diff(&diff);

    // Past the plannable limit the prompt cannot carry enough of each hunk for
    // the model to tell them apart, so it would be assigning ids it never saw
    // the contents of. Group whole files instead, and say so.
    if units.unit_count() > MAX_PLANNABLE_UNITS {
        println!(
            "{} {}",
            "·".yellow(),
            format!(
                "{} in these saves — grouping by file instead of by hunk",
                pluralize(units.unit_count(), "hunk")
            )
            .dimmed()
        );
        units = units.coarsened_to_files();
    }

    if units.is_empty() {
        println!(
            "{} {}",
            "·".dimmed(),
            "nothing to land — the saves contain no changes".dimmed()
        );
        return Ok(None);
    }

    Ok(Some(LandScope {
        base: stack.base,
        save_count: stack.count,
        units,
    }))
}

fn stash_dirty_worktree_for_land() -> Result<bool> {
    let status = execute_git(&["status", "--porcelain"])?;
    if status.trim().is_empty() {
        return Ok(false);
    }

    execute_git(&[
        "stash",
        "push",
        "--include-untracked",
        "-m",
        "kt land: temporary stash",
    ])?;
    Ok(true)
}

fn restore_dirty_worktree_for_land() -> Result<()> {
    execute_git(&["stash", "pop", "--index"])?;
    Ok(())
}

/// Builds patch-staged commits from the hunk groups, then dry-runs the whole
/// sequence against a temporary index. If the replayed commits would not
/// reproduce the saved tree exactly, falls back to whole-file staging so a
/// landing can never change what the branch ultimately contains.
fn plan_commits(base: &KiteBase, units: &DiffUnits, groups: &[CommitGroup]) -> Vec<LandCommit> {
    let commits: Vec<LandCommit> = groups
        .iter()
        .map(|group| {
            let ids: HashSet<String> = group.hunks.iter().cloned().collect();
            LandCommit {
                message: group.message.clone(),
                files: units.file_stats(&ids),
                stage: StageOp::Patch(units.assemble_patch(&ids)),
            }
        })
        .collect();

    match verify_patch_plan(base, &commits) {
        Ok(()) => commits,
        Err(error) => {
            println!(
                "{} Hunk-level plan failed verification — landing whole files instead ({})",
                "·".yellow(),
                flatten_error(&format!("{error:#}"))
            );
            collapse_to_whole_files(units, groups)
        }
    }
}

/// Replays the patch sequence against a temporary index and requires the
/// resulting tree to be identical to `HEAD`'s tree, so a hunk-level rewrite
/// is proven exact before any history changes.
fn verify_patch_plan(base: &KiteBase, commits: &[LandCommit]) -> Result<()> {
    let index_path = std::env::temp_dir().join(format!(
        "kite-verify-index-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or(0)
    ));
    let index_path_string = index_path.to_string_lossy().into_owned();
    let envs = [("GIT_INDEX_FILE", index_path_string.as_str())];

    let result = (|| {
        match base {
            KiteBase::Commit(sha) => {
                let tree = format!("{sha}^{{tree}}");
                execute_git_with(&["read-tree", &tree], &envs, None)?;
            }
            KiteBase::Root => {
                execute_git_with(&["read-tree", "--empty"], &envs, None)?;
            }
        }

        for commit in commits {
            let StageOp::Patch(patch) = &commit.stage else {
                continue;
            };
            execute_git_with(
                &["apply", "--cached", "--whitespace=nowarn"],
                &envs,
                Some(patch),
            )
            .with_context(|| format!("`{}` did not apply cleanly", commit.message))?;
        }

        let landed_tree = execute_git_with(&["write-tree"], &envs, None)?;
        let expected_tree = execute_git(&["rev-parse", "HEAD^{tree}"])?;
        if landed_tree.trim() != expected_tree.trim() {
            anyhow::bail!("replayed commits do not reproduce the saved tree");
        }
        Ok(())
    })();

    let _ = std::fs::remove_file(&index_path);
    result
}

/// File-level fallback: each file goes whole into the first group that
/// touches it, so coverage stays exactly-once without hunk staging.
fn collapse_to_whole_files(units: &DiffUnits, groups: &[CommitGroup]) -> Vec<LandCommit> {
    let mut assigned = HashSet::new();
    let mut commits = Vec::new();

    for group in groups {
        let ids: HashSet<String> = group.hunks.iter().cloned().collect();
        let files: Vec<String> = units
            .files_for(&ids)
            .into_iter()
            .filter(|file| assigned.insert(file.clone()))
            .collect();
        if files.is_empty() {
            continue;
        }

        commits.push(whole_files_commit(group.message.clone(), files));
    }

    commits
}

/// One commit staged as whole files; used by the verification fallback and
/// the manual path.
fn whole_files_commit(message: String, files: Vec<String>) -> LandCommit {
    LandCommit {
        message,
        files: files.iter().cloned().map(FileStat::whole).collect(),
        stage: StageOp::WholeFiles(files),
    }
}

/// Renders the proposed history as a numbered list of commits, each with a
/// small file tree underneath. Files split across commits show how many of
/// their hunks each commit takes and which parts they are.
fn render_land_plan(commits: &[LandCommit], save_count: usize) -> String {
    let mut plan = format!(
        "{} Plan: {} {} {}\n\n",
        "·".cyan(),
        pluralize(save_count, "save"),
        "→".dimmed(),
        pluralize(commits.len(), "commit"),
    );

    for (index, commit) in commits.iter().enumerate() {
        // Only the subject goes on the numbered line: a multi-line message
        // would otherwise print flush-left through the file tree and wreck the
        // one screen the user has to read before history is rewritten.
        let mut lines = commit.message.lines();
        let subject = lines.next().unwrap_or("").trim();
        plan.push_str(&format!("  {}. {}\n", index + 1, subject.bold()));

        let body_lines = lines.filter(|line| !line.trim().is_empty()).count();
        if body_lines > 0 {
            plan.push_str(&format!(
                "     {}\n",
                format!("+ {}", pluralize(body_lines, "body line")).dimmed()
            ));
        }

        for (position, file) in commit.files.iter().enumerate() {
            let last = position + 1 == commit.files.len();
            let glyph = if last { "└─" } else { "├─" };
            let mut line = file.path.clone();
            if file.selected < file.total {
                line.push_str(&format!(" ({}/{} hunks)", file.selected, file.total));
            }
            plan.push_str(&format!("     {} {}\n", glyph.dimmed(), line));

            let continuation = if last { " " } else { "│" };
            for heading in &file.headings {
                let clipped: String = heading.chars().take(72).collect();
                plan.push_str(&format!(
                    "     {}    {}\n",
                    continuation.dimmed(),
                    format!("· {clipped}").dimmed()
                ));
            }
        }
    }

    plan.push('\n');
    plan
}

fn execute_land(base: &KiteBase, commits: &[LandCommit]) -> Result<()> {
    if commits.is_empty() {
        anyhow::bail!("Refusing to land an empty plan — that would discard the saves.");
    }

    // Resolved before anything is rewritten: on a detached HEAD there is no
    // branch to point at the landed commits, and discovering that after the
    // rewrite strands the user on the recovery branch.
    let original_branch = current_branch_name()?;
    let pre_land_sha = execute_git(&["rev-parse", "HEAD"])?;
    let recovery_branch = format!(
        "{RECOVERY_BRANCH_PREFIX}{}",
        Local::now().format("%Y%m%d%H%M%S")
    );

    execute_git(&["update-ref", PRE_LAND_REF, pre_land_sha.trim()])?;
    config_set(PRE_LAND_BRANCH_KEY, &original_branch)?;
    // Cleared until the land succeeds, so a stale value can never make `kt
    // undo` think HEAD is untouched.
    config_unset(PRE_LAND_HEAD_KEY);

    if let Err(err) = prepare_landing_branch(base, &recovery_branch).and_then(|_| {
        create_commits(commits)?;
        finalize_landed_branch(&original_branch, &recovery_branch)
    }) {
        anyhow::bail!(
            "{}",
            render_land_failure(&err, &original_branch, &recovery_branch)
        );
    }

    if let Ok(landed_head) = execute_git(&["rev-parse", "HEAD"]) {
        let _ = config_set(PRE_LAND_HEAD_KEY, landed_head.trim());
    }

    Ok(())
}

fn render_land_failure(
    err: &anyhow::Error,
    original_branch: &str,
    recovery_branch: &str,
) -> String {
    let mut message = format!(
        "{err}\n\nLanding stopped before updating `{original_branch}`.\nKite kept the in-progress landing state on recovery branch `{recovery_branch}` so partial commits or staged changes are not lost."
    );

    if current_branch_name()
        .map(|branch| branch == recovery_branch)
        .unwrap_or(false)
    {
        message.push_str(&format!("\nYou are currently on `{recovery_branch}`."));
    }

    message.push_str(&format!(
        "\nFix the issue there and rerun `kt land`, or run `git switch {original_branch}` if you want to abandon this landing attempt.\nRecovery ref: `refs/kite/pre_land`."
    ));

    message
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

fn create_commits(commits: &[LandCommit]) -> Result<()> {
    for commit in commits {
        match &commit.stage {
            StageOp::Patch(patch) => apply_cached_patch(patch)?,
            StageOp::WholeFiles(files) => {
                for file in files {
                    execute_git(&["add", "--", file])?;
                }
            }
        }
        commit_git(&commit.message)?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{
        acquire_cwd_lock, git, init_repo, init_root_kite_repo, with_repo_cwd, write_file,
    };

    fn collect_land_scope_in_repo(
        repo: &std::path::Path,
        allow_dirty: bool,
    ) -> Result<Option<LandScope>> {
        with_repo_cwd(repo, || collect_land_scope(allow_dirty))
    }

    fn execute_land_in_repo(
        repo: &std::path::Path,
        base: &KiteBase,
        commits: &[LandCommit],
    ) -> Result<()> {
        with_repo_cwd(repo, || execute_land(base, commits))
    }

    fn undo_in_repo(repo: &std::path::Path) -> Result<()> {
        with_repo_cwd(repo, undo)
    }

    fn files_commit(message: &str, files: &[&str]) -> LandCommit {
        whole_files_commit(
            message.to_string(),
            files.iter().map(|file| file.to_string()).collect(),
        )
    }

    fn group(message: &str, hunks: &[&str]) -> CommitGroup {
        CommitGroup {
            message: message.to_string(),
            hunks: hunks.iter().map(|id| id.to_string()).collect(),
        }
    }

    /// Two edits far enough apart to produce two hunks in one file.
    fn save_two_hunk_change(repo: &std::path::Path) {
        write_file(
            repo,
            "code.txt",
            "alpha\nb\nc\nd\ne\nf\ng\nh\ni\nj\nk\nomega\n",
        );
        git(repo, &["add", "code.txt"]);
        git(repo, &["commit", "-m", "chore: add code"]);

        write_file(
            repo,
            "code.txt",
            "ALPHA\nb\nc\nd\ne\nf\ng\nh\ni\nj\nk\nOMEGA\n",
        );
        git(repo, &["add", "code.txt"]);
        git(repo, &["commit", "-m", "[kite] save 12:00:00"]);
    }

    #[test]
    fn render_land_plan_numbers_commits_and_marks_split_files() {
        // Pin colors off so we assert the structural layout, not ANSI codes.
        colored::control::set_override(false);

        let plan = render_land_plan(
            &[
                LandCommit {
                    message: "feat(api): add webhooks".to_string(),
                    files: vec![
                        FileStat {
                            path: "src/api.rs".to_string(),
                            selected: 1,
                            total: 2,
                            headings: vec!["fn register_webhook()".to_string()],
                        },
                        FileStat::whole("src/hooks.rs".to_string()),
                    ],
                    stage: StageOp::Patch(String::new()),
                },
                LandCommit {
                    message: "docs: refresh readme".to_string(),
                    files: vec![FileStat::whole("README.md".to_string())],
                    stage: StageOp::Patch(String::new()),
                },
            ],
            3,
        );

        assert!(plan.contains("Plan: 3 saves → 2 commits"));
        assert!(plan.contains("  1. feat(api): add webhooks\n"));
        assert!(plan.contains("     ├─ src/api.rs (1/2 hunks)\n"));
        assert!(plan.contains("     │    · fn register_webhook()\n"));
        assert!(plan.contains("     └─ src/hooks.rs\n"));
        assert!(plan.contains("  2. docs: refresh readme\n"));
        assert!(plan.contains("     └─ README.md\n"));
    }

    #[test]
    fn collect_land_scope_rejects_dirty_worktree() {
        let _lock = acquire_cwd_lock();
        let repo = init_repo();

        write_file(&repo.path, "tracked.txt", "dirty\n");

        let err = collect_land_scope_in_repo(&repo.path, false)
            .expect_err("dirty repos should fail without allow_dirty");
        assert!(format!("{err:#}").contains("Working directory must be clean"));
    }

    #[test]
    fn collect_land_scope_allows_dirty_worktree_with_allow_dirty() {
        let _lock = acquire_cwd_lock();
        let repo = init_repo();

        write_file(&repo.path, "tracked.txt", "saved change\n");
        git(&repo.path, &["add", "tracked.txt"]);
        git(&repo.path, &["commit", "-m", "[kite] save 12:00:00"]);

        write_file(&repo.path, "other.txt", "local worktree change\n");

        let scope = collect_land_scope_in_repo(&repo.path, true)
            .expect("land scope should collect when allow_dirty is true")
            .expect("kite saves should be landable");
        assert!(matches!(scope.base, KiteBase::Commit(_)));
        assert_eq!(scope.save_count, 1);
        assert!(scope.units.all_files().contains(&"tracked.txt".to_string()));
    }

    #[test]
    fn land_splits_one_file_across_commits_and_preserves_the_tree() {
        let _lock = acquire_cwd_lock();
        let repo = init_repo();
        save_two_hunk_change(&repo.path);

        let pre_land_tree = git(&repo.path, &["rev-parse", "HEAD^{tree}"]);

        let scope = collect_land_scope_in_repo(&repo.path, false)
            .expect("land scope should collect")
            .expect("kite saves should be landable");
        assert_eq!(scope.units.unit_ids(), vec!["h1", "h2"]);

        // Deliberately out of file order: the bottom hunk lands first.
        let groups = [
            group("feat: bottom change", &["h2"]),
            group("feat: top change", &["h1"]),
        ];
        let commits = with_repo_cwd(&repo.path, || {
            plan_commits(&scope.base, &scope.units, &groups)
        });
        assert!(matches!(commits[0].stage, StageOp::Patch(_)));

        execute_land_in_repo(&repo.path, &scope.base, &commits).expect("land should succeed");

        let messages = git(&repo.path, &["log", "--pretty=%s", "-n", "2"]);
        assert_eq!(
            messages.lines().collect::<Vec<_>>(),
            vec!["feat: top change", "feat: bottom change"]
        );

        // The intermediate commit holds only the bottom hunk.
        let intermediate = git(&repo.path, &["show", "HEAD^:code.txt"]);
        assert!(intermediate.contains("OMEGA"));
        assert!(intermediate.contains("alpha\n"));
        assert!(!intermediate.contains("ALPHA"));

        // The landed branch reproduces the saved tree exactly.
        let landed_tree = git(&repo.path, &["rev-parse", "HEAD^{tree}"]);
        assert_eq!(landed_tree, pre_land_tree);

        let status = git(&repo.path, &["status", "--porcelain"]);
        assert!(status.trim().is_empty(), "expected clean tree: {status}");
    }

    #[test]
    fn land_splits_edits_only_a_few_lines_apart() {
        let _lock = acquire_cwd_lock();
        let repo = init_repo();

        write_file(
            &repo.path,
            "code.txt",
            "one\ntwo\nthree\nfour\nfive\nsix\nseven\neight\nnine\nten\n",
        );
        git(&repo.path, &["add", "code.txt"]);
        git(&repo.path, &["commit", "-m", "chore: add code"]);

        // Lines 2 and 6 — close enough to merge into one hunk at the default
        // context width, but separate hunks with -U1.
        write_file(
            &repo.path,
            "code.txt",
            "one\nTWO\nthree\nfour\nfive\nSIX\nseven\neight\nnine\nten\n",
        );
        git(&repo.path, &["add", "code.txt"]);
        git(&repo.path, &["commit", "-m", "[kite] save 12:00:00"]);

        let pre_land_tree = git(&repo.path, &["rev-parse", "HEAD^{tree}"]);
        let scope = collect_land_scope_in_repo(&repo.path, false)
            .expect("land scope should collect")
            .expect("kite saves should be landable");
        assert_eq!(scope.units.unit_ids(), vec!["h1", "h2"]);

        let groups = [
            group("feat: upcase two", &["h1"]),
            group("feat: upcase six", &["h2"]),
        ];
        let commits = with_repo_cwd(&repo.path, || {
            plan_commits(&scope.base, &scope.units, &groups)
        });
        assert!(matches!(commits[0].stage, StageOp::Patch(_)));

        execute_land_in_repo(&repo.path, &scope.base, &commits).expect("land should succeed");

        let intermediate = git(&repo.path, &["show", "HEAD^:code.txt"]);
        assert!(intermediate.contains("TWO") && intermediate.contains("six\n"));

        let landed_tree = git(&repo.path, &["rev-parse", "HEAD^{tree}"]);
        assert_eq!(landed_tree, pre_land_tree);
    }

    #[test]
    fn land_splits_hunks_inside_a_crlf_file() {
        let _lock = acquire_cwd_lock();
        let repo = init_repo();
        // Keep git from normalizing line endings, so the blobs really hold
        // CRLF the way they do in a Windows-authored repository.
        git(&repo.path, &["config", "core.autocrlf", "false"]);
        write_file(&repo.path, ".gitattributes", "* -text\n");
        git(&repo.path, &["add", ".gitattributes"]);
        git(&repo.path, &["commit", "-m", "chore: pin line endings"]);

        write_file(
            &repo.path,
            "code.txt",
            "alpha\r\nb\r\nc\r\nd\r\ne\r\nf\r\ng\r\nh\r\ni\r\nj\r\nk\r\nomega\r\n",
        );
        git(&repo.path, &["add", "code.txt"]);
        git(&repo.path, &["commit", "-m", "chore: add code"]);

        write_file(
            &repo.path,
            "code.txt",
            "ALPHA\r\nb\r\nc\r\nd\r\ne\r\nf\r\ng\r\nh\r\ni\r\nj\r\nk\r\nOMEGA\r\n",
        );
        git(&repo.path, &["add", "code.txt"]);
        git(&repo.path, &["commit", "-m", "[kite] save 12:00:00"]);

        let pre_land_tree = git(&repo.path, &["rev-parse", "HEAD^{tree}"]);
        let scope = collect_land_scope_in_repo(&repo.path, false)
            .expect("land scope should collect")
            .expect("kite saves should be landable");
        assert_eq!(scope.units.unit_ids(), vec!["h1", "h2"]);

        let groups = [
            group("feat: upcase alpha", &["h1"]),
            group("feat: upcase omega", &["h2"]),
        ];
        let commits = with_repo_cwd(&repo.path, || {
            plan_commits(&scope.base, &scope.units, &groups)
        });

        // The whole point: CRLF content must still stage hunk by hunk rather
        // than silently degrading to whole-file commits.
        assert!(
            matches!(commits[0].stage, StageOp::Patch(_)),
            "CRLF file fell back to whole-file staging: {:?}",
            commits[0].stage
        );

        execute_land_in_repo(&repo.path, &scope.base, &commits).expect("land should succeed");

        let intermediate = git(&repo.path, &["show", "HEAD^:code.txt"]);
        assert!(intermediate.contains("ALPHA\r\n"));
        assert!(intermediate.contains("omega\r\n"));

        assert_eq!(
            git(&repo.path, &["rev-parse", "HEAD^{tree}"]),
            pre_land_tree
        );
    }

    #[test]
    fn render_land_plan_keeps_multi_line_messages_out_of_the_file_tree() {
        colored::control::set_override(false);

        let plan = render_land_plan(
            &[LandCommit {
                message: "feat(api): add webhooks\n\nExplains the change\nover several lines."
                    .to_string(),
                files: vec![FileStat::whole("src/api.rs".to_string())],
                stage: StageOp::Patch(String::new()),
            }],
            1,
        );

        assert!(plan.contains("  1. feat(api): add webhooks\n"));
        assert!(plan.contains("+ 2 body lines"));
        assert!(!plan.contains("\nExplains the change"));
        assert!(plan.contains("     └─ src/api.rs\n"));
    }

    #[test]
    fn plan_commits_falls_back_to_whole_files_when_hunks_are_missing() {
        let _lock = acquire_cwd_lock();
        let repo = init_repo();
        save_two_hunk_change(&repo.path);

        let scope = collect_land_scope_in_repo(&repo.path, false)
            .expect("land scope should collect")
            .expect("kite saves should be landable");

        // A plan that drops h2 cannot reproduce the saved tree, so planning
        // must degrade to whole-file staging instead of losing the change.
        let groups = [group("feat: partial", &["h1"])];
        let commits = with_repo_cwd(&repo.path, || {
            plan_commits(&scope.base, &scope.units, &groups)
        });

        assert_eq!(commits.len(), 1);
        assert!(
            matches!(&commits[0].stage, StageOp::WholeFiles(files) if files == &vec!["code.txt".to_string()])
        );

        execute_land_in_repo(&repo.path, &scope.base, &commits).expect("land should succeed");
        let content = git(&repo.path, &["show", "HEAD:code.txt"]);
        assert!(content.contains("ALPHA") && content.contains("OMEGA"));
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

        let scope = collect_land_scope_in_repo(&repo.path, false)
            .expect("land scope should collect")
            .expect("kite saves should be landable");
        assert!(matches!(scope.base, KiteBase::Commit(_)));

        execute_land_in_repo(
            &repo.path,
            &scope.base,
            &[files_commit("feat: land tracked change", &["tracked.txt"])],
        )
        .expect("land should succeed");

        let head_message = git(&repo.path, &["log", "-1", "--pretty=%s"]);
        assert_eq!(head_message.trim(), "feat: land tracked change");

        let recorded_pre_land = git(&repo.path, &["rev-parse", "refs/kite/pre_land"]);
        assert_eq!(recorded_pre_land.trim(), pre_land_sha);

        let current_branch = git(&repo.path, &["rev-parse", "--abbrev-ref", "HEAD"]);
        assert_eq!(current_branch.trim(), original_branch);

        let recovery_branches = git(&repo.path, &["branch", "--list", "kite-recovery-*"]);
        assert!(recovery_branches.trim().is_empty());
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

        let scope = collect_land_scope_in_repo(&repo.path, false)
            .expect("land scope should collect")
            .expect("kite saves should be landable");

        execute_land_in_repo(
            &repo.path,
            &scope.base,
            &[files_commit("feat: land tracked change", &["tracked.txt"])],
        )
        .expect("land should succeed");

        undo_in_repo(&repo.path).expect("undo should succeed");

        let restored_head = git(&repo.path, &["rev-parse", "HEAD"]);
        assert_eq!(restored_head.trim(), pre_land_sha);

        let status = git(&repo.path, &["status", "--porcelain"]);
        assert!(status.trim().is_empty());
    }

    #[test]
    fn undo_refuses_when_the_land_belongs_to_another_branch() {
        let _lock = acquire_cwd_lock();
        let repo = init_repo();
        let default_branch = git(&repo.path, &["rev-parse", "--abbrev-ref", "HEAD"]);
        let default_branch = default_branch.trim().to_string();

        git(&repo.path, &["checkout", "-q", "-b", "feature-a"]);
        write_file(&repo.path, "tracked.txt", "saved change\n");
        git(&repo.path, &["add", "tracked.txt"]);
        git(&repo.path, &["commit", "-m", "[kite] save 12:00:00"]);

        let scope = collect_land_scope_in_repo(&repo.path, false)
            .expect("land scope should collect")
            .expect("kite saves should be landable");
        execute_land_in_repo(
            &repo.path,
            &scope.base,
            &[files_commit("feat: land tracked change", &["tracked.txt"])],
        )
        .expect("land should succeed");

        // Move to an unrelated branch and commit there.
        git(&repo.path, &["checkout", "-q", &default_branch]);
        write_file(&repo.path, "elsewhere.txt", "unrelated work\n");
        git(&repo.path, &["add", "elsewhere.txt"]);
        git(&repo.path, &["commit", "-m", "feat: unrelated work"]);
        let protected_head = git(&repo.path, &["rev-parse", "HEAD"]);

        let err = undo_in_repo(&repo.path).expect_err("undo should refuse on another branch");
        assert!(format!("{err:#}").contains("The last land was on `feature-a`"));

        // Crucially, nothing moved.
        let head_after = git(&repo.path, &["rev-parse", "HEAD"]);
        assert_eq!(head_after, protected_head);
        assert!(repo.path.join("elsewhere.txt").exists());
        assert!(
            check_ref_in(&repo.path, PRE_LAND_REF).is_some(),
            "a refused undo must keep the rollback marker"
        );
    }

    #[test]
    fn undo_on_the_landed_branch_still_restores_the_saves() {
        let _lock = acquire_cwd_lock();
        let repo = init_repo();

        write_file(&repo.path, "tracked.txt", "saved change\n");
        git(&repo.path, &["add", "tracked.txt"]);
        git(&repo.path, &["commit", "-m", "[kite] save 12:00:00"]);
        let pre_land_sha = git(&repo.path, &["rev-parse", "HEAD"]);

        let scope = collect_land_scope_in_repo(&repo.path, false)
            .expect("land scope should collect")
            .expect("kite saves should be landable");
        execute_land_in_repo(
            &repo.path,
            &scope.base,
            &[files_commit("feat: land tracked change", &["tracked.txt"])],
        )
        .expect("land should succeed");

        undo_in_repo(&repo.path).expect("undo should succeed on the landed branch");

        assert_eq!(
            git(&repo.path, &["rev-parse", "HEAD"]).trim(),
            pre_land_sha.trim()
        );
        assert!(
            check_ref_in(&repo.path, PRE_LAND_REF).is_none(),
            "a successful undo consumes the marker"
        );
    }

    #[test]
    fn land_refuses_on_a_detached_head_before_touching_history() {
        let _lock = acquire_cwd_lock();
        let repo = init_repo();

        write_file(&repo.path, "tracked.txt", "saved change\n");
        git(&repo.path, &["add", "tracked.txt"]);
        git(&repo.path, &["commit", "-m", "[kite] save 12:00:00"]);
        let head_before = git(&repo.path, &["rev-parse", "HEAD"]);

        git(&repo.path, &["checkout", "-q", "--detach"]);

        let err = with_repo_cwd(&repo.path, land_preflight)
            .expect_err("a detached HEAD should be refused up front");
        assert!(format!("{err:#}").contains("HEAD is detached"));

        assert_eq!(git(&repo.path, &["rev-parse", "HEAD"]), head_before);
        let recovery = git(&repo.path, &["branch", "--list", "kite-recovery-*"]);
        assert!(
            recovery.trim().is_empty(),
            "nothing should have been rewritten"
        );
    }

    #[test]
    fn land_preflight_refuses_an_unresolved_merge() {
        let _lock = acquire_cwd_lock();
        let repo = init_repo();

        git(&repo.path, &["checkout", "-q", "-b", "side"]);
        write_file(&repo.path, "tracked.txt", "side\n");
        git(&repo.path, &["commit", "-qam", "feat: side"]);
        git(&repo.path, &["checkout", "-q", "-"]);
        write_file(&repo.path, "tracked.txt", "main\n");
        git(&repo.path, &["commit", "-qam", "feat: main"]);
        let _ = std::process::Command::new("git")
            .args(["merge", "side"])
            .current_dir(&repo.path)
            .output();

        let err = with_repo_cwd(&repo.path, land_preflight)
            .expect_err("a conflicted repo should be refused");
        assert!(format!("{err:#}").contains("unresolved merge conflicts"));
    }

    fn check_ref_in(repo: &std::path::Path, name: &str) -> Option<String> {
        with_repo_cwd(repo, || check_ref(name))
    }

    #[test]
    fn execute_land_supports_root_only_kite_history() {
        let _lock = acquire_cwd_lock();
        let repo = init_root_kite_repo();

        let original_branch = git(&repo.path, &["rev-parse", "--abbrev-ref", "HEAD"]);
        let original_branch = original_branch.trim().to_string();
        let pre_land_sha = git(&repo.path, &["rev-parse", "HEAD"]);
        let pre_land_sha = pre_land_sha.trim().to_string();

        let scope = collect_land_scope_in_repo(&repo.path, false)
            .expect("land scope should collect")
            .expect("root kite save should be landable");
        assert!(matches!(scope.base, KiteBase::Root));

        let groups = [group("feat: bootstrap project", &["h1"])];
        let commits = with_repo_cwd(&repo.path, || {
            plan_commits(&scope.base, &scope.units, &groups)
        });
        assert!(matches!(commits[0].stage, StageOp::Patch(_)));

        execute_land_in_repo(&repo.path, &scope.base, &commits).expect("root land should succeed");

        let head_message = git(&repo.path, &["log", "-1", "--pretty=%s"]);
        assert_eq!(head_message.trim(), "feat: bootstrap project");

        let recorded_pre_land = git(&repo.path, &["rev-parse", "refs/kite/pre_land"]);
        assert_eq!(recorded_pre_land.trim(), pre_land_sha);

        let current_branch = git(&repo.path, &["rev-parse", "--abbrev-ref", "HEAD"]);
        assert_eq!(current_branch.trim(), original_branch);
    }

    #[test]
    fn execute_land_succeeds_from_a_nested_directory() {
        let _lock = acquire_cwd_lock();
        let repo = init_repo();
        let nested = repo.path.join("nested");

        std::fs::create_dir_all(&nested).expect("nested directory should exist");
        write_file(&repo.path, "nested/feature.txt", "saved change\n");
        git(&repo.path, &["add", "nested/feature.txt"]);
        git(&repo.path, &["commit", "-m", "[kite] save 12:00:00"]);

        let scope = with_repo_cwd(&nested, || collect_land_scope(false))
            .expect("land scope should collect")
            .expect("kite saves should be landable");

        with_repo_cwd(&nested, || {
            execute_land(
                &scope.base,
                &[files_commit(
                    "feat: land nested change",
                    &["nested/feature.txt"],
                )],
            )
        })
        .expect("land should succeed from a nested directory");

        let head_message = git(&repo.path, &["log", "-1", "--pretty=%s"]);
        assert_eq!(head_message.trim(), "feat: land nested change");
    }

    #[test]
    fn execute_land_failure_explains_recovery_branch_usage() {
        let _lock = acquire_cwd_lock();
        let repo = init_repo();
        let original_branch = git(&repo.path, &["rev-parse", "--abbrev-ref", "HEAD"]);
        let original_branch = original_branch.trim().to_string();

        write_file(&repo.path, "tracked.txt", "saved change\n");
        git(&repo.path, &["add", "tracked.txt"]);
        git(&repo.path, &["commit", "-m", "[kite] save 12:00:00"]);

        let scope = collect_land_scope_in_repo(&repo.path, false)
            .expect("land scope should collect")
            .expect("kite saves should be landable");

        let err = execute_land_in_repo(
            &repo.path,
            &scope.base,
            &[files_commit("feat: land tracked change", &["missing.txt"])],
        )
        .expect_err("land should fail when a grouped file cannot be staged");

        let rendered = format!("{err:#}");
        assert!(rendered.contains(&format!(
            "Landing stopped before updating `{original_branch}`"
        )));
        assert!(rendered.contains("recovery branch `kite-recovery-"));
        assert!(rendered.contains("Fix the issue there and rerun `kt land`"));

        let current_branch = git(&repo.path, &["rev-parse", "--abbrev-ref", "HEAD"]);
        assert!(current_branch.trim().starts_with("kite-recovery-"));
    }
}
