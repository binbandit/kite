//! Undo saves and lands, or finish recovery after an interrupted command.

use super::{stash, state::*};
use crate::git::{
    DETACHED_TARGET, Head, active_git_operation, check_ref, current_worktree_key, execute_git,
    has_head_commit, has_remote, head_position, head_symbolic_ref, is_ancestor, is_save_subject,
    short_sha,
};
use crate::ui::{Spinner, confirm, pluralize};
use anyhow::{Context, Result};
use colored::*;

/// Reverses the most recent thing Kite did on this branch.
///
/// A quicksave sitting on top of history is by definition more recent than any
/// land beneath it, so that goes first; otherwise this undoes the last land.
/// Running it repeatedly walks back through saves and then the land, which is
/// the order they happened in.
pub(crate) fn undo() -> Result<()> {
    ensure_no_git_operation_in_progress("kt undo")?;

    // An interrupted rewrite takes precedence over the save commit that Kite
    // was in the middle of replacing. Otherwise `kt undo` would peel saves
    // from the recorded target while leaving the interrupted transaction
    // behind.
    match pre_land_state() {
        PreLandState::InProgress(_) | PreLandState::LegacyInProgress { .. } => {
            if recover_interrupted_land()? {
                stash::restore()?;
                return Ok(());
            }
        }
        PreLandState::Undoing(_) => return undo_last_land(),
        PreLandState::Inconsistent => anyhow::bail!(
            "Kite's rollback marker is incomplete, so it cannot be undone safely. Inspect `{PRE_LAND_REF}` and `{LAND_STATE_REF}`."
        ),
        PreLandState::Empty { .. } | PreLandState::Completed(_) => {}
    }

    if stash::restore()? {
        return Ok(());
    }

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
pub(super) fn undo_last_save(subject: &str) -> Result<()> {
    match check_ref("HEAD~1") {
        Some(parent) => execute_git(&["reset", "--mixed", &parent]).map(|_| ())?,
        None => {
            // The save is the repository's very first commit: there is no
            // parent to reset onto. Empty the index and make the branch unborn;
            // leave working files alone, just like an ordinary mixed reset.
            match head_position()? {
                Head::Branch(branch) => {
                    let saved_head = execute_git(&["rev-parse", "HEAD"])?;
                    execute_git(&["read-tree", "--empty"])?;
                    execute_git(&[
                        "update-ref",
                        "-d",
                        &format!("refs/heads/{branch}"),
                        saved_head.trim(),
                    ])?;
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

/// Cancels an owned land that stopped between recording its rollback state
/// and recording its final `HEAD`.
///
/// Recovery is deliberately explicit. The worktree id proves where Kite was
/// running, but it cannot prove that a different detached commit checked out
/// later is still one of Kite's partial commits. Requiring `kt undo` keeps an
/// ordinary command from silently resetting that newer checkout.
pub(super) fn recover_interrupted_land() -> Result<bool> {
    let transaction = match pre_land_state() {
        PreLandState::InProgress(transaction) => transaction,
        PreLandState::LegacyInProgress { owner } => {
            if owner != current_worktree_key()? {
                anyhow::bail!(
                    "An interrupted `kt land` belongs to another worktree ({owner}). Run `kt undo` there instead."
                );
            }
            anyhow::bail!(
                "This legacy interrupted land belongs to worktree {owner}, but has no exact transaction ref. Inspect `{PRE_LAND_REF}` before recovering it manually."
            );
        }
        _ => return Ok(false),
    };
    let land = &transaction.landing;
    if land.owner != current_worktree_key()? {
        anyhow::bail!(
            "An interrupted `kt land` belongs to another worktree ({}). Run `kt undo` there; this worktree was not changed.",
            land.owner
        );
    }
    let restore_target = if land.target == DETACHED_TARGET {
        Head::Detached(land.pre_land_sha.clone())
    } else {
        Head::Branch(land.target.clone())
    };
    restore_head(&restore_target, &land.pre_land_sha, &land.transaction_ref)
        .context("Could not recover the interrupted land")?;
    restore_previous_marker(&transaction).context(
        "The interrupted land was restored, but Kite could not clear its recovery marker",
    )?;
    let recovered_position = head_position().unwrap_or(restore_target);

    println!(
        "{} Recovered interrupted land — back on {} with every save intact",
        "✓".green(),
        recovered_position.describe()
    );
    Ok(true)
}

pub(super) fn undo_last_land() -> Result<()> {
    let state = pre_land_state();
    let (transaction, target) = match state {
        PreLandState::Empty { .. } => {
            println!(
                "{} {}",
                "·".yellow(),
                "Nothing to undo — no quicksave on top and no land recorded".dimmed()
            );
            return Ok(());
        }
        PreLandState::Completed(recorded) => {
            let head = head_position()?;
            let restore_from = guard_undo_target(&head, &recorded)?;
            let status = execute_git(&["status", "--porcelain"])?;
            if !status.trim().is_empty() {
                anyhow::bail!(
                    "Working directory is not clean. Please `kt` your changes or stash them before undoing."
                );
            }
            let target = recorded.target.clone();
            (begin_completed_undo(recorded, &restore_from)?, target)
        }
        PreLandState::Undoing(transaction) => {
            if transaction.owner != current_worktree_key()? {
                anyhow::bail!(
                    "An interrupted `kt undo` belongs to another worktree ({}). Finish it there instead.",
                    transaction.owner
                );
            }
            let target = transaction.land.target.clone();
            (transaction, target)
        }
        PreLandState::InProgress(_) | PreLandState::LegacyInProgress { .. } => {
            anyhow::bail!("Recover the interrupted land before undoing a completed one")
        }
        PreLandState::Inconsistent => anyhow::bail!(
            "Kite's rollback marker is incomplete, so it cannot be undone safely. Inspect `{PRE_LAND_REF}` and `refs/kite/land_state`."
        ),
    };

    restore_completed_land(&transaction)?;
    finish_completed_undo(&transaction)?;

    if target != DETACHED_TARGET && has_remote() {
        let branch = target;
        let pre_land_sha = &transaction.land.pre_land_sha;
        let spinner = Spinner::start("Reverting remote");
        let reverted = execute_git(&[
            "push",
            &format!(
                "--force-with-lease=refs/heads/{branch}:{}",
                transaction.land.landed_head
            ),
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
    } else if has_remote() {
        // Nothing to revert: a detached land was never publishable, so the
        // remote cannot be holding the history it produced.
        println!(
            "{} {}",
            "·".dimmed(),
            "Remote untouched — a detached HEAD has no branch to revert".dimmed()
        );
    }

    println!("{} Restored pre-land saves", "✓".green());
    Ok(())
}

/// `refs/kite/pre_land` is a single repo-wide ref, so on its own it says
/// nothing about where it belongs. Without this guard, landing in one place and
/// undoing in another hard-resets — and force-pushes — the wrong branch to an
/// unrelated commit.
pub(super) fn guard_undo_target(head: &Head, recorded: &CompletedMarker) -> Result<String> {
    if recorded.target != head.land_key() {
        anyhow::bail!(
            "The last land was on {}, but you are on {}. {} — undoing here would reset unrelated history.",
            describe_landed_target(&recorded.target),
            head.describe(),
            recover_landed_target(&recorded.target, Some(recorded.landed_head.as_str()))
        );
    }

    // A branch name identifies its target across worktrees. "Detached" does
    // not: every detached linked worktree would otherwise look identical and
    // could consume another worktree's rollback marker.
    if matches!(head, Head::Detached(_)) {
        let Some(landed_worktree) = recorded.owner.as_deref() else {
            anyhow::bail!(
                "The last detached land has no worktree recorded, so Kite cannot undo it safely. {} and inspect it before retrying.",
                recover_landed_target(&recorded.target, Some(recorded.landed_head.as_str()))
            );
        };
        let current_worktree = current_worktree_key()?;
        if landed_worktree != current_worktree {
            anyhow::bail!(
                "The last detached land belongs to another worktree. Run `kt undo` there instead; its landed commit is recorded at {}.",
                recorded.landed_head
            );
        }
    }

    // Landing recorded where it left HEAD. If HEAD moved since, undo would
    // throw away whatever was committed on top of the landed commits.
    let landed_head = &recorded.landed_head;
    let current_head = execute_git(&["rev-parse", "HEAD"])?;
    if current_head.trim() == landed_head {
        return Ok(current_head.trim().to_string());
    }

    // Moving forward from a detached land is analogous to adding commits on a
    // branch, so it can be confirmed. An unrelated or older detached commit is
    // a different location entirely and must never be reset by this marker.
    if matches!(head, Head::Detached(_)) && !is_ancestor(landed_head, "HEAD") {
        anyhow::bail!(
            "You are no longer on the history produced by the last detached land. Return to it with `git switch --detach {landed_head}`, then run `kt undo` again."
        );
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
        return Ok(current_head.trim().to_string());
    }
    anyhow::bail!("Undo cancelled — nothing changed")
}

/// Claims a completed marker before moving any history. The CAS means a
/// concurrent land or undo cannot replace the marker between validation and
/// reset, and `from_head` makes crash recovery use the exact value the user
/// approved rather than whatever the branch happens to contain later.
pub(super) fn begin_completed_undo(
    recorded: CompletedMarker,
    from_head: &str,
) -> Result<UndoTransaction> {
    let owner = current_worktree_key()?;
    let landed_head = recorded.landed_head.clone();
    let (keepalive_ref, create_keepalive) = match recorded.keepalive_ref.clone() {
        Some(existing) => {
            if check_ref(&existing).as_deref() != Some(recorded.pre_land_sha.as_str()) {
                anyhow::bail!("The completed land's keepalive ref moved; undo stopped safely");
            }
            (existing, false)
        }
        None => (unique_kite_ref("refs/kite/keepalive/undo-"), true),
    };
    let land = CompletedLand {
        pre_land_sha: recorded.pre_land_sha,
        target: recorded.target,
        owner: recorded.owner,
        landed_head,
        keepalive_ref: keepalive_ref.clone(),
    };
    let undoing = AtomicLandRecord {
        version: LAND_STATE_VERSION,
        phase: AtomicLandPhase::Undoing {
            land: land.clone(),
            owner: owner.clone(),
            from_head: from_head.to_string(),
        },
    };
    let state_oid = write_land_record(&undoing)?;
    let mut edits = vec![match recorded.state_oid {
        Some(old) => RefEdit::Update {
            name: LAND_STATE_REF.to_string(),
            new: state_oid.clone(),
            old,
        },
        None => RefEdit::Create {
            name: LAND_STATE_REF.to_string(),
            new: state_oid.clone(),
        },
    }];
    if create_keepalive {
        edits.push(RefEdit::Create {
            name: keepalive_ref,
            new: land.pre_land_sha.clone(),
        });
    }
    commit_ref_transaction(&edits).context("Could not reserve the completed land for undo")?;
    clear_legacy_marker_config();

    Ok(UndoTransaction {
        state_oid,
        land,
        owner,
        from_head: from_head.to_string(),
    })
}

/// Restores exactly the ref value captured by `begin_completed_undo`. Every
/// ref move is compare-and-swap, so a branch advanced by another worktree or
/// process survives untouched.
pub(super) fn restore_completed_land(transaction: &UndoTransaction) -> Result<()> {
    if transaction.owner != current_worktree_key()? {
        anyhow::bail!("This undo transaction belongs to another worktree");
    }

    let pre_land_sha = transaction.land.pre_land_sha.as_str();
    if transaction.land.target == DETACHED_TARGET {
        if head_symbolic_ref().is_some() {
            anyhow::bail!(
                "This detached undo is no longer on a detached HEAD; the current checkout was left untouched"
            );
        }
        let current = check_ref("HEAD").context("The detached HEAD no longer resolves")?;
        if current != pre_land_sha {
            if current != transaction.from_head {
                anyhow::bail!(
                    "Detached HEAD moved after undo began; its newer value was left untouched"
                );
            }
            execute_git(&[
                "update-ref",
                "--no-deref",
                "HEAD",
                pre_land_sha,
                &transaction.from_head,
            ])
            .context("Detached HEAD moved during undo and was left untouched")?;
        }
    } else {
        let branch = transaction.land.target.as_str();
        let branch_ref = format!("refs/heads/{branch}");
        if head_symbolic_ref().as_deref() != Some(branch_ref.as_str()) {
            anyhow::bail!(
                "This undo is no longer checked out on `{branch}`; the branch was left untouched"
            );
        }
        let current =
            check_ref(&branch_ref).with_context(|| format!("`{branch}` no longer exists"))?;
        if current != pre_land_sha {
            if current != transaction.from_head {
                anyhow::bail!(
                    "`{branch}` moved after undo began; its newer value was left untouched"
                );
            }
            execute_git(&[
                "update-ref",
                &branch_ref,
                pre_land_sha,
                &transaction.from_head,
            ])
            .with_context(|| {
                format!("`{branch}` moved during undo and its newer value was left untouched")
            })?;
        }
    }

    // A two-tree checkout can be repeated after a crash and preserves later
    // edits, refusing conflicting changes instead of overwriting them.
    execute_git(&["read-tree", "-m", "-u", &transaction.from_head, pre_land_sha])
        .context("Could not restore the saved tree without overwriting local changes. Save them outside this worktree or stash them, then rerun `kt undo`")?;
    Ok(())
}

pub(super) fn finish_completed_undo(transaction: &UndoTransaction) -> Result<()> {
    if check_ref(&transaction.land.keepalive_ref).as_deref()
        != Some(transaction.land.pre_land_sha.as_str())
    {
        anyhow::bail!("The completed land's keepalive ref moved; undo state was preserved");
    }

    let empty = AtomicLandRecord {
        version: LAND_STATE_VERSION,
        phase: AtomicLandPhase::Empty,
    };
    let empty_oid = write_land_record(&empty)?;
    let mut edits = vec![
        RefEdit::Update {
            name: LAND_STATE_REF.to_string(),
            new: empty_oid,
            old: transaction.state_oid.clone(),
        },
        RefEdit::Delete {
            name: transaction.land.keepalive_ref.clone(),
            old: transaction.land.pre_land_sha.clone(),
        },
    ];
    match check_ref(PRE_LAND_REF) {
        Some(current) if current == transaction.land.pre_land_sha => {
            edits.push(RefEdit::Delete {
                name: PRE_LAND_REF.to_string(),
                old: current,
            });
        }
        None => {}
        Some(_) => anyhow::bail!("Kite's rollback ref moved; undo state was preserved"),
    }
    commit_ref_transaction(&edits)
        .context("History was restored, but undo state could not be cleared")?;
    clear_legacy_marker_config();
    Ok(())
}

/// A recorded land target read back from config, which is a branch name or
/// `DETACHED_TARGET` — never a `Head`, because the commit a detached land
/// started from is not what the marker keeps.
pub(super) fn describe_landed_target(landed_target: &str) -> String {
    if landed_target == DETACHED_TARGET {
        "a detached HEAD".to_string()
    } else {
        format!("`{landed_target}`")
    }
}

/// How to get back to where a land happened. For a detached land that is the
/// commit it left `HEAD` on, which the marker records; without it there is no
/// name to offer, so the instruction stays honest about that.
pub(super) fn recover_landed_target(landed_target: &str, landed_head: Option<&str>) -> String {
    if landed_target != DETACHED_TARGET {
        return format!("Run `git switch {landed_target}` first");
    }

    match landed_head {
        Some(landed_head) => {
            format!("Check that commit out again with `git switch --detach {landed_head}`")
        }
        None => "Check out the commit that land left behind first".to_string(),
    }
}

/// Checks for a land that stopped after installing its rollback marker.
///
/// Returns `true` when this worktree owns an interrupted rewrite that requires
/// an explicit `kt undo`. The owner identifies the worktree, but not its
/// current detached history, so an arbitrary next command must never trigger a
/// reset.
#[cfg(test)]
pub(crate) fn heal_interrupted_land() -> bool {
    current_worktree_key()
        .is_ok_and(|owner| pre_land_state().recovery_owner() == Some(owner.as_str()))
}

/// Other worktrees can keep working, but only the owner can recover a rewrite.
pub(crate) fn recovery_blocks_commands() -> Result<bool> {
    let state = pre_land_state();
    if matches!(state, PreLandState::Inconsistent) {
        anyhow::bail!(
            "Kite's rollback marker is incomplete. Run `kt undo` to inspect it before doing anything else."
        );
    }
    Ok(stash::is_pending()? || state.recovery_owner() == Some(current_worktree_key()?.as_str()))
}

pub(super) fn ensure_no_land_in_progress() -> Result<()> {
    let state = pre_land_state();
    if matches!(state, PreLandState::Inconsistent) {
        anyhow::bail!(
            "Kite's rollback marker is incomplete. Inspect `{PRE_LAND_REF}` and `{LAND_STATE_REF}` before continuing."
        );
    }
    if let Some(owner) = state.recovery_owner() {
        let location = if current_worktree_key()? == owner {
            "this worktree"
        } else {
            "another worktree"
        };
        anyhow::bail!(
            "An earlier `kt land` is still in progress in {location} ({owner}). Run `kt undo` in its worktree before starting another land."
        );
    }
    Ok(())
}

pub(super) fn ensure_no_git_operation_in_progress(command: &str) -> Result<()> {
    if let Some(operation) = active_git_operation()? {
        anyhow::bail!(
            "Git has a {operation} in progress. Finish or abort it before running `{command}`."
        );
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
pub(super) fn restore_head(target: &Head, pre_land_sha: &str, transaction_ref: &str) -> Result<()> {
    let transaction_tip = check_ref(transaction_ref);
    let symbolic_head = head_symbolic_ref();
    let current_head = check_ref("HEAD");
    let current_branch = symbolic_head
        .as_deref()
        .and_then(|head| head.strip_prefix("refs/heads/"));
    let on_transaction = symbolic_head.as_deref() == Some(transaction_ref);

    let checkout_is_owned = if on_transaction {
        true
    } else {
        match target {
            Head::Branch(branch) => {
                current_branch == Some(branch)
                    || (current_branch.is_none()
                        && current_head.as_ref().is_some_and(|head| {
                            head == pre_land_sha || transaction_tip.as_ref() == Some(head)
                        }))
            }
            Head::Detached(_) => {
                current_branch.is_none()
                    && current_head.as_ref().is_some_and(|head| {
                        head == pre_land_sha || transaction_tip.as_ref() == Some(head)
                    })
            }
        }
    };
    if !checkout_is_owned {
        anyhow::bail!(
            "This worktree moved away from Kite's recorded land transaction. Its current HEAD was left untouched."
        );
    }

    if let Head::Branch(branch) = target {
        restore_branch_ref(branch, pre_land_sha, transaction_tip.as_deref())?;
    }

    if on_transaction {
        if has_head_commit() {
            execute_git(&["reset", "--mixed", pre_land_sha])?;
        } else {
            execute_git(&["read-tree", pre_land_sha])?;
        }
    }

    match target {
        Head::Branch(branch) => {
            if current_branch == Some(branch) {
                execute_git(&["reset", "--mixed", pre_land_sha])?;
            } else if branch_checked_out_elsewhere(branch)? {
                execute_git(&["checkout", "--detach", pre_land_sha])?;
                println!(
                    "{} `{branch}` is checked out in another worktree; recovery left this worktree detached at its saved commit",
                    "·".yellow()
                );
            } else {
                execute_git(&["checkout", branch])?;
            }
        }
        Head::Detached(_) => {
            if !on_transaction {
                execute_git(&["reset", "--mixed", pre_land_sha])?;
            }
            execute_git(&["checkout", "--detach", pre_land_sha])?;
        }
    }
    Ok(())
}

pub(super) fn restore_branch_ref(
    branch: &str,
    pre_land_sha: &str,
    candidate: Option<&str>,
) -> Result<()> {
    let branch_ref = format!("refs/heads/{branch}");
    let current = check_ref(&branch_ref).with_context(|| format!("`{branch}` no longer exists"))?;
    if current == pre_land_sha {
        execute_git(&["update-ref", &branch_ref, pre_land_sha, pre_land_sha])?;
        return Ok(());
    }
    if candidate == Some(current.as_str()) {
        execute_git(&["update-ref", &branch_ref, pre_land_sha, &current]).with_context(|| {
            format!("`{branch}` moved again during recovery and was left untouched")
        })?;
        return Ok(());
    }
    anyhow::bail!(
        "`{branch}` moved to {current} while Kite was landing. That newer value was left untouched."
    )
}

pub(super) fn branch_checked_out_elsewhere(branch: &str) -> Result<bool> {
    let branch_ref = format!("refs/heads/{branch}");
    let worktrees = execute_git(&["worktree", "list", "--porcelain"])?;
    Ok(worktrees
        .lines()
        .any(|line| line.strip_prefix("branch ") == Some(branch_ref.as_str())))
}
