use anyhow::{Context, Result, anyhow};
use chrono::Local;
use colored::*;
use std::collections::HashSet;

use crate::ai::flatten_error;
use crate::git::{
    DETACHED_TARGET, Head, KiteBase, apply_cached_patch, branch_to_publish, check_ref, commit_git,
    config_get, config_set, config_unset, diff_for_base, execute_git, execute_git_with,
    has_head_commit, has_remote, has_unmerged_paths, head_branch_hint, head_position, is_ancestor,
    is_save_subject, kite_save_stack, short_sha, subjects_missing_from_head,
};
use crate::hunks::{DiffUnits, FileStat, parse_diff};
use crate::synth::{
    CommitGroup, MAX_DIFF_BYTES, normalize_groups, sanitize_commit_message, synthesize_groups,
};
use crate::ui::{Spinner, confirm, pluralize, print_ai_unavailable, prompt_line};

/// Where the pre-land `HEAD` is parked so `kt undo` can restore it, plus the
/// local config keys recording which branch that land belongs to — or
/// `DETACHED_TARGET` when it was landed on a detached `HEAD` — and where it
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
    let Some(status) = land_preflight(push)? else {
        return Ok(());
    };

    let stashed = if allow_dirty {
        stash_dirty_worktree_for_land()?
    } else {
        false
    };

    let land_result = (async {
        // The preflight status predates the stash, so it only describes the
        // tree the clean-worktree check cares about.
        let Some(scope) = collect_land_scope(allow_dirty, Some(&status))? else {
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
/// A bare `--force-with-lease` is not enough on its own. The lease compares
/// against the local remote-tracking ref, and when that ref does not exist —
/// the usual case for a branch someone else created — git has nothing to
/// compare and lets the push through, silently destroying their work. So the
/// lease is always given an explicit expected sha, and the one case with no
/// sha to give is the one case worth a round trip to resolve.
pub(crate) fn publish_current_branch() -> Result<()> {
    if !has_remote() {
        println!("{} No remote — history stays local", "·".dimmed());
        return Ok(());
    }

    let branch = branch_to_publish()?;
    let remote_ref = format!("refs/remotes/origin/{branch}");

    // No fetch on the common path. The lease below is checked by git against
    // the remote's real state at push time, so a tracking ref that has gone
    // stale causes a safe rejection, never a clobber. Only when there is no
    // tracking ref at all — nothing to lease against, which is precisely the
    // dangerous case — is it worth a round trip to find out what is there.
    let remote_sha = check_ref(&remote_ref).or_else(|| {
        let spinner = Spinner::start(format!("Checking origin/{branch}"));
        let _ = execute_git(&["fetch", "origin", &branch]);
        spinner.stop();
        check_ref(&remote_ref)
    });

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

    pushed.map_err(|error| {
        // A rejected lease has one cause and one fix, and git's four lines of
        // remote URLs and ref arrows only bury them. Everything else — auth,
        // a missing remote, a hook — still needs git's own words.
        let detail = flatten_error(&format!("{error:#}"));
        if ["stale info", "fetch first", "non-fast-forward"]
            .iter()
            .any(|marker| detail.contains(marker))
        {
            anyhow!(
                "Push rejected — `origin/{branch}` has moved since you last fetched it. Run `git fetch origin {branch}` to see what changed, then rerun `kt publish`."
            )
        } else {
            error.context(format!("Could not publish `{branch}`"))
        }
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

/// Reverses the most recent thing Kite did on this branch.
///
/// A quicksave sitting on top of history is by definition more recent than any
/// land beneath it, so that goes first; otherwise this undoes the last land.
/// Running it repeatedly walks back through saves and then the land, which is
/// the order they happened in.
pub(crate) fn undo() -> Result<()> {
    if !has_head_commit() {
        println!("{} Nothing to undo — no commits yet", "·".yellow());
        return Ok(());
    }

    let head_subject = execute_git(&["log", "-1", "--pretty=%s"]).unwrap_or_default();
    if is_save_subject(&head_subject) {
        return undo_last_save(head_subject.trim());
    }

    undo_last_land()
}

/// Uncommits the quicksave on top of history, putting its changes back in the
/// working tree. A *mixed* reset, so the result is the state the user was in
/// before they ran `kt` — and it never touches the working tree, so edits made
/// since the save survive and nothing has to be clean first.
fn undo_last_save(subject: &str) -> Result<()> {
    match check_ref("HEAD~1") {
        Some(parent) => execute_git(&["reset", "--mixed", &parent]).map(|_| ())?,
        None => {
            // The save is the repository's very first commit: there is no
            // parent to reset onto, so make the branch unborn again. The index
            // and working tree are left exactly as they are.
            match head_position()? {
                Head::Branch(branch) => {
                    execute_git(&["update-ref", "-d", &format!("refs/heads/{branch}")])?;
                }
                // Only a branch can be unborn. A detached `HEAD` has to point
                // at some commit, and deleting the save would leave nowhere for
                // it to point.
                Head::Detached(sha) => anyhow::bail!(
                    "{} is the first commit in this repository and HEAD is detached, so there is nothing to move it back to. Run `git switch -c <name>` first, then `kt undo`.",
                    short_sha(&sha)
                ),
            }
        }
    }

    println!(
        "{} Undid {} {}",
        "✓".green(),
        subject.bold(),
        "— your changes are back in the working tree".dimmed()
    );
    Ok(())
}

fn undo_last_land() -> Result<()> {
    let Some(pre_land_sha) = check_ref(PRE_LAND_REF) else {
        println!(
            "{} {}",
            "·".yellow(),
            "Nothing to undo — no quicksave on top and no land recorded".dimmed()
        );
        return Ok(());
    };

    let head = head_position()?;
    guard_undo_target(&head)?;

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

    match &head {
        Head::Branch(branch) if has_remote() => {
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
        // Nothing to revert: a detached land was never publishable, so the
        // remote cannot be holding the history it produced.
        Head::Detached(_) if has_remote() => println!(
            "{} {}",
            "·".dimmed(),
            "Remote untouched — a detached HEAD has no branch to revert".dimmed()
        ),
        _ => {}
    }

    println!("{} Restored pre-land saves", "✓".green());
    Ok(())
}

/// `refs/kite/pre_land` is a single repo-wide ref, so on its own it says
/// nothing about where it belongs. Without this guard, landing in one place and
/// undoing in another hard-resets — and force-pushes — the wrong branch to an
/// unrelated commit.
fn guard_undo_target(head: &Head) -> Result<()> {
    let Some(landed_target) = config_get(PRE_LAND_BRANCH_KEY) else {
        // Recorded by a Kite that predates this guard: we cannot tell where it
        // belongs, so make the user look before it resets.
        if confirm(&format!(
            "The recorded land has no branch attached. Reset {} to it anyway?",
            head.describe()
        ))? {
            return Ok(());
        }
        anyhow::bail!("Undo cancelled — nothing changed");
    };

    if landed_target != head.land_key() {
        anyhow::bail!(
            "The last land was on {}, but you are on {}. {} — undoing here would reset unrelated history.",
            describe_landed_target(&landed_target),
            head.describe(),
            recover_landed_target(&landed_target)
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
        "{} has moved since that land. Undo will discard {} made since. Continue?",
        head.describe(),
        pluralize(added, "commit")
    ))? {
        return Ok(());
    }
    anyhow::bail!("Undo cancelled — nothing changed")
}

/// A recorded land target read back from config, which is a branch name or
/// `DETACHED_TARGET` — never a `Head`, because the commit a detached land
/// started from is not what the marker keeps.
fn describe_landed_target(landed_target: &str) -> String {
    if landed_target == DETACHED_TARGET {
        "a detached HEAD".to_string()
    } else {
        format!("`{landed_target}`")
    }
}

/// How to get back to where a land happened. For a detached land that is the
/// commit it left `HEAD` on, which the marker records; without it there is no
/// name to offer, so the instruction stays honest about that.
fn recover_landed_target(landed_target: &str) -> String {
    if landed_target != DETACHED_TARGET {
        return format!("Run `git switch {landed_target}` first");
    }

    match config_get(PRE_LAND_HEAD_KEY) {
        Some(landed_head) => format!(
            "Check that commit out again with `git switch --detach {}`",
            short_sha(&landed_head)
        ),
        None => "Check out the commit that land left behind first".to_string(),
    }
}

const LANDING_BRANCH_PREFIX: &str = "kite-landing-";
/// Branches left by versions of Kite that parked failed landings on one.
const LEGACY_RECOVERY_PREFIX: &str = "kite-recovery-";

fn is_landing_branch(branch: &str) -> bool {
    branch.starts_with(LANDING_BRANCH_PREFIX) || branch.starts_with(LEGACY_RECOVERY_PREFIX)
}

/// Cleans up after a land that never finished — Ctrl-C, a crash, a closed
/// terminal — which is the only way HEAD can still be parked mid-rewrite.
///
/// Runs before every command. Nothing was lost: the branch still holds every
/// save, because landing only moves it as its final step. So this puts the
/// user back rather than making them work out what happened and undo it by
/// hand.
pub(crate) fn heal_interrupted_land() {
    // Free — a file read, no subprocess. On an ordinary branch there is
    // nothing to heal, which is every normal invocation, so the quicksave
    // path pays nothing for this.
    let parked_branch = match head_branch_hint() {
        Some(branch) if is_landing_branch(&branch) => Some(branch),
        Some(_) => return,
        None => None, // detached: mid-land, or the user's own checkout
    };

    // A land in progress has recorded its target but not the head it
    // produced. Any other combination means no land is half-done.
    let (Some(pre_land_sha), Some(landed_target), None) = (
        check_ref(PRE_LAND_REF),
        config_get(PRE_LAND_BRANCH_KEY),
        config_get(PRE_LAND_HEAD_KEY),
    ) else {
        return;
    };

    println!(
        "{} Cleaning up a `kt land` that was interrupted",
        "·".yellow()
    );

    // The pre-land sha is exactly where a detached land started, so it is the
    // commit `HEAD` has to go back to.
    let target = if landed_target == DETACHED_TARGET {
        Head::Detached(pre_land_sha.clone())
    } else {
        Head::Branch(landed_target)
    };

    let landing_branch = parked_branch.unwrap_or_default();
    match restore_head(&target, &pre_land_sha, &landing_branch) {
        Ok(()) => {
            // The land never happened, so stop the marker looking in-progress.
            let _ = config_set(PRE_LAND_HEAD_KEY, &pre_land_sha);
            println!(
                "  {}",
                format!(
                    "Back on {} with every save intact — nothing was landed.",
                    target.describe()
                )
                .dimmed()
            );
        }
        Err(error) => println!(
            "  {}",
            format!(
                "Could not do it automatically ({}). Run `{}` — it still has every save.",
                flatten_error(&format!("{error:#}")),
                target.switch_command()
            )
            .dimmed()
        ),
    }
}

/// Everything that must hold before Kite touches the worktree. Run before
/// `--allow-dirty` stashes anything, so a repository that cannot be landed
/// never gets its work put away first and its diagnosis second.
fn land_preflight(push: bool) -> Result<Option<String>> {
    if !has_head_commit() {
        println!(
            "{} No commits yet — make an initial commit before landing",
            "·".yellow()
        );
        return Ok(None);
    }

    // Landing itself works on a detached HEAD — it moves HEAD onto the landed
    // commits — but publishing needs a branch name. Checked here so it reports
    // before any work is done, rather than after the rewrite has happened and
    // there is nothing left to do but the push that cannot run.
    if push {
        branch_to_publish()?;
    }

    let status = execute_git(&["status", "--porcelain"])?;
    if has_unmerged_paths(&status) {
        anyhow::bail!(
            "This repository has unresolved merge conflicts. Resolve them and commit the merge before landing."
        );
    }

    Ok(Some(status))
}

/// `land_preflight` has already run when this is reached from `land`; the
/// `status` it read is passed along so a large worktree is not walked twice.
fn collect_land_scope(allow_dirty: bool, status: Option<&str>) -> Result<Option<LandScope>> {
    if !has_head_commit() {
        println!(
            "{} No commits yet — make an initial commit before landing",
            "·".yellow()
        );
        return Ok(None);
    }

    if !allow_dirty {
        let owned;
        let status = match status {
            Some(status) => status,
            None => {
                owned = execute_git(&["status", "--porcelain"])?;
                &owned
            }
        };
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

    // Only when the hunks are too many *and* too large to show: the model
    // would otherwise be assigning ids whose contents it never saw. Hundreds
    // of small hunks still fit whole and keep their hunk-level split.
    if units.exceeds_prompt_budget(MAX_DIFF_BYTES) {
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

    // Resolved before anything is rewritten, so the landed commits have a
    // recorded home: a branch to move, or the detached HEAD to leave sitting on
    // them. Discovering it afterwards would mean unwinding a rewrite that had
    // already happened.
    let target = head_position()?;
    let pre_land_sha = execute_git(&["rev-parse", "HEAD"])?;
    // Only ever used by a root rewrite, which is the one case that cannot build
    // its commits on a detached HEAD.
    let landing_branch = format!(
        "{LANDING_BRANCH_PREFIX}{}",
        Local::now().format("%Y%m%d%H%M%S")
    );

    // Kept so a land that fails cannot overwrite the rollback marker left by
    // the last one that succeeded.
    let previous_marker = PreLandMarker::capture();

    execute_git(&["update-ref", PRE_LAND_REF, pre_land_sha.trim()])?;
    config_set(PRE_LAND_BRANCH_KEY, &target.land_key())?;
    // Cleared until the land succeeds, so a stale value can never make `kt
    // undo` think HEAD is untouched.
    config_unset(PRE_LAND_HEAD_KEY);

    if let Err(err) = prepare_landing_head(base, &landing_branch).and_then(|landing| {
        create_commits(commits)?;
        finalize_landed_head(&target, &landing)
    }) {
        // A failed land is almost always a hook rejecting a commit, and the
        // fix belongs where the user already was. Nothing they wrote is at
        // stake — their saves are still reachable from `target`, which is not
        // moved until the final step, and landing stages through the index
        // without ever writing the worktree — so put them back instead of
        // stranding them on a pile of derived commits.
        return Err(
            match restore_head(&target, pre_land_sha.trim(), &landing_branch) {
                Ok(()) => {
                    previous_marker.restore();
                    anyhow!("{}", render_recovered_failure(&err, &target))
                }
                Err(restore_error) => {
                    anyhow!("{}", render_stranded_failure(&err, &target, &restore_error))
                }
            },
        );
    }

    if let Ok(landed_head) = execute_git(&["rev-parse", "HEAD"]) {
        let _ = config_set(PRE_LAND_HEAD_KEY, landed_head.trim());
    }

    Ok(())
}

/// Undoes a failed or interrupted landing attempt, leaving the user exactly
/// where they started.
///
/// Safe by construction: a branch target is only moved by the final step, so
/// while landing is in progress it still points at every save, and the pre-land
/// sha holds them for a detached one. A *mixed* reset is deliberate — it clears
/// the half-staged index but leaves the worktree alone, so files a pre-commit
/// hook rewrote (a formatter, say) survive.
fn restore_head(target: &Head, pre_land_sha: &str, landing_branch: &str) -> Result<()> {
    execute_git(&["reset", "--mixed", pre_land_sha])?;

    match target {
        Head::Branch(branch) => {
            execute_git(&["checkout", branch])?;
        }
        // A root rewrite parks HEAD on an orphan branch even when the user was
        // detached, so getting back means detaching again. Asked of git rather
        // than read from `.git/HEAD`: this only ever runs while recovering, so
        // the subprocess is free, and a layout the fast path cannot read would
        // otherwise leave HEAD on the temporary branch. The reset above already
        // made the index and worktree match, so this moves HEAD alone.
        Head::Detached(_) if matches!(head_position(), Ok(Head::Branch(_))) => {
            execute_git(&["checkout", "--detach", pre_land_sha])?;
        }
        // A detached land builds its commits detached, so the reset was enough.
        Head::Detached(_) => {}
    }

    // Exists only after a root rewrite, and only ever held commits the next
    // attempt rebuilds from scratch.
    if !landing_branch.is_empty() {
        let _ = execute_git(&["branch", "-D", landing_branch]);
    }
    Ok(())
}

fn render_recovered_failure(err: &anyhow::Error, target: &Head) -> String {
    format!(
        "{err:#}\n\nNothing changed — you are still on {} with every save intact.\nFix the problem above, then run `kt land` again.",
        target.describe()
    )
}

/// The cleanup itself failed, which is the only case left where the user has
/// to be told about the temporary branch at all.
fn render_stranded_failure(
    err: &anyhow::Error,
    target: &Head,
    restore_error: &anyhow::Error,
) -> String {
    let (safety, recovery) = match target {
        Head::Branch(branch) => (
            format!("`{branch}` still has every save and was never changed."),
            format!("Run `git reset --mixed {branch}` then `git switch {branch}` to get back."),
        ),
        Head::Detached(sha) => (
            format!("Every save is still reachable from {sha} and was never changed."),
            format!("Run `git reset --mixed {sha}` then `git switch --detach {sha}` to get back."),
        ),
    };

    format!(
        "{err:#}\n\nKite could not put you back on {}: {restore_error:#}\n{safety}\n{recovery}\nRollback ref: `{PRE_LAND_REF}`.",
        target.describe()
    )
}

/// The rollback marker as it stood before a land began.
struct PreLandMarker {
    sha: Option<String>,
    branch: Option<String>,
    head: Option<String>,
}

impl PreLandMarker {
    fn capture() -> Self {
        Self {
            sha: check_ref(PRE_LAND_REF),
            branch: config_get(PRE_LAND_BRANCH_KEY),
            head: config_get(PRE_LAND_HEAD_KEY),
        }
    }

    /// Puts the marker back after a failed land, so `kt undo` still refers to
    /// the last land that actually happened rather than one that did not.
    fn restore(&self) {
        match &self.sha {
            Some(sha) => {
                let _ = execute_git(&["update-ref", PRE_LAND_REF, sha]);
            }
            None => {
                let _ = execute_git(&["update-ref", "-d", PRE_LAND_REF]);
            }
        }
        restore_config(PRE_LAND_BRANCH_KEY, self.branch.as_deref());
        restore_config(PRE_LAND_HEAD_KEY, self.head.as_deref());
    }
}

fn restore_config(key: &str, value: Option<&str>) {
    match value {
        Some(value) => {
            let _ = config_set(key, value);
        }
        None => config_unset(key),
    }
}

/// Where the landed commits get built before `HEAD` settles onto them.
///
/// Detached wherever possible, so an ordinary land creates no branch at all
/// and there is nothing left behind to clean up if it stops early. Rewriting
/// from the root needs an *unborn* HEAD, and `checkout --orphan <name>` is the
/// only way git offers to get one — so that one case still names a branch, even
/// when the user was detached to begin with.
#[derive(Clone, Debug)]
enum LandingHead {
    Detached,
    Orphan(String),
}

fn prepare_landing_head(base: &KiteBase, orphan_branch: &str) -> Result<LandingHead> {
    match base {
        KiteBase::Commit(base_sha) => {
            execute_git(&["checkout", "--detach"])?;
            execute_git(&["reset", "--soft", base_sha])?;
            execute_git(&["reset"])?;
            Ok(LandingHead::Detached)
        }
        KiteBase::Root => {
            execute_git(&["checkout", "--orphan", orphan_branch, "HEAD"])?;
            execute_git(&["read-tree", "--empty"])?;
            Ok(LandingHead::Orphan(orphan_branch.to_string()))
        }
    }
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

fn finalize_landed_head(target: &Head, landing: &LandingHead) -> Result<()> {
    let new_head = execute_git(&["rev-parse", "HEAD"])?;

    match target {
        Head::Branch(branch) => {
            execute_git(&["branch", "-f", branch, new_head.trim()])?;
            execute_git(&["checkout", branch])?;
        }
        // A detached land is already sitting on the landed commits, so there is
        // nothing to move — except after a root rewrite, where the commits were
        // built on the orphan branch and HEAD has to come off it before that
        // branch is deleted.
        Head::Detached(_) => {
            if matches!(landing, LandingHead::Orphan(_)) {
                execute_git(&["checkout", "--detach", new_head.trim()])?;
            }
        }
    }

    if let LandingHead::Orphan(branch) = landing {
        let _ = execute_git(&["branch", "-D", branch]);
    }
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
        with_repo_cwd(repo, || collect_land_scope(allow_dirty, None))
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
    fn undo_uncommits_the_last_quicksave_and_restores_the_pre_save_state() {
        let _lock = acquire_cwd_lock();
        let repo = init_repo();

        write_file(&repo.path, "tracked.txt", "my work\n");
        write_file(&repo.path, "brand-new.txt", "new file\n");
        let before = git(&repo.path, &["status", "--porcelain"]);
        let head_before = git(&repo.path, &["rev-parse", "HEAD"]);

        // What `kt` does: stage everything, commit with the save prefix.
        git(&repo.path, &["add", "-A"]);
        git(&repo.path, &["commit", "-m", "[kite] save 12:00:00"]);
        assert!(
            git(&repo.path, &["status", "--porcelain"])
                .trim()
                .is_empty()
        );

        undo_in_repo(&repo.path).expect("undo should uncommit the save");

        assert_eq!(git(&repo.path, &["rev-parse", "HEAD"]), head_before);
        // Exactly the state the user was in before running `kt`, down to the
        // new file being untracked again.
        assert_eq!(git(&repo.path, &["status", "--porcelain"]), before);
        assert_eq!(
            std::fs::read_to_string(repo.path.join("tracked.txt")).unwrap(),
            "my work\n"
        );
        assert_eq!(
            std::fs::read_to_string(repo.path.join("brand-new.txt")).unwrap(),
            "new file\n"
        );
    }

    #[test]
    fn undo_of_a_quicksave_keeps_edits_made_since_and_needs_no_clean_tree() {
        let _lock = acquire_cwd_lock();
        let repo = init_repo();

        write_file(&repo.path, "tracked.txt", "saved\n");
        git(&repo.path, &["add", "tracked.txt"]);
        git(&repo.path, &["commit", "-m", "[kite] save 12:00:00"]);

        // Kept working after the save — undo must not require a clean tree,
        // and must not roll the newer edit back.
        write_file(&repo.path, "tracked.txt", "newer than the save\n");

        undo_in_repo(&repo.path).expect("undo should work with a dirty tree");

        assert_eq!(
            std::fs::read_to_string(repo.path.join("tracked.txt")).unwrap(),
            "newer than the save\n"
        );
        assert_eq!(
            git(&repo.path, &["log", "-1", "--pretty=%s"]).trim(),
            "chore: initial"
        );
    }

    #[test]
    fn undo_takes_the_quicksave_before_the_land_beneath_it() {
        let _lock = acquire_cwd_lock();
        let repo = init_repo();

        write_file(&repo.path, "tracked.txt", "first\n");
        git(&repo.path, &["add", "tracked.txt"]);
        git(&repo.path, &["commit", "-m", "[kite] save 12:00:00"]);
        let scope = collect_land_scope_in_repo(&repo.path, false)
            .expect("scope")
            .expect("saves");
        execute_land_in_repo(
            &repo.path,
            &scope.base,
            &[files_commit("feat: landed", &["tracked.txt"])],
        )
        .expect("land should succeed");
        let landed_head = git(&repo.path, &["rev-parse", "HEAD"]);

        // A save on top is more recent than the land, so it goes first.
        write_file(&repo.path, "tracked.txt", "second\n");
        git(&repo.path, &["add", "tracked.txt"]);
        git(&repo.path, &["commit", "-m", "[kite] save 13:00:00"]);

        undo_in_repo(&repo.path).expect("undo should take the save");
        assert_eq!(git(&repo.path, &["rev-parse", "HEAD"]), landed_head);

        // With the save gone, the next undo reverses the land itself.
        git(&repo.path, &["checkout", "--", "tracked.txt"]);
        undo_in_repo(&repo.path).expect("undo should now take the land");
        assert_eq!(
            git(&repo.path, &["log", "-1", "--pretty=%s"]).trim(),
            "[kite] save 12:00:00"
        );
    }

    #[test]
    fn undo_of_a_root_quicksave_leaves_the_branch_unborn_with_the_work_intact() {
        let _lock = acquire_cwd_lock();
        let repo = init_root_kite_repo();

        undo_in_repo(&repo.path).expect("undo should handle a root save");

        assert!(
            check_ref_in(&repo.path, "HEAD").is_none(),
            "the branch should be unborn again"
        );
        assert_eq!(
            std::fs::read_to_string(repo.path.join("tracked.txt")).unwrap(),
            "base\n"
        );
    }

    /// The one thing a detached `HEAD` genuinely cannot do: there is no unborn
    /// detached state for it to become, so this has to say so rather than
    /// deleting the commit HEAD points at.
    #[test]
    fn undo_of_a_detached_root_quicksave_explains_it_needs_a_branch() {
        let _lock = acquire_cwd_lock();
        let repo = init_root_kite_repo();
        git(&repo.path, &["checkout", "-q", "--detach"]);
        let head_before = git(&repo.path, &["rev-parse", "HEAD"]);

        let err = undo_in_repo(&repo.path).expect_err("a detached root save cannot be unmade");
        let rendered = format!("{err:#}");
        assert!(rendered.contains("HEAD is detached"));
        assert!(rendered.contains("git switch -c"));

        assert_eq!(git(&repo.path, &["rev-parse", "HEAD"]), head_before);
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
    fn land_preflight_accepts_a_detached_head_but_refuses_to_publish_from_one() {
        let _lock = acquire_cwd_lock();
        let repo = init_repo();

        write_file(&repo.path, "tracked.txt", "saved change\n");
        git(&repo.path, &["add", "tracked.txt"]);
        git(&repo.path, &["commit", "-m", "[kite] save 12:00:00"]);
        let head_before = git(&repo.path, &["rev-parse", "HEAD"]);

        git(&repo.path, &["checkout", "-q", "--detach"]);

        // Landing moves HEAD itself, so it needs no branch.
        with_repo_cwd(&repo.path, || land_preflight(false))
            .expect("a detached HEAD should be landable");

        // `--push` does need one, and must say so before anything is rewritten.
        let err = with_repo_cwd(&repo.path, || land_preflight(true))
            .expect_err("publishing a detached HEAD should be refused up front");
        assert!(format!("{err:#}").contains("HEAD is detached"));
        assert!(format!("{err:#}").contains("git switch -c"));

        assert_eq!(git(&repo.path, &["rev-parse", "HEAD"]), head_before);
        let recovery = git(&repo.path, &["branch", "--list", "kite-*"]);
        assert!(
            recovery.trim().is_empty(),
            "nothing should have been rewritten"
        );
    }

    #[test]
    fn execute_land_moves_a_detached_head_onto_the_landed_commits() {
        let _lock = acquire_cwd_lock();
        let repo = init_repo();
        let branch = git(&repo.path, &["rev-parse", "--abbrev-ref", "HEAD"]);
        let branch = branch.trim().to_string();

        write_file(&repo.path, "tracked.txt", "saved change\n");
        git(&repo.path, &["add", "tracked.txt"]);
        git(&repo.path, &["commit", "-m", "[kite] save 12:00:00"]);

        git(&repo.path, &["checkout", "-q", "--detach"]);
        let pre_land_sha = git(&repo.path, &["rev-parse", "HEAD"]);
        let pre_land_tree = git(&repo.path, &["rev-parse", "HEAD^{tree}"]);

        let scope = collect_land_scope_in_repo(&repo.path, false)
            .expect("land scope should collect")
            .expect("kite saves should be landable");
        execute_land_in_repo(
            &repo.path,
            &scope.base,
            &[files_commit("feat: land tracked change", &["tracked.txt"])],
        )
        .expect("a detached land should succeed");

        // HEAD is still detached — just on the landed commit now.
        assert!(
            with_repo_cwd(&repo.path, head_branch_hint).is_none(),
            "landing must not attach HEAD to a branch"
        );
        assert_eq!(
            git(&repo.path, &["log", "-1", "--pretty=%s"]).trim(),
            "feat: land tracked change"
        );
        assert_eq!(
            git(&repo.path, &["rev-parse", "HEAD^{tree}"]),
            pre_land_tree
        );

        // The branch the user deliberately stepped off is left where it was.
        assert_eq!(
            git(&repo.path, &["rev-parse", &branch]),
            pre_land_sha,
            "a detached land moved a branch it was not on"
        );

        let recorded = git(&repo.path, &["config", "--local", PRE_LAND_BRANCH_KEY]);
        assert_eq!(recorded.trim(), DETACHED_TARGET);
        assert_eq!(
            git(&repo.path, &["rev-parse", PRE_LAND_REF]).trim(),
            pre_land_sha.trim()
        );

        let leftovers = git(&repo.path, &["branch", "--list", "kite-*"]);
        assert!(
            leftovers.trim().is_empty(),
            "a detached land left a branch behind: {leftovers}"
        );
    }

    #[test]
    fn execute_land_rewrites_root_history_from_a_detached_head() {
        let _lock = acquire_cwd_lock();
        let repo = init_root_kite_repo();
        let branch = git(&repo.path, &["rev-parse", "--abbrev-ref", "HEAD"]);
        let branch = branch.trim().to_string();

        git(&repo.path, &["checkout", "-q", "--detach"]);
        let pre_land_sha = git(&repo.path, &["rev-parse", "HEAD"]);

        let scope = collect_land_scope_in_repo(&repo.path, false)
            .expect("land scope should collect")
            .expect("root kite save should be landable");
        assert!(matches!(scope.base, KiteBase::Root));

        let groups = [group("feat: bootstrap project", &["h1"])];
        let commits = with_repo_cwd(&repo.path, || {
            plan_commits(&scope.base, &scope.units, &groups)
        });
        execute_land_in_repo(&repo.path, &scope.base, &commits)
            .expect("a detached root land should succeed");

        // A root rewrite has to build on an orphan branch; HEAD must come back
        // off it, and the branch must not be left behind.
        assert!(
            with_repo_cwd(&repo.path, head_branch_hint).is_none(),
            "a root land left HEAD attached to the orphan branch"
        );
        assert_eq!(
            git(&repo.path, &["log", "-1", "--pretty=%s"]).trim(),
            "feat: bootstrap project"
        );
        assert_eq!(git(&repo.path, &["rev-parse", &branch]), pre_land_sha);

        let leftovers = git(&repo.path, &["branch", "--list", "kite-*"]);
        assert!(
            leftovers.trim().is_empty(),
            "a detached root land left a branch behind: {leftovers}"
        );
    }

    /// The awkward corner: a root rewrite has to build on an orphan branch, so
    /// recovering a detached one means getting HEAD back off that branch as well
    /// as back to the right commit.
    #[test]
    fn a_failed_detached_root_land_detaches_back_off_the_orphan_branch() {
        let _lock = acquire_cwd_lock();
        let repo = init_root_kite_repo();
        git(&repo.path, &["checkout", "-q", "--detach"]);
        let pre_land_sha = git(&repo.path, &["rev-parse", "HEAD"]);

        let scope = collect_land_scope_in_repo(&repo.path, false)
            .expect("land scope should collect")
            .expect("root kite save should be landable");
        assert!(matches!(scope.base, KiteBase::Root));

        let err = execute_land_in_repo(
            &repo.path,
            &scope.base,
            &[files_commit("feat: bootstrap", &["missing.txt"])],
        )
        .expect_err("land should fail when a grouped file cannot be staged");
        assert!(format!("{err:#}").contains("the detached HEAD at"));

        assert!(
            with_repo_cwd(&repo.path, head_branch_hint).is_none(),
            "recovery left HEAD on the orphan branch"
        );
        assert_eq!(git(&repo.path, &["rev-parse", "HEAD"]), pre_land_sha);
        let leftovers = git(&repo.path, &["branch", "--list", "kite-*"]);
        assert!(
            leftovers.trim().is_empty(),
            "a failed detached root land left a branch behind: {leftovers}"
        );
        assert!(
            git(&repo.path, &["status", "--porcelain"])
                .trim()
                .is_empty(),
            "a failed land left staged changes to clean up"
        );
    }

    #[test]
    fn undo_restores_a_detached_land_without_touching_the_remote() {
        let _lock = acquire_cwd_lock();
        let (repo, _remote) = crate::test_support::init_repo_with_remote_branch("teammate-work");

        write_file(&repo.path, "tracked.txt", "saved change\n");
        git(&repo.path, &["add", "tracked.txt"]);
        git(&repo.path, &["commit", "-m", "[kite] save 12:00:00"]);

        git(&repo.path, &["checkout", "-q", "--detach"]);
        let pre_land_sha = git(&repo.path, &["rev-parse", "HEAD"]);

        let scope = collect_land_scope_in_repo(&repo.path, false)
            .expect("land scope should collect")
            .expect("kite saves should be landable");
        execute_land_in_repo(
            &repo.path,
            &scope.base,
            &[files_commit("feat: land tracked change", &["tracked.txt"])],
        )
        .expect("a detached land should succeed");

        undo_in_repo(&repo.path).expect("undo should restore a detached land");

        assert_eq!(git(&repo.path, &["rev-parse", "HEAD"]), pre_land_sha);
        assert!(with_repo_cwd(&repo.path, head_branch_hint).is_none());
        assert!(
            check_ref_in(&repo.path, PRE_LAND_REF).is_none(),
            "a successful undo consumes the marker"
        );
    }

    #[test]
    fn undo_refuses_when_the_land_was_on_a_branch_and_head_is_detached() {
        let _lock = acquire_cwd_lock();
        let repo = init_repo();
        let branch = git(&repo.path, &["rev-parse", "--abbrev-ref", "HEAD"]);
        let branch = branch.trim().to_string();

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

        // Same commit, but reached without the branch — undo would leave the
        // branch pointing at the landed history it was supposed to remove.
        git(&repo.path, &["checkout", "-q", "--detach"]);
        let protected_head = git(&repo.path, &["rev-parse", "HEAD"]);

        let err = undo_in_repo(&repo.path).expect_err("undo should refuse on a detached HEAD");
        let rendered = format!("{err:#}");
        assert!(rendered.contains(&format!("The last land was on `{branch}`")));
        assert!(rendered.contains("the detached HEAD at"));

        assert_eq!(git(&repo.path, &["rev-parse", "HEAD"]), protected_head);
        assert!(
            check_ref_in(&repo.path, PRE_LAND_REF).is_some(),
            "a refused undo must keep the rollback marker"
        );
    }

    #[test]
    fn undo_refuses_a_detached_land_from_a_branch_and_says_where_to_go() {
        let _lock = acquire_cwd_lock();
        let repo = init_repo();

        write_file(&repo.path, "tracked.txt", "saved change\n");
        git(&repo.path, &["add", "tracked.txt"]);
        git(&repo.path, &["commit", "-m", "[kite] save 12:00:00"]);

        git(&repo.path, &["checkout", "-q", "--detach"]);
        let scope = collect_land_scope_in_repo(&repo.path, false)
            .expect("land scope should collect")
            .expect("kite saves should be landable");
        execute_land_in_repo(
            &repo.path,
            &scope.base,
            &[files_commit("feat: land tracked change", &["tracked.txt"])],
        )
        .expect("a detached land should succeed");
        let landed_head = git(&repo.path, &["rev-parse", "HEAD"]);

        // Picking the landed work up on a branch is a different place from the
        // one that land recorded, so undoing here would rewind a ref the land
        // never touched.
        git(&repo.path, &["checkout", "-q", "-b", "picked-up"]);
        let protected_head = git(&repo.path, &["rev-parse", "HEAD"]);

        let err = undo_in_repo(&repo.path).expect_err("undo should refuse on a branch");
        let rendered = format!("{err:#}");
        assert!(rendered.contains("The last land was on a detached HEAD"));
        // The landed commit is the only way back, so the message names it.
        assert!(rendered.contains(&format!(
            "git switch --detach {}",
            short_sha(landed_head.trim())
        )));

        assert_eq!(git(&repo.path, &["rev-parse", "HEAD"]), protected_head);
        assert!(check_ref_in(&repo.path, PRE_LAND_REF).is_some());
    }

    #[test]
    fn an_interrupted_detached_land_heals_itself_back_to_the_same_commit() {
        let _lock = acquire_cwd_lock();
        let repo = init_repo();

        write_file(&repo.path, "tracked.txt", "saved change\n");
        git(&repo.path, &["add", "tracked.txt"]);
        git(&repo.path, &["commit", "-m", "[kite] save 12:00:00"]);
        git(&repo.path, &["checkout", "-q", "--detach"]);
        let saves_head = git(&repo.path, &["rev-parse", "HEAD"]);

        // What a detached land killed mid-rewrite leaves: a marker saying the
        // land had no branch, no landed head recorded, HEAD on a partial commit.
        git(&repo.path, &["update-ref", PRE_LAND_REF, saves_head.trim()]);
        git(
            &repo.path,
            &["config", "--local", PRE_LAND_BRANCH_KEY, DETACHED_TARGET],
        );
        git(&repo.path, &["reset", "-q", "--soft", "HEAD~1"]);
        git(&repo.path, &["reset", "-q"]);
        git(&repo.path, &["add", "tracked.txt"]);
        git(&repo.path, &["commit", "-qm", "feat: half a landing"]);

        with_repo_cwd(&repo.path, heal_interrupted_land);

        assert!(
            with_repo_cwd(&repo.path, head_branch_hint).is_none(),
            "healing a detached land must not attach HEAD to a branch"
        );
        assert_eq!(git(&repo.path, &["rev-parse", "HEAD"]), saves_head);
        assert!(
            git(&repo.path, &["status", "--porcelain"])
                .trim()
                .is_empty()
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

        let err = with_repo_cwd(&repo.path, || land_preflight(false))
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

        let scope = with_repo_cwd(&nested, || collect_land_scope(false, None))
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
            "you are still on `{original_branch}` with every save intact"
        )));
        assert!(rendered.contains("run `kt land` again"));
        // The temporary branch is an implementation detail of a successful
        // land; a failure should never make the user deal with it.
        assert!(!rendered.contains("kite-recovery-"));

        let current_branch = git(&repo.path, &["rev-parse", "--abbrev-ref", "HEAD"]);
        assert_eq!(current_branch.trim(), original_branch);

        let leftovers = git(&repo.path, &["branch", "--list", "kite-recovery-*"]);
        assert!(
            leftovers.trim().is_empty(),
            "a failed land left a branch behind: {leftovers}"
        );

        let status = git(&repo.path, &["status", "--porcelain"]);
        assert!(
            status.trim().is_empty(),
            "a failed land left staged changes to clean up: {status}"
        );
    }

    #[test]
    fn landing_creates_no_branch_at_all() {
        let _lock = acquire_cwd_lock();
        let repo = init_repo();
        save_two_hunk_change(&repo.path);

        let scope = collect_land_scope_in_repo(&repo.path, false)
            .expect("scope")
            .expect("saves");

        // A hook is the only way to observe the repository mid-land.
        write_file(
            &repo.path,
            ".git/hooks/pre-commit",
            "#!/bin/sh\ngit branch --format='%(refname:short)' >> .git/seen\n",
        );
        let hook = repo.path.join(".git/hooks/pre-commit");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755))
                .expect("hook should be executable");
        }

        let groups = [group("feat: top", &["h1"]), group("feat: bottom", &["h2"])];
        let commits = with_repo_cwd(&repo.path, || {
            plan_commits(&scope.base, &scope.units, &groups)
        });
        execute_land_in_repo(&repo.path, &scope.base, &commits).expect("land should succeed");

        let seen = std::fs::read_to_string(repo.path.join(".git/seen")).unwrap_or_default();
        assert!(!seen.contains("kite-"), "landing created a branch: {seen}");
    }

    #[test]
    fn an_interrupted_land_heals_itself_on_the_next_command() {
        let _lock = acquire_cwd_lock();
        let repo = init_repo();

        write_file(&repo.path, "tracked.txt", "saved change\n");
        git(&repo.path, &["add", "tracked.txt"]);
        git(&repo.path, &["commit", "-m", "[kite] save 12:00:00"]);
        let saves_head = git(&repo.path, &["rev-parse", "HEAD"]);
        let branch = git(&repo.path, &["rev-parse", "--abbrev-ref", "HEAD"]);
        let branch = branch.trim().to_string();

        // Exactly what a land killed mid-rewrite leaves: a marker naming the
        // branch, no landed head recorded, HEAD detached on a partial commit.
        git(&repo.path, &["update-ref", PRE_LAND_REF, saves_head.trim()]);
        git(
            &repo.path,
            &["config", "--local", PRE_LAND_BRANCH_KEY, &branch],
        );
        git(&repo.path, &["checkout", "-q", "--detach"]);
        git(&repo.path, &["reset", "-q", "--soft", "HEAD~1"]);
        git(&repo.path, &["reset", "-q"]);
        git(&repo.path, &["add", "tracked.txt"]);
        git(&repo.path, &["commit", "-qm", "feat: half a landing"]);

        with_repo_cwd(&repo.path, heal_interrupted_land);

        assert_eq!(
            git(&repo.path, &["rev-parse", "--abbrev-ref", "HEAD"]).trim(),
            branch
        );
        assert_eq!(git(&repo.path, &["rev-parse", "HEAD"]), saves_head);
        assert!(
            git(&repo.path, &["status", "--porcelain"])
                .trim()
                .is_empty()
        );
    }

    #[test]
    fn healing_leaves_a_finished_land_alone() {
        let _lock = acquire_cwd_lock();
        let repo = init_repo();

        write_file(&repo.path, "tracked.txt", "saved change\n");
        git(&repo.path, &["add", "tracked.txt"]);
        git(&repo.path, &["commit", "-m", "[kite] save 12:00:00"]);

        let scope = collect_land_scope_in_repo(&repo.path, false)
            .expect("scope")
            .expect("saves");
        execute_land_in_repo(
            &repo.path,
            &scope.base,
            &[files_commit("feat: landed", &["tracked.txt"])],
        )
        .expect("land should succeed");
        let landed_head = git(&repo.path, &["rev-parse", "HEAD"]);

        // Healing must never undo a land that actually completed.
        with_repo_cwd(&repo.path, heal_interrupted_land);
        assert_eq!(git(&repo.path, &["rev-parse", "HEAD"]), landed_head);
        assert_eq!(
            git(&repo.path, &["log", "-1", "--pretty=%s"]).trim(),
            "feat: landed"
        );
    }

    #[test]
    fn failed_land_leaves_the_saves_and_the_previous_rollback_marker_alone() {
        let _lock = acquire_cwd_lock();
        let repo = init_repo();

        // A first land that succeeds, so there is a real rollback marker.
        write_file(&repo.path, "tracked.txt", "first save\n");
        git(&repo.path, &["add", "tracked.txt"]);
        git(&repo.path, &["commit", "-m", "[kite] save 12:00:00"]);
        let first_pre_land = git(&repo.path, &["rev-parse", "HEAD"]);
        let scope = collect_land_scope_in_repo(&repo.path, false)
            .expect("scope")
            .expect("saves");
        execute_land_in_repo(
            &repo.path,
            &scope.base,
            &[files_commit("feat: first", &["tracked.txt"])],
        )
        .expect("first land should succeed");

        // A second land that fails.
        write_file(&repo.path, "tracked.txt", "second save\n");
        git(&repo.path, &["add", "tracked.txt"]);
        git(&repo.path, &["commit", "-m", "[kite] save 13:00:00"]);
        let saves_head = git(&repo.path, &["rev-parse", "HEAD"]);
        let scope = collect_land_scope_in_repo(&repo.path, false)
            .expect("scope")
            .expect("saves");
        execute_land_in_repo(
            &repo.path,
            &scope.base,
            &[files_commit("feat: second", &["missing.txt"])],
        )
        .expect_err("second land should fail");

        // The saves are exactly where they were.
        assert_eq!(git(&repo.path, &["rev-parse", "HEAD"]), saves_head);

        // And `kt undo` still points at the land that actually happened.
        let marker = git(&repo.path, &["rev-parse", PRE_LAND_REF]);
        assert_eq!(
            marker.trim(),
            first_pre_land.trim(),
            "a failed land clobbered the previous land's rollback marker"
        );
    }

    #[test]
    fn failed_land_keeps_files_a_hook_rewrote() {
        let _lock = acquire_cwd_lock();
        let repo = init_repo();

        write_file(&repo.path, "tracked.txt", "saved change\n");
        git(&repo.path, &["add", "tracked.txt"]);
        git(&repo.path, &["commit", "-m", "[kite] save 12:00:00"]);

        let scope = collect_land_scope_in_repo(&repo.path, false)
            .expect("scope")
            .expect("saves");

        // Stand in for a formatter hook that rewrites the worktree before the
        // commit is rejected: cleanup must not throw its work away.
        write_file(&repo.path, "tracked.txt", "reformatted by a hook\n");

        execute_land_in_repo(
            &repo.path,
            &scope.base,
            &[files_commit("feat: land", &["missing.txt"])],
        )
        .expect_err("land should fail");

        let content = std::fs::read_to_string(repo.path.join("tracked.txt"))
            .expect("worktree file should exist");
        assert_eq!(content, "reformatted by a hook\n");
    }
}
