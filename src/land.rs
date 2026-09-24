//! Plan a local rewrite, build its commits, and recover failed attempts.

mod commits;
mod publish;
mod recovery;
mod stash;
mod state;
use commits::{create_commits, verify_saved_tree};
pub(crate) use publish::publish_current_branch;
#[cfg(test)]
pub(crate) use recovery::heal_interrupted_land;
use recovery::{
    branch_checked_out_elsewhere, ensure_no_git_operation_in_progress, ensure_no_land_in_progress,
    restore_head,
};
pub(crate) use recovery::{recovery_blocks_commands, undo};
use state::*;

use anyhow::{Context, Result, anyhow};
use colored::*;

use crate::diff::ChangedFiles;
use crate::git::{
    Head, Hooks, KiteBase, branch_to_publish, check_ref, current_worktree_key, execute_git,
    has_head_commit, has_remote, has_unmerged_paths, head_position, kite_save_stack, saved_changes,
};
use crate::synth::{CommitGroup, normalize_groups, sanitize_commit_message, synthesize_groups};
use crate::ui::{Spinner, confirm, overflow_note, pluralize, print_ai_unavailable, prompt_line};

/// How many files to list under a planned commit before summarizing. The plan
/// is the last thing shown before history is rewritten, and one that scrolls
/// for thousands of lines is one nobody actually reads.
const MAX_PLAN_FILES_SHOWN: usize = 12;
const MAX_PLAN_BODY_LINES_SHOWN: usize = 6;
const EMPTY_INITIAL_MESSAGE: &str = "chore: empty initial snapshot";

#[derive(Clone, Debug)]
struct LandScope {
    base: KiteBase,
    save_count: usize,
    files: ChangedFiles,
}

pub(crate) struct LandOptions {
    pub(crate) push: bool,
    pub(crate) yes: bool,
    pub(crate) allow_dirty: bool,
    pub(crate) tag: Option<String>,
    pub(crate) hooks: Hooks,
}

pub(crate) async fn land(options: LandOptions) -> Result<()> {
    let LandOptions {
        push,
        yes: auto_confirm,
        allow_dirty,
        tag,
        hooks,
    } = options;

    let Some(status) = land_preflight(push)? else {
        return Ok(());
    };

    let stashed = allow_dirty && stash::save()?;

    let land_result = (async {
        let expected_target = head_position()?;
        let expected_head = execute_git(&["rev-parse", "HEAD"])?;
        // The preflight status predates the stash, so it only describes the
        // tree the clean-worktree check cares about.
        let Some(scope) = collect_land_scope(allow_dirty, Some(&status))? else {
            return Ok(());
        };

        let Some(mut commits) = plan_commits(&scope.files, auto_confirm).await? else {
            return Ok(());
        };
        if commits.is_empty() && scope.base == KiteBase::Root {
            commits.push(CommitGroup {
                message: EMPTY_INITIAL_MESSAGE.to_string(),
                files: Vec::new(),
            });
        }

        if let Some(tag) = tag.as_deref().filter(|s| !s.trim().is_empty()) {
            for commit in &mut commits {
                commit.message = append_tag_to_message(&commit.message, tag);
            }
        }

        if scope.files.paths().is_empty() {
            let outcome = if scope.base == KiteBase::Root { "replace them with one empty initial commit" } else { "remove them" };
            println!("{} These {} cancel out; {outcome} without changing any files.", "·".cyan(), pluralize(scope.save_count, "save"));
        }
        if !commits.is_empty() {
            print!("{}", render_land_plan(&commits, scope.save_count));
        }

        let question = if push {
            "Rewrite history and publish?"
        } else {
            "Rewrite history?"
        };
        if !auto_confirm && !confirm(question)? {
            println!("{} Aborted — no history changed", "·".red());
            return Ok(());
        }

        if head_position()? != expected_target
            || execute_git(&["rev-parse", "HEAD"])? != expected_head
        {
            anyhow::bail!("HEAD changed while planning the land. No history was rewritten; run `kt land` again to review the current saves.");
        }
        execute_land(&scope.base, &commits, hooks)?;

        if push {
            println!("{} Landed", "✓".green());
            publish_current_branch().context("Landed locally, but publishing failed")?;
        } else if has_remote() {
            if matches!(head_position(), Ok(Head::Detached(_))) {
                println!(
                    "{} Landed — HEAD is detached; create a branch with {} to publish",
                    "✓".green(),
                    "git switch -c <name>".bold()
                );
            } else {
                println!(
                    "{} Landed — review, then {} or {}",
                    "✓".green(),
                    "kt publish".bold(),
                    "kt pr".bold()
                );
            }
        } else {
            println!("{} Landed", "✓".green());
        }

        Ok(())
    })
    .await;

    if stashed && let Err(restore_error) = stash::restore() {
        return match land_result {
            Ok(_) => Err(restore_error),
            Err(land_error) => Err(anyhow!(
                "{land_error}\n\nIn addition, restoring your stashed changes failed: {restore_error}"
            )),
        };
    }

    land_result
}

async fn plan_commits(
    files: &ChangedFiles,
    auto_confirm: bool,
) -> Result<Option<Vec<CommitGroup>>> {
    if files.paths().is_empty() {
        return Ok(Some(Vec::new()));
    }
    let spinner = Spinner::start("Synthesizing");
    let result = synthesize_groups(files).await;
    spinner.stop();
    match result {
        Ok(groups) => Ok(Some(normalize_groups(groups, files))),
        Err(error) => {
            print_ai_unavailable(&error);
            if auto_confirm {
                anyhow::bail!(
                    "AI synthesis is unavailable. Rerun `kt land` without --yes to enter a commit message manually."
                );
            }
            let Some(message) = prompt_line("One commit message (blank to abort)")? else {
                println!("{} Aborted - no history changed", "·".red());
                return Ok(None);
            };
            Ok(Some(vec![CommitGroup {
                message: sanitize_commit_message(&message),
                files: files.paths().to_vec(),
            }]))
        }
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

    ensure_no_git_operation_in_progress("kt land")?;
    ensure_no_land_in_progress()?;

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

    // The path list is authoritative; the diff is only ever shown to the
    // model, so a path is never recovered from it.
    let (paths, diff) = saved_changes(&stack.base)?;

    Ok(Some(LandScope {
        base: stack.base,
        save_count: stack.count,
        files: ChangedFiles::new(paths, diff),
    }))
}

fn append_tag_to_message(message: &str, tag: &str) -> String {
    let tag = tag.trim();
    if tag.is_empty() {
        return message.to_string();
    }
    let (subject, body) = message.split_once('\n').unwrap_or((message, ""));
    let suffix = format!(" [{tag}]");
    if subject.trim_end().ends_with(&suffix) {
        return message.to_string();
    }
    let tagged = format!("{}{suffix}", subject.trim_end())
        .trim_start()
        .to_string();
    if message.contains('\n') {
        format!("{tagged}\n{body}")
    } else {
        tagged
    }
}

/// Renders the proposed history as a numbered list of commits, each with the
/// files it lands underneath.
fn render_land_plan(commits: &[CommitGroup], save_count: usize) -> String {
    let mut plan = format!(
        "{} Plan: {} {} {}\n\n",
        "·".cyan(),
        pluralize(save_count, "save"),
        "→".dimmed(),
        pluralize(commits.len(), "commit"),
    );

    for (index, commit) in commits.iter().enumerate() {
        // The subject gets the numbered line and the body sits indented under
        // it, so a multi-line message never runs flush-left through the file
        // tree. Bodies carry what each commit fixes; this is the one screen
        // read before history is rewritten.
        let mut lines = commit.message.lines();
        let subject = lines.next().unwrap_or("").trim();
        plan.push_str(&format!("  {}. {}\n", index + 1, subject.bold()));

        let body: Vec<&str> = lines
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .collect();
        for line in body.iter().take(MAX_PLAN_BODY_LINES_SHOWN) {
            plan.push_str(&format!("     {}\n", line.dimmed()));
        }
        if let Some(note) = overflow_note(body.len(), MAX_PLAN_BODY_LINES_SHOWN) {
            plan.push_str(&format!("     {}\n", note.dimmed()));
        }

        for (position, file) in commit.files.iter().take(MAX_PLAN_FILES_SHOWN).enumerate() {
            let last = position + 1 == commit.files.len();
            let glyph = if last { "└─" } else { "├─" };
            plan.push_str(&format!("     {} {}\n", glyph.dimmed(), file));
        }

        if let Some(note) = overflow_note(commit.files.len(), MAX_PLAN_FILES_SHOWN) {
            plan.push_str(&format!("     {}\n", note.dimmed()));
        }
    }

    plan.push('\n');
    plan
}

fn execute_land(base: &KiteBase, commits: &[CommitGroup], hooks: Hooks) -> Result<()> {
    if commits.is_empty() && !saved_changes(base)?.0.is_empty() {
        anyhow::bail!("Refusing to land an empty plan - that would discard the saves.");
    }

    // Most callers pass through preflight, but keep the history-mutating core
    // safe when invoked directly by tests or future commands.
    ensure_no_git_operation_in_progress("kt land")?;
    ensure_no_land_in_progress()?;

    if !execute_git(&["status", "--porcelain"])?.trim().is_empty() {
        anyhow::bail!(
            "The worktree changed while planning the land. Save or stash those changes, then run `kt land` again; no history was rewritten."
        );
    }

    // Resolved before anything is rewritten, so the landed commits have a
    // recorded home: a branch to move, or the detached HEAD to leave sitting on
    // them. Discovering it afterwards would mean unwinding a rewrite that had
    // already happened.
    let target = head_position()?;
    let worktree = current_worktree_key()?;
    let pre_land_sha = execute_git(&["rev-parse", "HEAD"])?;
    // Hooks see a normal branch, and recovery can identify its exact ref.
    let transaction_ref = unique_kite_ref(TRANSACTION_REF_PREFIX);

    // Kept so a land that fails cannot overwrite the rollback marker left by
    // the last one that succeeded.
    let previous_marker = PreLandMarker::capture();
    let transaction = install_in_progress_marker(
        &previous_marker,
        pre_land_sha.trim(),
        &target.land_key(),
        &worktree,
        &transaction_ref,
    )?;

    let rewrite = (|| -> Result<()> {
        prepare_landing_head(base, pre_land_sha.trim(), &transaction_ref)?;
        // An unborn branch still needs one commit to represent an empty tree.
        let empty_root = [CommitGroup {
            message: EMPTY_INITIAL_MESSAGE.to_string(),
            files: Vec::new(),
        }];
        let groups = if *base == KiteBase::Root && commits.is_empty() {
            &empty_root
        } else {
            commits
        };
        let hook_changes = create_commits(groups, hooks)?;
        verify_saved_tree(pre_land_sha.trim(), &hook_changes)?;
        stash::record_landed_head(execute_git(&["rev-parse", "HEAD"])?.trim())?;
        finalize_landed_head(&target, pre_land_sha.trim(), &transaction_ref)?;
        let landed_head =
            execute_git(&["rev-parse", "HEAD"]).context("Could not resolve the landed HEAD")?;
        record_landed_head(&transaction, landed_head.trim())
            .context("Could not record the landed HEAD for `kt undo`")?;
        if !hook_changes.is_empty() {
            println!(
                "{} Included commit-hook changes to {}",
                "✓".green(),
                pluralize(hook_changes.len(), "file")
            );
        }
        Ok(())
    })();

    // Commit creation and marker completion share the same rollback. A land
    // cannot succeed (or publish) until both history and recovery state agree.
    let Err(error) = rewrite else {
        return Ok(());
    };
    if let Err(restore_error) = restore_head(&target, pre_land_sha.trim(), &transaction_ref) {
        anyhow::bail!(
            "{error:#}\n\nKite could not restore {}: {restore_error:#}\nYour saves are retained at `{PRE_LAND_REF}`. Resolve the problem above, then run `kt undo` in this worktree.",
            target.describe()
        );
    }
    let recovery = format!(
        "{error:#}\n\nThe rewrite was undone. You are back on {} with every save intact.\nFix the problem above, then run `kt land` again.",
        target.describe()
    );
    if let Err(marker_error) = restore_previous_marker(&transaction) {
        anyhow::bail!(
            "{recovery}\n\nKite could not restore the previous rollback marker: {marker_error:#}. Run `kt undo` before retrying."
        );
    }
    Err(anyhow!(recovery))
}

/// Builds commits on one exact, transaction-owned temporary branch. Keeping
/// `HEAD` under `refs/heads` preserves the assumptions made by ordinary Git
/// hooks, while every commit advances a persisted recovery pointer with no
/// commit-to-marker crash gap. Cleanup only ever deletes this recorded ref
/// with its expected object id.
fn prepare_landing_head(base: &KiteBase, pre_land_sha: &str, transaction_ref: &str) -> Result<()> {
    if check_ref(transaction_ref).as_deref() != Some(pre_land_sha) {
        anyhow::bail!("Kite's reserved land transaction ref moved before rewriting began");
    }
    execute_git(&["symbolic-ref", "HEAD", transaction_ref])?;

    match base {
        KiteBase::Commit(base_sha) => {
            execute_git(&["reset", "--soft", base_sha])?;
            execute_git(&["reset"])?;
        }
        KiteBase::Root => {
            // Deleting the ref while HEAD points to it creates an unborn
            // temporary branch while retaining its exact symbolic HEAD.
            execute_git(&["update-ref", "-d", transaction_ref, pre_land_sha])?;
            execute_git(&["read-tree", "--empty"])?;
        }
    }
    Ok(())
}

fn finalize_landed_head(target: &Head, pre_land_sha: &str, transaction_ref: &str) -> Result<()> {
    let new_head = check_ref(transaction_ref)
        .context("Kite's land transaction produced no commit to finalize")?;

    match target {
        Head::Branch(branch) => {
            if branch_checked_out_elsewhere(branch)? {
                anyhow::bail!(
                    "`{branch}` was checked out in another worktree while Kite was landing; it was left untouched"
                );
            }
            execute_git(&[
                "update-ref",
                &format!("refs/heads/{branch}"),
                &new_head,
                pre_land_sha,
            ])
            .with_context(|| {
                format!(
                    "`{branch}` moved while Kite was landing; its newer value was left untouched"
                )
            })?;
            execute_git(&["checkout", branch])?;
        }
        // Leave the landed result detached, not symbolically attached to
        // Kite's internal transaction ref.
        Head::Detached(_) => {
            execute_git(&["checkout", "--detach", &new_head])?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
