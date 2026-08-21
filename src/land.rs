use anyhow::{Context, Result, anyhow};
use chrono::Local;
use colored::*;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

use crate::ai::flatten_error;
use crate::git::{
    DETACHED_TARGET, Head, Hooks, KiteBase, active_git_operation, apply_cached_patch,
    branch_to_publish, check_ref, commit_git, config_get, config_unset, current_worktree_key,
    diff_for_base, execute_git, execute_git_with, has_head_commit, has_remote, has_unmerged_paths,
    head_branch_hint, head_position, head_symbolic_ref, is_ancestor, is_save_subject,
    kite_save_stack, short_sha, subjects_missing_from_head,
};
use crate::hunks::{DiffUnits, FileStat, parse_diff};
use crate::synth::{
    CommitGroup, MAX_DIFF_BYTES, normalize_groups, sanitize_commit_message, synthesize_groups,
};
use crate::ui::{Spinner, confirm, pluralize, print_ai_unavailable, prompt_line};

/// Where the pre-land `HEAD` is parked so `kt undo` can restore it, plus the
/// local config keys recording which branch that land belongs to — or
/// `DETACHED_TARGET` when it was landed on a detached `HEAD` — where it left
/// `HEAD`, and which linked worktree owns the marker.
const PRE_LAND_REF: &str = "refs/kite/pre_land";
const PRE_LAND_BRANCH_KEY: &str = "kite.preland.branch";
const PRE_LAND_HEAD_KEY: &str = "kite.preland.head";
const PRE_LAND_WORKTREE_KEY: &str = "kite.preland.worktree";
/// Atomic, authoritative rollback metadata. The ref points to a JSON blob so
/// every field changes in one compare-and-swap ref transaction.
const LAND_STATE_REF: &str = "refs/kite/land_state";
const LAND_STATE_VERSION: u8 = 1;

#[derive(Clone, Debug, Serialize, Deserialize)]
struct CompletedLand {
    pre_land_sha: String,
    target: String,
    owner: Option<String>,
    landed_head: String,
    keepalive_ref: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum StableLand {
    Empty,
    Completed { land: CompletedLand },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct LandingLand {
    pre_land_sha: String,
    target: String,
    owner: String,
    transaction_ref: String,
    keepalive_ref: String,
    previous: StableLand,
}

/// One immutable object containing the whole rollback transaction. Updating a
/// ref to this blob with an expected old oid both serializes linked worktrees
/// and prevents crashes from exposing a mixture of old and new fields.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct AtomicLandRecord {
    version: u8,
    #[serde(flatten)]
    phase: AtomicLandPhase,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "phase", rename_all = "snake_case")]
enum AtomicLandPhase {
    Empty,
    Landing {
        land: LandingLand,
    },
    Completed {
        land: CompletedLand,
    },
    Undoing {
        land: CompletedLand,
        owner: String,
        from_head: String,
    },
}

#[derive(Clone, Debug)]
struct RecordedLand {
    state_oid: Option<String>,
    pre_land_sha: String,
    target: String,
    owner: Option<String>,
    landed_head: Option<String>,
    transaction_ref: Option<String>,
    keepalive_ref: Option<String>,
    previous: Option<StableLand>,
    undo_from: Option<String>,
}

#[derive(Debug)]
enum PreLandState {
    Empty { state_oid: Option<String> },
    Completed(RecordedLand),
    InProgress(RecordedLand),
    Undoing(RecordedLand),
    Inconsistent,
}

fn pre_land_state() -> PreLandState {
    match read_atomic_land_record() {
        Ok(Some((state_oid, AtomicLandPhase::Empty))) => {
            return PreLandState::Empty {
                state_oid: Some(state_oid),
            };
        }
        Ok(Some((state_oid, AtomicLandPhase::Completed { land }))) => {
            return PreLandState::Completed(recorded_completed(Some(state_oid), land));
        }
        Ok(Some((state_oid, AtomicLandPhase::Landing { land }))) => {
            return PreLandState::InProgress(RecordedLand {
                state_oid: Some(state_oid),
                pre_land_sha: land.pre_land_sha,
                target: land.target,
                owner: Some(land.owner),
                landed_head: None,
                transaction_ref: Some(land.transaction_ref),
                keepalive_ref: Some(land.keepalive_ref),
                previous: Some(land.previous),
                undo_from: None,
            });
        }
        Ok(Some((
            state_oid,
            AtomicLandPhase::Undoing {
                land,
                owner,
                from_head,
            },
        ))) => {
            let mut recorded = recorded_completed(Some(state_oid), land);
            recorded.owner = Some(owner);
            recorded.undo_from = Some(from_head);
            return PreLandState::Undoing(recorded);
        }
        Err(_) => return PreLandState::Inconsistent,
        Ok(None) => {}
    }

    // Backward-compatible fallback for markers written before the atomic state
    // ref existed. New writes always use the blob above as their authority.
    let sha = check_ref(PRE_LAND_REF);
    let target = config_get(PRE_LAND_BRANCH_KEY);
    let landed_head = config_get(PRE_LAND_HEAD_KEY);
    let owner = config_get(PRE_LAND_WORKTREE_KEY);

    match (sha, target, landed_head, owner) {
        (None, None, None, None) => PreLandState::Empty { state_oid: None },
        (Some(pre_land_sha), Some(target), Some(landed_head), owner) => {
            PreLandState::Completed(RecordedLand {
                state_oid: None,
                pre_land_sha,
                target,
                owner,
                landed_head: Some(landed_head),
                transaction_ref: None,
                keepalive_ref: None,
                previous: None,
                undo_from: None,
            })
        }
        (Some(pre_land_sha), Some(target), None, Some(owner)) => {
            PreLandState::InProgress(RecordedLand {
                state_oid: None,
                pre_land_sha,
                target,
                owner: Some(owner),
                landed_head: None,
                transaction_ref: None,
                keepalive_ref: None,
                previous: None,
                undo_from: None,
            })
        }
        _ => PreLandState::Inconsistent,
    }
}

fn recorded_completed(state_oid: Option<String>, land: CompletedLand) -> RecordedLand {
    RecordedLand {
        state_oid,
        pre_land_sha: land.pre_land_sha,
        target: land.target,
        owner: land.owner,
        landed_head: Some(land.landed_head),
        transaction_ref: None,
        keepalive_ref: Some(land.keepalive_ref),
        previous: None,
        undo_from: None,
    }
}

fn read_atomic_land_record() -> Result<Option<(String, AtomicLandPhase)>> {
    let Some(state_oid) = check_ref(LAND_STATE_REF) else {
        return Ok(None);
    };
    let json = execute_git(&["cat-file", "blob", &state_oid])
        .context("Could not read Kite's atomic land marker")?;
    let record: AtomicLandRecord =
        serde_json::from_str(&json).context("Kite's atomic land marker is not valid JSON")?;

    if record.version != LAND_STATE_VERSION || !valid_atomic_phase(&record.phase) {
        anyhow::bail!("Kite's atomic land marker has an unsupported or incomplete shape");
    }

    Ok(Some((state_oid, record.phase)))
}

fn valid_atomic_phase(phase: &AtomicLandPhase) -> bool {
    let valid_completed = |land: &CompletedLand| {
        !land.pre_land_sha.is_empty()
            && !land.target.is_empty()
            && !land.landed_head.is_empty()
            && land.keepalive_ref.starts_with("refs/kite/keepalive/")
    };

    match phase {
        AtomicLandPhase::Empty => true,
        AtomicLandPhase::Completed { land } => valid_completed(land),
        AtomicLandPhase::Undoing {
            land,
            owner,
            from_head,
        } => valid_completed(land) && !owner.is_empty() && !from_head.is_empty(),
        AtomicLandPhase::Landing { land } => {
            !land.pre_land_sha.is_empty()
                && !land.target.is_empty()
                && !land.owner.is_empty()
                && land.transaction_ref.starts_with(TRANSACTION_REF_PREFIX)
                && land.keepalive_ref.starts_with("refs/kite/keepalive/")
                && match &land.previous {
                    StableLand::Empty => true,
                    StableLand::Completed { land } => valid_completed(land),
                }
        }
    }
}

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

        let mut commits = match synthesized {
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

        if let Some(tag) = tag.as_deref().filter(|s| !s.trim().is_empty()) {
            for commit in &mut commits {
                commit.message = append_tag_to_message(&commit.message, tag);
            }
        }

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
    ensure_no_git_operation_in_progress("kt undo")?;

    // An interrupted rewrite takes precedence over the save commit that Kite
    // was in the middle of replacing. Otherwise `kt undo` would peel saves
    // from the recorded target while leaving the interrupted transaction
    // behind.
    match pre_land_state() {
        PreLandState::InProgress(_) => {
            if recover_interrupted_land()? {
                return Ok(());
            }
        }
        PreLandState::Undoing(_) => return undo_last_land(),
        PreLandState::Inconsistent => anyhow::bail!(
            "Kite's rollback marker is incomplete, so it cannot be undone safely. Inspect `{PRE_LAND_REF}` and `{LAND_STATE_REF}`."
        ),
        PreLandState::Empty { .. } | PreLandState::Completed(_) => {}
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

/// Cancels an owned land that stopped between recording its rollback state
/// and recording its final `HEAD`.
///
/// Recovery is deliberately explicit. The worktree id proves where Kite was
/// running, but it cannot prove that a different detached commit checked out
/// later is still one of Kite's partial commits. Requiring `kt undo` keeps an
/// ordinary command from silently resetting that newer checkout.
fn recover_interrupted_land() -> Result<bool> {
    let PreLandState::InProgress(recorded) = pre_land_state() else {
        return Ok(false);
    };

    let owner = recorded
        .owner
        .as_deref()
        .context("This interrupted land has no recorded worktree owner")?;
    let current_worktree = current_worktree_key()?;
    if owner != current_worktree {
        anyhow::bail!(
            "An interrupted `kt land` belongs to another worktree ({owner}). Run `kt undo` there; this worktree was not changed."
        );
    }
    let state_oid = recorded
        .state_oid
        .clone()
        .context("This legacy interrupted land has no atomic transaction state")?;
    let transaction_ref = recorded.transaction_ref.clone().context(
        "This legacy interrupted land has no exact transaction ref, so Kite cannot recover it safely",
    )?;
    let keepalive_ref = recorded
        .keepalive_ref
        .clone()
        .context("This interrupted land has no keepalive ref")?;
    let previous = recorded
        .previous
        .clone()
        .context("This interrupted land has no previous stable state")?;
    let restore_target = if recorded.target == DETACHED_TARGET {
        Head::Detached(recorded.pre_land_sha.clone())
    } else {
        Head::Branch(recorded.target.clone())
    };
    let transaction = LandTransaction {
        state_oid,
        landing: LandingLand {
            pre_land_sha: recorded.pre_land_sha.clone(),
            target: recorded.target,
            owner: owner.to_string(),
            transaction_ref: transaction_ref.clone(),
            keepalive_ref,
            previous,
        },
    };

    restore_head(
        &restore_target,
        &recorded.pre_land_sha,
        Some(&transaction_ref),
    )
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

fn undo_last_land() -> Result<()> {
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
        PreLandState::Undoing(recorded) => {
            let owner = recorded
                .owner
                .as_deref()
                .context("The interrupted undo has no worktree owner")?;
            if owner != current_worktree_key()? {
                anyhow::bail!(
                    "An interrupted `kt undo` belongs to another worktree ({owner}). Finish it there instead."
                );
            }
            let target = recorded.target.clone();
            (UndoTransaction::from_recorded(recorded)?, target)
        }
        PreLandState::InProgress(_) => {
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
fn guard_undo_target(head: &Head, recorded: &RecordedLand) -> Result<String> {
    if recorded.target != head.land_key() {
        anyhow::bail!(
            "The last land was on {}, but you are on {}. {} — undoing here would reset unrelated history.",
            describe_landed_target(&recorded.target),
            head.describe(),
            recover_landed_target(&recorded.target, recorded.landed_head.as_deref())
        );
    }

    // A branch name identifies its target across worktrees. "Detached" does
    // not: every detached linked worktree would otherwise look identical and
    // could consume another worktree's rollback marker.
    if matches!(head, Head::Detached(_)) {
        let Some(landed_worktree) = recorded.owner.as_deref() else {
            anyhow::bail!(
                "The last detached land has no worktree recorded, so Kite cannot undo it safely. {} and inspect it before retrying.",
                recover_landed_target(&recorded.target, recorded.landed_head.as_deref())
            );
        };
        let current_worktree = current_worktree_key()?;
        if landed_worktree != current_worktree {
            anyhow::bail!(
                "The last detached land belongs to another worktree. Run `kt undo` there instead; its landed commit is recorded at {}.",
                recorded
                    .landed_head
                    .as_deref()
                    .unwrap_or("an unknown commit")
            );
        }
    }

    // Landing recorded where it left HEAD. If HEAD moved since, undo would
    // throw away whatever was committed on top of the landed commits.
    let landed_head = recorded
        .landed_head
        .as_deref()
        .context("The completed land has no recorded landed HEAD")?;
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

#[derive(Clone, Debug)]
struct UndoTransaction {
    state_oid: String,
    land: CompletedLand,
    owner: String,
    from_head: String,
}

impl UndoTransaction {
    fn from_recorded(recorded: RecordedLand) -> Result<Self> {
        let state_oid = recorded
            .state_oid
            .context("The interrupted undo has no atomic state")?;
        let owner = recorded
            .owner
            .context("The interrupted undo has no worktree owner")?;
        let landed_head = recorded
            .landed_head
            .context("The interrupted undo has no recorded landed HEAD")?;
        let keepalive_ref = recorded
            .keepalive_ref
            .context("The interrupted undo has no keepalive ref")?;
        let from_head = recorded
            .undo_from
            .context("The interrupted undo has no recorded starting HEAD")?;

        Ok(Self {
            state_oid,
            land: CompletedLand {
                pre_land_sha: recorded.pre_land_sha,
                target: recorded.target,
                // The transition owner is authoritative while an undo is in
                // progress. The completed marker's original owner is no
                // longer needed to finish this exact transaction.
                owner: Some(owner.clone()),
                landed_head,
                keepalive_ref,
            },
            owner,
            from_head,
        })
    }
}

/// Claims a completed marker before moving any history. The CAS means a
/// concurrent land or undo cannot replace the marker between validation and
/// reset, and `from_head` makes crash recovery use the exact value the user
/// approved rather than whatever the branch happens to contain later.
fn begin_completed_undo(recorded: RecordedLand, from_head: &str) -> Result<UndoTransaction> {
    let owner = current_worktree_key()?;
    let landed_head = recorded
        .landed_head
        .clone()
        .context("The completed land has no recorded landed HEAD")?;
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
fn restore_completed_land(transaction: &UndoTransaction) -> Result<()> {
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

    // The ref may already have been restored by a process that crashed before
    // updating the worktree. This step is idempotent and never moves a ref.
    execute_git(&["read-tree", "--reset", "-u", pre_land_sha])?;
    Ok(())
}

fn finish_completed_undo(transaction: &UndoTransaction) -> Result<()> {
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

fn unique_kite_ref(prefix: &str) -> String {
    format!(
        "{prefix}{}-{}-{}",
        Local::now().format("%Y%m%d%H%M%S"),
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.subsec_nanos())
            .unwrap_or(0)
    )
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
fn recover_landed_target(landed_target: &str, landed_head: Option<&str>) -> String {
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

const TRANSACTION_REF_PREFIX: &str = "refs/heads/kite-landing-";

/// Checks for a land that stopped after installing its rollback marker.
///
/// Returns `true` when this worktree owns an interrupted rewrite that requires
/// an explicit `kt undo`. The owner identifies the worktree, but not its
/// current detached history, so an arbitrary next command must never trigger a
/// reset.
#[cfg(test)]
pub(crate) fn heal_interrupted_land() -> bool {
    let recorded = match pre_land_state() {
        PreLandState::InProgress(recorded) | PreLandState::Undoing(recorded) => recorded,
        _ => return false,
    };

    let Ok(current_worktree) = current_worktree_key() else {
        return false;
    };
    recorded.owner.as_deref() == Some(current_worktree.as_str())
}

/// Dispatch guard for commands other than `kt undo`. An owned transaction
/// blocks this worktree until explicit recovery, while a malformed marker
/// blocks everywhere because no command can safely infer what it represents.
/// A well-formed transaction owned by another linked worktree is left alone.
pub(crate) fn recovery_blocks_commands() -> Result<bool> {
    match pre_land_state() {
        PreLandState::Inconsistent => anyhow::bail!(
            "Kite's rollback marker is incomplete. Run `kt undo` to inspect it before doing anything else."
        ),
        PreLandState::InProgress(recorded) | PreLandState::Undoing(recorded) => {
            Ok(recorded.owner.as_deref() == Some(current_worktree_key()?.as_str()))
        }
        PreLandState::Empty { .. } | PreLandState::Completed(_) => Ok(false),
    }
}

/// A failed automatic recovery must not be overwritten by a fresh land. This
/// also keeps one linked worktree from replacing another worktree's in-progress
/// marker after `heal_interrupted_land` deliberately left it alone.
fn ensure_no_land_in_progress() -> Result<()> {
    match pre_land_state() {
        PreLandState::Empty { .. } | PreLandState::Completed(_) => Ok(()),
        PreLandState::InProgress(recorded) | PreLandState::Undoing(recorded) => {
            let owner = recorded.owner.as_deref().unwrap_or("an unknown worktree");
            let detail = match current_worktree_key() {
                Ok(current) if current != owner => format!(" in another worktree ({owner})"),
                _ => " in this worktree".to_string(),
            };
            anyhow::bail!(
                "An earlier `kt land` is still in progress{detail}. Run `kt undo` in its worktree before starting another land."
            )
        }
        PreLandState::Inconsistent => anyhow::bail!(
            "Kite's rollback marker is incomplete, so a new land could overwrite recovery state. Inspect `{PRE_LAND_REF}` and the `kite.preland.*` local config before continuing."
        ),
    }
}

fn ensure_no_git_operation_in_progress(command: &str) -> Result<()> {
    if let Some(operation) = active_git_operation()? {
        anyhow::bail!(
            "Git has a {operation} in progress. Finish or abort it before running `{command}`."
        );
    }
    Ok(())
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
fn append_tag_to_message(message: &str, tag: &str) -> String {
    let tag_token = tag.trim();
    if tag_token.is_empty() {
        return message.to_string();
    }

    let mut lines = message.lines();
    let first = lines.next().unwrap_or("").trim_end();

    // Allocate the suffix once; only allocate/join the body when it exists.
    let suffix = format!(" [{tag_token}]");
    if first.ends_with(&suffix) {
        return message.to_string();
    }

    let new_first = if first.is_empty() {
        format!("[{tag_token}]")
    } else {
        format!("{first}{suffix}")
    };

    // If there is no body, we're done.
    let Some(first_body_line) = lines.next() else {
        return new_first;
    };

    let mut rest_lines = Vec::new();
    rest_lines.push(first_body_line);
    rest_lines.extend(lines.collect::<Vec<_>>());

    format!("{new_first}\n{}", rest_lines.join("\n"))
}

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

fn execute_land(base: &KiteBase, commits: &[LandCommit], hooks: Hooks) -> Result<()> {
    if commits.is_empty() {
        anyhow::bail!("Refusing to land an empty plan — that would discard the saves.");
    }

    // Most callers pass through preflight, but keep the history-mutating core
    // safe when invoked directly by tests or future commands.
    ensure_no_git_operation_in_progress("kt land")?;
    ensure_no_land_in_progress()?;

    // Resolved before anything is rewritten, so the landed commits have a
    // recorded home: a branch to move, or the detached HEAD to leave sitting on
    // them. Discovering it afterwards would mean unwinding a rewrite that had
    // already happened.
    let target = head_position()?;
    let worktree = current_worktree_key()?;
    let pre_land_sha = execute_git(&["rev-parse", "HEAD"])?;
    // Only ever used by a root rewrite, which is the one case that cannot build
    // its commits on a detached HEAD.
    let transaction_ref = unique_kite_ref(TRANSACTION_REF_PREFIX);

    // Kept so a land that fails cannot overwrite the rollback marker left by
    // the last one that succeeded.
    let previous_marker = PreLandMarker::capture();
    let transaction = install_in_progress_marker(
        &previous_marker,
        pre_land_sha.trim(),
        &target.land_key(),
        &worktree,
        Some(&transaction_ref),
    )?;

    if let Err(err) =
        prepare_landing_head(base, pre_land_sha.trim(), &transaction_ref).and_then(|_| {
            create_commits(commits, hooks)?;
            finalize_landed_head(&target, pre_land_sha.trim(), &transaction_ref)
        })
    {
        // A failed land is almost always a hook rejecting a commit, and the
        // fix belongs where the user already was. Nothing they wrote is at
        // stake — their saves are still reachable from `target`, which is not
        // moved until the final step, and landing stages through the index
        // without ever writing the worktree — so put them back instead of
        // stranding them on a pile of derived commits.
        return Err(
            match restore_head(&target, pre_land_sha.trim(), Some(&transaction_ref)) {
                Ok(()) => {
                    if let Err(marker_error) = restore_previous_marker(&transaction) {
                        anyhow!(
                            "{}\n\nKite restored your history but could not restore the previous rollback marker: {marker_error:#}",
                            render_recovered_failure(&err, &target)
                        )
                    } else {
                        anyhow!("{}", render_recovered_failure(&err, &target))
                    }
                }
                Err(restore_error) => {
                    anyhow!("{}", render_stranded_failure(&err, &target, &restore_error))
                }
            },
        );
    }

    let completion = execute_git(&["rev-parse", "HEAD"])
        .context("Could not resolve the landed HEAD")
        .and_then(|landed_head| {
            record_landed_head(&transaction, landed_head.trim())
                .context("Could not record the landed HEAD for `kt undo`")
        });
    if let Err(error) = completion {
        // A land is not complete until its rollback marker says where it left
        // HEAD. In particular, `kt land --push` must never publish while the
        // marker still looks interrupted: a later command could otherwise
        // rewind a history that was already sent to the remote.
        return match restore_head(&target, pre_land_sha.trim(), Some(&transaction_ref)) {
            Ok(()) => match restore_previous_marker(&transaction) {
                Ok(()) => Err(anyhow!(
                    "{error:#}\n\nThe rewrite was undone because Kite could not record safe rollback state. You are back on {} with every save intact.",
                    target.describe()
                )),
                Err(marker_error) => Err(anyhow!(
                    "{error:#}\n\nThe rewrite was undone, but Kite could not restore the previous rollback marker: {marker_error:#}"
                )),
            },
            Err(restore_error) => Err(anyhow!(
                "{error:#}\n\nKite also could not restore {}: {restore_error:#}\nRollback ref: `{PRE_LAND_REF}`.",
                target.describe()
            )),
        };
    }

    Ok(())
}

fn record_landed_head(transaction: &LandTransaction, landed_head: &str) -> Result<()> {
    #[cfg(test)]
    if FAIL_LANDED_HEAD_WRITE.swap(false, std::sync::atomic::Ordering::SeqCst) {
        anyhow::bail!("injected landed-head marker failure");
    }

    let completed_land = CompletedLand {
        pre_land_sha: transaction.landing.pre_land_sha.clone(),
        target: transaction.landing.target.clone(),
        owner: Some(transaction.landing.owner.clone()),
        landed_head: landed_head.to_string(),
        keepalive_ref: transaction.landing.keepalive_ref.clone(),
    };
    let completed = AtomicLandRecord {
        version: LAND_STATE_VERSION,
        phase: AtomicLandPhase::Completed {
            land: completed_land,
        },
    };
    let completed_oid = write_land_record(&completed)?;
    let transaction_tip = check_ref(&transaction.landing.transaction_ref)
        .context("Kite's land transaction ref disappeared before completion")?;
    if transaction_tip != landed_head {
        anyhow::bail!(
            "Kite's land transaction ref moved unexpectedly; refusing to record a different landed HEAD"
        );
    }

    let mut edits = vec![
        RefEdit::Update {
            name: LAND_STATE_REF.to_string(),
            new: completed_oid,
            old: transaction.state_oid.clone(),
        },
        RefEdit::Delete {
            name: transaction.landing.transaction_ref.clone(),
            old: landed_head.to_string(),
        },
    ];
    if let StableLand::Completed { land } = &transaction.landing.previous {
        edits.push(RefEdit::Delete {
            name: land.keepalive_ref.clone(),
            old: land.pre_land_sha.clone(),
        });
    }
    commit_ref_transaction(&edits)?;
    clear_legacy_marker_config();
    Ok(())
}

#[cfg(test)]
static FAIL_LANDED_HEAD_WRITE: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Undoes a failed or interrupted landing attempt, leaving the user exactly
/// where they started.
///
/// Safe by construction: a branch target is only moved by the final step, so
/// while landing is in progress it still points at every save, and the pre-land
/// sha holds them for a detached one. A *mixed* reset is deliberate — it clears
/// the half-staged index but leaves the worktree alone, so files a pre-commit
/// hook rewrote (a formatter, say) survive.
fn restore_head(target: &Head, pre_land_sha: &str, landing_branch: Option<&str>) -> Result<()> {
    let transaction_ref = landing_branch.context(
        "This interrupted land has no exact transaction ref, so Kite cannot recover it automatically",
    )?;
    let transaction_tip = check_ref(transaction_ref);
    let symbolic_head = head_symbolic_ref();
    let current_head = check_ref("HEAD");
    let current_branch = head_branch_hint();
    let on_transaction = symbolic_head.as_deref() == Some(transaction_ref);

    let checkout_is_owned = if on_transaction {
        true
    } else {
        match target {
            Head::Branch(branch) => {
                current_branch.as_deref() == Some(branch)
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
            if current_branch.as_deref() == Some(branch) {
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

fn restore_branch_ref(branch: &str, pre_land_sha: &str, candidate: Option<&str>) -> Result<()> {
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

fn branch_checked_out_elsewhere(branch: &str) -> Result<bool> {
    let branch_ref = format!("refs/heads/{branch}");
    let worktrees = execute_git(&["worktree", "list", "--porcelain"])?;
    Ok(worktrees
        .lines()
        .any(|line| line.strip_prefix("branch ") == Some(branch_ref.as_str())))
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

fn install_in_progress_marker(
    previous: &PreLandMarker,
    pre_land_sha: &str,
    target: &str,
    worktree: &str,
    transaction_ref: Option<&str>,
) -> Result<LandTransaction> {
    let transaction_ref = transaction_ref.context("A land transaction ref was not generated")?;
    let transaction_id = transaction_ref
        .strip_prefix(TRANSACTION_REF_PREFIX)
        .context("Kite generated an invalid land transaction ref")?;
    let keepalive_ref = format!("refs/kite/keepalive/{transaction_id}");
    let previous_keepalive_ref = format!("{keepalive_ref}-previous");
    let (stable, create_previous_keepalive) = previous.stable(&previous_keepalive_ref)?;

    let landing = LandingLand {
        pre_land_sha: pre_land_sha.to_string(),
        target: target.to_string(),
        owner: worktree.to_string(),
        transaction_ref: transaction_ref.to_string(),
        keepalive_ref: keepalive_ref.clone(),
        previous: stable,
    };
    let state = AtomicLandRecord {
        version: LAND_STATE_VERSION,
        phase: AtomicLandPhase::Landing {
            land: landing.clone(),
        },
    };
    let state_oid = write_land_record(&state)?;

    let mut edits = Vec::new();
    edits.push(match previous.state_oid.as_deref() {
        Some(old) => RefEdit::Update {
            name: LAND_STATE_REF.to_string(),
            new: state_oid.clone(),
            old: old.to_string(),
        },
        None => RefEdit::Create {
            name: LAND_STATE_REF.to_string(),
            new: state_oid.clone(),
        },
    });
    edits.push(RefEdit::Create {
        name: transaction_ref.to_string(),
        new: pre_land_sha.to_string(),
    });
    edits.push(RefEdit::Create {
        name: keepalive_ref,
        new: pre_land_sha.to_string(),
    });
    if let Some(previous_sha) = create_previous_keepalive {
        edits.push(RefEdit::Create {
            name: previous_keepalive_ref,
            new: previous_sha,
        });
    }
    edits.push(match previous.sha.as_deref() {
        Some(old) => RefEdit::Update {
            name: PRE_LAND_REF.to_string(),
            new: pre_land_sha.to_string(),
            old: old.to_string(),
        },
        None => RefEdit::Create {
            name: PRE_LAND_REF.to_string(),
            new: pre_land_sha.to_string(),
        },
    });

    commit_ref_transaction(&edits)
        .context("Could not install rollback state; no history changed")?;
    clear_legacy_marker_config();

    Ok(LandTransaction { state_oid, landing })
}

/// The rollback marker as it stood before a land began.
struct PreLandMarker {
    state_oid: Option<String>,
    sha: Option<String>,
    branch: Option<String>,
    head: Option<String>,
    worktree: Option<String>,
    recorded: Option<RecordedLand>,
}

impl PreLandMarker {
    fn capture() -> Self {
        let state = pre_land_state();
        let (state_oid, recorded) = match state {
            PreLandState::Empty { state_oid } => (state_oid, None),
            PreLandState::Completed(recorded) => (recorded.state_oid.clone(), Some(recorded)),
            PreLandState::InProgress(_) | PreLandState::Undoing(_) | PreLandState::Inconsistent => {
                (None, None)
            }
        };
        Self {
            state_oid,
            sha: check_ref(PRE_LAND_REF),
            branch: config_get(PRE_LAND_BRANCH_KEY),
            head: config_get(PRE_LAND_HEAD_KEY),
            worktree: config_get(PRE_LAND_WORKTREE_KEY),
            recorded,
        }
    }

    fn stable(&self, legacy_keepalive_ref: &str) -> Result<(StableLand, Option<String>)> {
        let Some(recorded) = &self.recorded else {
            if self.sha.is_none()
                && self.branch.is_none()
                && self.head.is_none()
                && self.worktree.is_none()
            {
                return Ok((StableLand::Empty, None));
            }
            anyhow::bail!("Kite's previous rollback marker is incomplete");
        };

        let landed_head = recorded
            .landed_head
            .clone()
            .context("Kite's previous completed marker has no landed HEAD")?;
        let (keepalive_ref, create_keepalive) = match &recorded.keepalive_ref {
            Some(existing) => (existing.clone(), None),
            None => (
                legacy_keepalive_ref.to_string(),
                Some(recorded.pre_land_sha.clone()),
            ),
        };
        Ok((
            StableLand::Completed {
                land: CompletedLand {
                    pre_land_sha: recorded.pre_land_sha.clone(),
                    target: recorded.target.clone(),
                    owner: recorded.owner.clone(),
                    landed_head,
                    keepalive_ref,
                },
            },
            create_keepalive,
        ))
    }
}

#[derive(Clone, Debug)]
struct LandTransaction {
    state_oid: String,
    landing: LandingLand,
}

#[derive(Clone, Debug)]
enum RefEdit {
    Create {
        name: String,
        new: String,
    },
    Update {
        name: String,
        new: String,
        old: String,
    },
    Delete {
        name: String,
        old: String,
    },
}

fn write_land_record(record: &AtomicLandRecord) -> Result<String> {
    let json = serde_json::to_string(record).context("Could not serialize Kite's land marker")?;
    let oid = execute_git_with(&["hash-object", "-w", "--stdin"], &[], Some(&json))?;
    let oid = oid.trim();
    if oid.is_empty() {
        anyhow::bail!("Git did not return an object id for Kite's land marker");
    }
    Ok(oid.to_string())
}

fn commit_ref_transaction(edits: &[RefEdit]) -> Result<()> {
    let mut input = String::from("start\n");
    for edit in edits {
        match edit {
            RefEdit::Create { name, new } => {
                input.push_str(&format!("create {name} {new}\n"));
            }
            RefEdit::Update { name, new, old } => {
                input.push_str(&format!("update {name} {new} {old}\n"));
            }
            RefEdit::Delete { name, old } => {
                input.push_str(&format!("delete {name} {old}\n"));
            }
        }
    }
    input.push_str("prepare\ncommit\n");
    execute_git_with(&["update-ref", "--stdin"], &[], Some(&input)).map(|_| ())
}

fn restore_previous_marker(transaction: &LandTransaction) -> Result<()> {
    let previous_phase = match &transaction.landing.previous {
        StableLand::Empty => AtomicLandPhase::Empty,
        StableLand::Completed { land } => {
            if check_ref(&land.keepalive_ref).as_deref() != Some(&land.pre_land_sha) {
                anyhow::bail!("The previous land's keepalive ref moved; recovery stopped safely");
            }
            AtomicLandPhase::Completed { land: land.clone() }
        }
    };
    let previous_state = AtomicLandRecord {
        version: LAND_STATE_VERSION,
        phase: previous_phase,
    };
    let previous_state_oid = write_land_record(&previous_state)?;
    let mut edits = vec![RefEdit::Update {
        name: LAND_STATE_REF.to_string(),
        new: previous_state_oid,
        old: transaction.state_oid.clone(),
    }];

    if let Some(transaction_tip) = check_ref(&transaction.landing.transaction_ref) {
        edits.push(RefEdit::Delete {
            name: transaction.landing.transaction_ref.clone(),
            old: transaction_tip,
        });
    }
    if check_ref(&transaction.landing.keepalive_ref).as_deref()
        != Some(&transaction.landing.pre_land_sha)
    {
        anyhow::bail!("Kite's current land keepalive ref moved; recovery stopped safely");
    }
    edits.push(RefEdit::Delete {
        name: transaction.landing.keepalive_ref.clone(),
        old: transaction.landing.pre_land_sha.clone(),
    });

    let current_pre_land = check_ref(PRE_LAND_REF);
    let previous_pre_land = match &transaction.landing.previous {
        StableLand::Empty => None,
        StableLand::Completed { land } => Some(land.pre_land_sha.clone()),
    };
    match (current_pre_land, previous_pre_land) {
        (Some(current), Some(previous))
            if current == transaction.landing.pre_land_sha || current == previous =>
        {
            edits.push(RefEdit::Update {
                name: PRE_LAND_REF.to_string(),
                new: previous,
                old: current,
            });
        }
        (None, Some(previous)) => edits.push(RefEdit::Create {
            name: PRE_LAND_REF.to_string(),
            new: previous,
        }),
        (Some(current), None) if current == transaction.landing.pre_land_sha => {
            edits.push(RefEdit::Delete {
                name: PRE_LAND_REF.to_string(),
                old: current,
            });
        }
        (None, None) => {}
        _ => anyhow::bail!("Kite's rollback ref moved during recovery; it was left untouched"),
    }

    commit_ref_transaction(&edits)?;
    clear_legacy_marker_config();
    Ok(())
}

fn clear_legacy_marker_config() {
    let _ = config_unset(PRE_LAND_BRANCH_KEY);
    let _ = config_unset(PRE_LAND_HEAD_KEY);
    let _ = config_unset(PRE_LAND_WORKTREE_KEY);
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

fn create_commits(commits: &[LandCommit], hooks: Hooks) -> Result<()> {
    for commit in commits {
        match &commit.stage {
            StageOp::Patch(patch) => apply_cached_patch(patch)?,
            StageOp::WholeFiles(files) => {
                for file in files {
                    execute_git(&["add", "--", file])?;
                }
            }
        }
        commit_git(&commit.message, hooks)?;
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
mod tests {
    use super::*;
    use crate::test_support::{
        TempDir, acquire_cwd_lock, git, init_repo, init_root_kite_repo, with_repo_cwd, write_file,
    };

    fn detached_worktree(repo: &std::path::Path, revision: &str) -> (TempDir, std::path::PathBuf) {
        let holder = TempDir::new("kite-linked-worktree");
        let checkout = holder.path.join("checkout");
        let checkout_arg = checkout
            .to_str()
            .expect("temporary worktree path should be UTF-8");
        git(
            repo,
            &["worktree", "add", "-q", "--detach", checkout_arg, revision],
        );
        (holder, checkout)
    }

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
        with_repo_cwd(repo, || execute_land(base, commits, Hooks::Run))
    }

    fn undo_in_repo(repo: &std::path::Path) -> Result<()> {
        with_repo_cwd(repo, undo)
    }

    fn leave_interrupted_land(repo: &std::path::Path, base: &KiteBase) -> LandTransaction {
        with_repo_cwd(repo, || {
            let target = head_position().expect("target should resolve");
            let owner = current_worktree_key().expect("worktree identity should resolve");
            let pre_land_sha =
                execute_git(&["rev-parse", "HEAD"]).expect("pre-land HEAD should resolve");
            let transaction_ref = unique_kite_ref(TRANSACTION_REF_PREFIX);
            let previous = PreLandMarker::capture();
            let transaction = install_in_progress_marker(
                &previous,
                pre_land_sha.trim(),
                &target.land_key(),
                &owner,
                Some(&transaction_ref),
            )
            .expect("atomic marker should install");
            prepare_landing_head(base, pre_land_sha.trim(), &transaction_ref)
                .expect("partial rewrite should begin");
            execute_git(&["add", "tracked.txt"]).expect("partial file should stage");
            execute_git(&["commit", "-qm", "feat: half a landing"])
                .expect("partial commit should be created");
            transaction
        })
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

    fn install_pre_commit_hook(repo: &std::path::Path, script: &str) {
        write_file(repo, ".git/hooks/pre-commit", script);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(
                repo.join(".git/hooks/pre-commit"),
                std::fs::Permissions::from_mode(0o755),
            )
            .expect("hook should be executable");
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
    fn append_tag_to_message_appends_suffix() {
        assert_eq!(
            append_tag_to_message("feat: add thing", "PROJ-123"),
            "feat: add thing [PROJ-123]"
        );
    }

    #[test]
    fn append_tag_to_message_is_idempotent() {
        assert_eq!(
            append_tag_to_message("feat: add thing [PROJ-123]", "PROJ-123"),
            "feat: add thing [PROJ-123]"
        );
    }

    #[test]
    fn append_tag_to_message_preserves_body() {
        assert_eq!(
            append_tag_to_message("feat: add thing\n\nBody line", "PROJ-123"),
            "feat: add thing [PROJ-123]\n\nBody line"
        );
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

        let recorded = with_repo_cwd(&repo.path, pre_land_state);
        let PreLandState::Completed(recorded) = recorded else {
            panic!("detached land should leave a completed marker");
        };
        assert_eq!(recorded.target, DETACHED_TARGET);
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
    fn execute_land_works_in_a_linked_detached_worktree() {
        let _lock = acquire_cwd_lock();
        let repo = init_repo();
        let branch = git(&repo.path, &["branch", "--show-current"])
            .trim()
            .to_string();

        write_file(&repo.path, "tracked.txt", "saved through a worktree\n");
        git(&repo.path, &["add", "tracked.txt"]);
        git(&repo.path, &["commit", "-m", "[kite] save 12:00:00"]);
        let pre_land_sha = git(&repo.path, &["rev-parse", "HEAD"]);
        let pre_land_tree = git(&repo.path, &["rev-parse", "HEAD^{tree}"]);

        let (_holder, linked) = detached_worktree(&repo.path, "HEAD");
        let scope = collect_land_scope_in_repo(&linked, false)
            .expect("land scope should collect in a linked worktree")
            .expect("kite saves should be landable");
        execute_land_in_repo(
            &linked,
            &scope.base,
            &[files_commit(
                "feat: land linked worktree change",
                &["tracked.txt"],
            )],
        )
        .expect("a linked detached worktree should land");

        assert!(
            with_repo_cwd(&linked, head_branch_hint).is_none(),
            "landing attached the linked worktree to a branch"
        );
        assert_eq!(
            git(&linked, &["log", "-1", "--pretty=%s"]).trim(),
            "feat: land linked worktree change"
        );
        assert_eq!(git(&linked, &["rev-parse", "HEAD^{tree}"]), pre_land_tree);
        assert_eq!(
            git(&repo.path, &["rev-parse", &branch]),
            pre_land_sha,
            "landing in the linked worktree moved the primary branch"
        );

        let recorded = with_repo_cwd(&linked, pre_land_state);
        let PreLandState::Completed(recorded) = recorded else {
            panic!("linked detached land should leave a completed marker");
        };
        let recorded_owner = recorded
            .owner
            .expect("completed marker should record its owner");
        let actual_owner = with_repo_cwd(&linked, current_worktree_key)
            .expect("linked worktree identity should resolve");
        assert_eq!(recorded_owner, actual_owner);
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
    fn an_interrupted_detached_land_requires_explicit_undo() {
        let _lock = acquire_cwd_lock();
        let repo = init_repo();

        write_file(&repo.path, "tracked.txt", "saved change\n");
        git(&repo.path, &["add", "tracked.txt"]);
        git(&repo.path, &["commit", "-m", "[kite] save 12:00:00"]);
        git(&repo.path, &["checkout", "-q", "--detach"]);
        let saves_head = git(&repo.path, &["rev-parse", "HEAD"]);

        let base = KiteBase::Commit(git(&repo.path, &["rev-parse", "HEAD~1"]).trim().to_string());
        leave_interrupted_land(&repo.path, &base);

        let partial_head = git(&repo.path, &["rev-parse", "HEAD"]);
        assert!(
            with_repo_cwd(&repo.path, heal_interrupted_land),
            "an ambiguous partial rewrite should require explicit recovery"
        );
        assert_eq!(git(&repo.path, &["rev-parse", "HEAD"]), partial_head);

        undo_in_repo(&repo.path).expect("explicit undo should recover the interrupted land");

        assert!(
            with_repo_cwd(&repo.path, head_branch_hint).is_none(),
            "recovering a detached land must not attach HEAD to a branch"
        );
        assert_eq!(git(&repo.path, &["rev-parse", "HEAD"]), saves_head);
        assert!(
            git(&repo.path, &["status", "--porcelain"])
                .trim()
                .is_empty()
        );
        assert!(check_ref_in(&repo.path, PRE_LAND_REF).is_none());
    }

    #[test]
    fn an_interrupted_land_never_heals_or_undoes_in_another_detached_worktree() {
        let _lock = acquire_cwd_lock();
        let repo = init_repo();

        write_file(&repo.path, "tracked.txt", "saved in worktree A\n");
        git(&repo.path, &["add", "tracked.txt"]);
        git(&repo.path, &["commit", "-m", "[kite] save 12:00:00"]);
        let saves_head = git(&repo.path, &["rev-parse", "HEAD"]);

        let (_holder_a, worktree_a) = detached_worktree(&repo.path, "HEAD");
        let (_holder_b, worktree_b) = detached_worktree(&repo.path, "HEAD~1");
        let owner_a = with_repo_cwd(&worktree_a, current_worktree_key)
            .expect("worktree A identity should resolve");

        // A crashed after recording its detached target and before recording
        // the landed head. These refs/config entries are shared by A and B.
        git(
            &worktree_a,
            &["update-ref", PRE_LAND_REF, saves_head.trim()],
        );
        git(
            &worktree_a,
            &["config", "--local", PRE_LAND_BRANCH_KEY, DETACHED_TARGET],
        );
        git(
            &worktree_a,
            &["config", "--local", PRE_LAND_WORKTREE_KEY, &owner_a],
        );
        let _ = std::process::Command::new("git")
            .args(["config", "--local", "--unset", PRE_LAND_HEAD_KEY])
            .current_dir(&worktree_a)
            .output();

        let head_before = git(&worktree_b, &["rev-parse", "HEAD"]);
        let status_before = git(&worktree_b, &["status", "--porcelain"]);
        let file_before = std::fs::read_to_string(worktree_b.join("tracked.txt"))
            .expect("worktree B file should exist");

        // Every command calls healing first. A foreign marker must be a strict
        // no-op, and destructive commands must refuse it explicitly.
        with_repo_cwd(&worktree_b, heal_interrupted_land);
        let undo_error =
            undo_in_repo(&worktree_b).expect_err("worktree B must not consume worktree A's marker");
        assert!(format!("{undo_error:#}").contains("another worktree"));
        let land_error = with_repo_cwd(&worktree_b, || land_preflight(false))
            .expect_err("worktree B must not overwrite an in-progress marker");
        assert!(format!("{land_error:#}").contains("still in progress"));

        assert_eq!(git(&worktree_b, &["rev-parse", "HEAD"]), head_before);
        assert_eq!(git(&worktree_b, &["status", "--porcelain"]), status_before);
        assert_eq!(
            std::fs::read_to_string(worktree_b.join("tracked.txt")).unwrap(),
            file_before
        );
        assert_eq!(
            git(&worktree_b, &["rev-parse", PRE_LAND_REF]),
            saves_head,
            "a foreign command consumed the interrupted land marker"
        );
    }

    #[test]
    fn another_worktree_cannot_consume_an_interrupted_branch_land() {
        let _lock = acquire_cwd_lock();
        let repo = init_repo();

        write_file(&repo.path, "tracked.txt", "saved in worktree A\n");
        git(&repo.path, &["add", "tracked.txt"]);
        git(&repo.path, &["commit", "-m", "[kite] save 12:00:00"]);
        let saves_head = git(&repo.path, &["rev-parse", "HEAD"]);
        let branch = git(&repo.path, &["branch", "--show-current"]);
        let branch = branch.trim().to_string();
        let owner_a = with_repo_cwd(&repo.path, current_worktree_key)
            .expect("worktree A identity should resolve");
        let (_holder_b, worktree_b) = detached_worktree(&repo.path, "HEAD~1");

        git(
            &repo.path,
            &["config", "--local", PRE_LAND_BRANCH_KEY, &branch],
        );
        git(
            &repo.path,
            &["config", "--local", PRE_LAND_WORKTREE_KEY, &owner_a],
        );
        git(&repo.path, &["update-ref", PRE_LAND_REF, saves_head.trim()]);
        git(&repo.path, &["checkout", "-q", "--detach"]);
        git(&repo.path, &["reset", "-q", "--soft", "HEAD~1"]);
        git(&repo.path, &["reset", "-q"]);
        git(&repo.path, &["add", "tracked.txt"]);
        git(&repo.path, &["commit", "-qm", "feat: half a landing"]);

        // A detached during its rewrite, so B can now check out the target
        // branch. The matching branch name must not override A's ownership.
        git(&worktree_b, &["checkout", "-q", &branch]);
        let head_before = git(&worktree_b, &["rev-parse", "HEAD"]);
        let status_before = git(&worktree_b, &["status", "--porcelain"]);

        assert!(!with_repo_cwd(&worktree_b, heal_interrupted_land));
        let error = undo_in_repo(&worktree_b)
            .expect_err("worktree B must not consume worktree A's branch marker");
        assert!(format!("{error:#}").contains("another worktree"));
        assert_eq!(git(&worktree_b, &["rev-parse", "HEAD"]), head_before);
        assert_eq!(git(&worktree_b, &["status", "--porcelain"]), status_before);
        assert_eq!(git(&worktree_b, &["rev-parse", PRE_LAND_REF]), saves_head);
        assert!(config_get_in(&worktree_b, PRE_LAND_HEAD_KEY).is_none());
    }

    #[test]
    fn ownerless_interrupted_marker_never_mutates_an_unrelated_detached_worktree() {
        let _lock = acquire_cwd_lock();
        let repo = init_repo();
        write_file(&repo.path, "tracked.txt", "saved\n");
        git(&repo.path, &["add", "tracked.txt"]);
        git(&repo.path, &["commit", "-m", "[kite] save 12:00:00"]);
        let saves_head = git(&repo.path, &["rev-parse", "HEAD"]);
        let branch = git(&repo.path, &["branch", "--show-current"]);
        let (_holder, detached) = detached_worktree(&repo.path, "HEAD~1");

        git(
            &repo.path,
            &["config", "--local", PRE_LAND_BRANCH_KEY, branch.trim()],
        );
        git(&repo.path, &["update-ref", PRE_LAND_REF, saves_head.trim()]);

        let head_before = git(&detached, &["rev-parse", "HEAD"]);
        let status_before = git(&detached, &["status", "--porcelain"]);
        assert!(!with_repo_cwd(&detached, heal_interrupted_land));
        let error = undo_in_repo(&detached)
            .expect_err("an ownerless interrupted marker cannot be consumed safely");
        assert!(format!("{error:#}").contains("rollback marker is incomplete"));
        let land_error = with_repo_cwd(&detached, || land_preflight(false))
            .expect_err("an incomplete marker must block a replacement land");
        assert!(format!("{land_error:#}").contains("rollback marker is incomplete"));
        assert_eq!(git(&detached, &["rev-parse", "HEAD"]), head_before);
        assert_eq!(git(&detached, &["status", "--porcelain"]), status_before);
        assert_eq!(git(&detached, &["rev-parse", PRE_LAND_REF]), saves_head);
    }

    #[test]
    fn undo_refuses_unrelated_history_in_the_same_detached_worktree() {
        let _lock = acquire_cwd_lock();
        let repo = init_repo();
        let base = git(&repo.path, &["rev-parse", "HEAD"]);

        write_file(&repo.path, "tracked.txt", "saved change\n");
        git(&repo.path, &["add", "tracked.txt"]);
        git(&repo.path, &["commit", "-m", "[kite] save 12:00:00"]);
        git(&repo.path, &["checkout", "-q", "--detach"]);

        let scope = collect_land_scope_in_repo(&repo.path, false)
            .expect("scope should collect")
            .expect("save should be landable");
        execute_land_in_repo(
            &repo.path,
            &scope.base,
            &[files_commit("feat: landed", &["tracked.txt"])],
        )
        .expect("detached land should succeed");
        let landed_head = git(&repo.path, &["rev-parse", "HEAD"]);

        // The original base is older than (and not descended from) the landed
        // commit. It is the same worktree, but not the same line of history.
        git(&repo.path, &["checkout", "-q", "--detach", base.trim()]);
        let protected_head = git(&repo.path, &["rev-parse", "HEAD"]);
        let err =
            undo_in_repo(&repo.path).expect_err("unrelated detached history must never be reset");
        let rendered = format!("{err:#}");
        assert!(rendered.contains("no longer on the history"));
        assert!(rendered.contains(&format!("git switch --detach {}", landed_head.trim())));
        assert_eq!(git(&repo.path, &["rev-parse", "HEAD"]), protected_head);
        assert!(check_ref_in(&repo.path, PRE_LAND_REF).is_some());
    }

    #[test]
    fn land_preflight_refuses_an_active_merge() {
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
        assert!(format!("{err:#}").contains("merge in progress"));
    }

    #[test]
    fn detached_linked_worktree_refuses_every_active_git_operation_marker() {
        let _lock = acquire_cwd_lock();
        let repo = init_repo();
        let (_holder, worktree) = detached_worktree(&repo.path, "HEAD");
        let git_dir = with_repo_cwd(&worktree, current_worktree_key)
            .expect("linked-worktree git dir should resolve");
        let git_dir = std::path::PathBuf::from(git_dir);
        let cases = [
            ("rebase-merge", true, "rebase"),
            ("rebase-apply", true, "rebase or am"),
            ("CHERRY_PICK_HEAD", false, "cherry-pick"),
            ("REVERT_HEAD", false, "revert"),
            ("BISECT_START", false, "bisect"),
            ("sequencer", true, "sequenced cherry-pick or revert"),
        ];

        for (marker, directory, expected) in cases {
            let path = git_dir.join(marker);
            if directory {
                std::fs::create_dir(&path).expect("operation marker directory should be created");
            } else {
                std::fs::write(&path, "marker\n").expect("operation marker file should be created");
            }

            let error = with_repo_cwd(&worktree, || land_preflight(false))
                .expect_err("an active Git operation must block detached landing");
            assert!(
                format!("{error:#}").contains(expected),
                "{marker} produced the wrong error: {error:#}"
            );

            if directory {
                std::fs::remove_dir(&path).expect("operation marker directory should be removed");
            } else {
                std::fs::remove_file(&path).expect("operation marker file should be removed");
            }
        }
    }

    #[test]
    fn execute_land_rechecks_for_a_git_operation_before_rewriting() {
        let _lock = acquire_cwd_lock();
        let repo = init_repo();
        write_file(&repo.path, "tracked.txt", "saved change\n");
        git(&repo.path, &["add", "tracked.txt"]);
        git(&repo.path, &["commit", "-m", "[kite] save 12:00:00"]);
        let original_head = git(&repo.path, &["rev-parse", "HEAD"]);
        let scope = collect_land_scope_in_repo(&repo.path, false)
            .expect("scope should collect")
            .expect("save should be landable");
        let git_dir =
            with_repo_cwd(&repo.path, current_worktree_key).expect("git dir should resolve");
        std::fs::write(
            std::path::Path::new(&git_dir).join("BISECT_START"),
            "marker\n",
        )
        .expect("bisect marker should be created");

        let error = execute_land_in_repo(
            &repo.path,
            &scope.base,
            &[files_commit("feat: landed", &["tracked.txt"])],
        )
        .expect_err("the mutating core must repeat the Git-operation preflight");
        assert!(format!("{error:#}").contains("bisect in progress"));
        assert_eq!(git(&repo.path, &["rev-parse", "HEAD"]), original_head);
        assert!(check_ref_in(&repo.path, PRE_LAND_REF).is_none());
    }

    #[test]
    fn detached_undo_refuses_an_active_git_operation() {
        let _lock = acquire_cwd_lock();
        let repo = init_repo();
        write_file(&repo.path, "tracked.txt", "saved change\n");
        git(&repo.path, &["add", "tracked.txt"]);
        git(&repo.path, &["commit", "-m", "[kite] save 12:00:00"]);
        git(&repo.path, &["checkout", "-q", "--detach"]);
        let original_head = git(&repo.path, &["rev-parse", "HEAD"]);
        let git_dir =
            with_repo_cwd(&repo.path, current_worktree_key).expect("git dir should resolve");
        std::fs::write(
            std::path::Path::new(&git_dir).join("BISECT_START"),
            "marker\n",
        )
        .expect("bisect marker should be created");

        let error = undo_in_repo(&repo.path)
            .expect_err("undo must not rewrite Git's temporary detached checkout");
        assert!(format!("{error:#}").contains("before running `kt undo`"));
        assert_eq!(git(&repo.path, &["rev-parse", "HEAD"]), original_head);
    }

    fn check_ref_in(repo: &std::path::Path, name: &str) -> Option<String> {
        with_repo_cwd(repo, || check_ref(name))
    }

    fn config_get_in(repo: &std::path::Path, key: &str) -> Option<String> {
        with_repo_cwd(repo, || config_get(key))
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
                Hooks::Run,
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
    fn skipping_hooks_lands_history_a_pre_commit_hook_would_reject() {
        let _lock = acquire_cwd_lock();
        let repo = init_repo();
        write_file(&repo.path, "tracked.txt", "saved change\n");
        git(&repo.path, &["add", "tracked.txt"]);
        git(&repo.path, &["commit", "-m", "[kite] save 12:00:00"]);

        let scope = collect_land_scope_in_repo(&repo.path, false)
            .expect("scope")
            .expect("saves");
        let commits = [files_commit("feat: landed", &["tracked.txt"])];

        install_pre_commit_hook(&repo.path, "#!/bin/sh\necho 'pre-commit: nope'\nexit 1\n");

        // The negative control: without it the second land could pass for
        // reasons that have nothing to do with hooks.
        let error = execute_land_in_repo(&repo.path, &scope.base, &commits)
            .expect_err("a rejecting hook should block an ordinary land");
        assert!(format!("{error:#}").contains("Git hook blocked the commit"));

        // The failed land put the saves back, so the same plan can be retried
        // with hooks skipped.
        with_repo_cwd(&repo.path, || {
            execute_land(&scope.base, &commits, Hooks::Skip)
        })
        .expect("skipping hooks should land the same plan");

        let landed = git(&repo.path, &["log", "-1", "--pretty=%s"]);
        assert_eq!(landed.trim(), "feat: landed");
    }

    #[test]
    fn landing_removes_its_exact_temporary_branch() {
        let _lock = acquire_cwd_lock();
        let repo = init_repo();
        save_two_hunk_change(&repo.path);

        let scope = collect_land_scope_in_repo(&repo.path, false)
            .expect("scope")
            .expect("saves");

        // A hook is the only way to observe the repository mid-land.
        install_pre_commit_hook(
            &repo.path,
            "#!/bin/sh\ngit branch --format='%(refname:short)' >> .git/seen\n",
        );

        let groups = [group("feat: top", &["h1"]), group("feat: bottom", &["h2"])];
        let commits = with_repo_cwd(&repo.path, || {
            plan_commits(&scope.base, &scope.units, &groups)
        });
        execute_land_in_repo(&repo.path, &scope.base, &commits).expect("land should succeed");

        let seen = std::fs::read_to_string(repo.path.join(".git/seen")).unwrap_or_default();
        assert!(
            seen.contains("kite-landing-"),
            "hooks should observe an ordinary temporary branch: {seen}"
        );
        let leftovers = git(&repo.path, &["branch", "--list", "kite-landing-*"]);
        assert!(
            leftovers.trim().is_empty(),
            "landing left its temporary branch behind: {leftovers}"
        );
    }

    #[test]
    fn finalization_refuses_a_target_checked_out_in_another_worktree() {
        let _lock = acquire_cwd_lock();
        let repo = init_repo();
        let branch = git(&repo.path, &["branch", "--show-current"])
            .trim()
            .to_string();

        write_file(&repo.path, "tracked.txt", "saved change\n");
        git(&repo.path, &["add", "tracked.txt"]);
        git(&repo.path, &["commit", "-m", "[kite] save 12:00:00"]);
        let pre_land_sha = git(&repo.path, &["rev-parse", "HEAD"]);
        let base = KiteBase::Commit(git(&repo.path, &["rev-parse", "HEAD~1"]).trim().to_string());
        let (_holder, linked) = detached_worktree(&repo.path, "HEAD~1");
        let transaction = leave_interrupted_land(&repo.path, &base);

        // Once the owning worktree moves to its transaction branch, another
        // worktree can claim the target. Finalization must not move a ref that
        // is now live under that other checkout.
        git(&linked, &["checkout", "-q", &branch]);
        let error = with_repo_cwd(&repo.path, || {
            finalize_landed_head(
                &Head::Branch(branch.clone()),
                pre_land_sha.trim(),
                &transaction.landing.transaction_ref,
            )
        })
        .expect_err("a branch checked out elsewhere must not be finalized");
        assert!(format!("{error:#}").contains("another worktree"));
        assert_eq!(
            git(&repo.path, &["rev-parse", &branch]),
            pre_land_sha,
            "the checked-out branch was moved"
        );

        // Explicit recovery may no longer reattach the branch, but it can
        // safely return this worktree to the saved commit and clear the exact
        // transaction ref.
        undo_in_repo(&repo.path).expect("owned interrupted land should recover");
        assert!(with_repo_cwd(&repo.path, head_branch_hint).is_none());
        assert_eq!(git(&repo.path, &["rev-parse", "HEAD"]), pre_land_sha);
        assert!(check_ref_in(&repo.path, &transaction.landing.transaction_ref).is_none());
    }

    #[test]
    fn finalization_never_overwrites_a_target_that_advanced() {
        let _lock = acquire_cwd_lock();
        let repo = init_repo();
        let branch = git(&repo.path, &["branch", "--show-current"])
            .trim()
            .to_string();

        write_file(&repo.path, "tracked.txt", "saved change\n");
        git(&repo.path, &["add", "tracked.txt"]);
        git(&repo.path, &["commit", "-m", "[kite] save 12:00:00"]);
        let pre_land_sha = git(&repo.path, &["rev-parse", "HEAD"]);
        let tree = git(&repo.path, &["rev-parse", "HEAD^{tree}"]);
        let base = KiteBase::Commit(git(&repo.path, &["rev-parse", "HEAD~1"]).trim().to_string());
        let transaction = leave_interrupted_land(&repo.path, &base);

        let advanced = git(
            &repo.path,
            &[
                "commit-tree",
                tree.trim(),
                "-p",
                pre_land_sha.trim(),
                "-m",
                "feat: concurrent branch update",
            ],
        );
        git(
            &repo.path,
            &[
                "update-ref",
                &format!("refs/heads/{branch}"),
                advanced.trim(),
                pre_land_sha.trim(),
            ],
        );

        let error = with_repo_cwd(&repo.path, || {
            finalize_landed_head(
                &Head::Branch(branch.clone()),
                pre_land_sha.trim(),
                &transaction.landing.transaction_ref,
            )
        })
        .expect_err("a compare-and-swap mismatch must reject finalization");
        assert!(format!("{error:#}").contains("moved while Kite was landing"));
        assert_eq!(git(&repo.path, &["rev-parse", &branch]), advanced);

        let recovery = undo_in_repo(&repo.path)
            .expect_err("recovery must not overwrite the advanced branch either");
        assert!(format!("{recovery:#}").contains("newer value was left untouched"));
        assert_eq!(git(&repo.path, &["rev-parse", &branch]), advanced);
    }

    #[test]
    fn interrupted_detached_root_land_recovers_before_its_first_commit() {
        let _lock = acquire_cwd_lock();
        let repo = init_root_kite_repo();
        git(&repo.path, &["checkout", "-q", "--detach"]);
        let pre_land_sha = git(&repo.path, &["rev-parse", "HEAD"]);

        let transaction = with_repo_cwd(&repo.path, || {
            let previous = PreLandMarker::capture();
            let owner = current_worktree_key().expect("worktree should resolve");
            let transaction_ref = unique_kite_ref(TRANSACTION_REF_PREFIX);
            let transaction = install_in_progress_marker(
                &previous,
                pre_land_sha.trim(),
                DETACHED_TARGET,
                &owner,
                Some(&transaction_ref),
            )
            .expect("marker should install");
            prepare_landing_head(&KiteBase::Root, pre_land_sha.trim(), &transaction_ref)
                .expect("root rewrite should become unborn");
            transaction
        });
        assert!(check_ref_in(&repo.path, "HEAD").is_none());

        undo_in_repo(&repo.path).expect("unborn transaction should recover");
        assert!(with_repo_cwd(&repo.path, head_branch_hint).is_none());
        assert_eq!(git(&repo.path, &["rev-parse", "HEAD"]), pre_land_sha);
        assert!(check_ref_in(&repo.path, &transaction.landing.transaction_ref).is_none());
    }

    #[test]
    fn interrupted_completed_undo_finishes_before_peeling_a_save() {
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

        with_repo_cwd(&repo.path, || {
            let PreLandState::Completed(recorded) = pre_land_state() else {
                panic!("land should be completed");
            };
            let from_head = execute_git(&["rev-parse", "HEAD"]).unwrap();
            let transaction = begin_completed_undo(recorded, from_head.trim()).unwrap();
            restore_completed_land(&transaction).unwrap();
            // Simulate a process dying after the branch was restored but
            // before the Undoing marker was changed to Empty.
        });
        assert_eq!(
            git(&repo.path, &["log", "-1", "--pretty=%s"]).trim(),
            "[kite] save 12:00:00"
        );

        undo_in_repo(&repo.path).expect("the interrupted undo should finish first");
        assert_eq!(
            git(&repo.path, &["log", "-1", "--pretty=%s"]).trim(),
            "[kite] save 12:00:00",
            "recovery incorrectly peeled the restored save"
        );
        assert!(matches!(
            with_repo_cwd(&repo.path, pre_land_state),
            PreLandState::Empty { .. }
        ));
    }

    #[test]
    fn an_interrupted_branch_land_requires_explicit_undo() {
        let _lock = acquire_cwd_lock();
        let repo = init_repo();

        write_file(&repo.path, "tracked.txt", "saved change\n");
        git(&repo.path, &["add", "tracked.txt"]);
        git(&repo.path, &["commit", "-m", "[kite] save 12:00:00"]);
        let saves_head = git(&repo.path, &["rev-parse", "HEAD"]);
        let branch = git(&repo.path, &["rev-parse", "--abbrev-ref", "HEAD"]);
        let branch = branch.trim().to_string();

        let base = KiteBase::Commit(git(&repo.path, &["rev-parse", "HEAD~1"]).trim().to_string());
        leave_interrupted_land(&repo.path, &base);

        let partial_head = git(&repo.path, &["rev-parse", "HEAD"]);
        assert!(with_repo_cwd(&repo.path, heal_interrupted_land));
        assert_eq!(git(&repo.path, &["rev-parse", "HEAD"]), partial_head);

        undo_in_repo(&repo.path).expect("explicit undo should recover the interrupted land");

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
        assert!(check_ref_in(&repo.path, PRE_LAND_REF).is_none());
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
    fn marker_setup_failure_preserves_the_previous_completed_marker() {
        let _lock = acquire_cwd_lock();
        let repo = init_repo();

        write_file(&repo.path, "tracked.txt", "first save\n");
        git(&repo.path, &["add", "tracked.txt"]);
        git(&repo.path, &["commit", "-m", "[kite] save 12:00:00"]);
        let first_scope = collect_land_scope_in_repo(&repo.path, false)
            .expect("scope")
            .expect("saves");
        execute_land_in_repo(
            &repo.path,
            &first_scope.base,
            &[files_commit("feat: first", &["tracked.txt"])],
        )
        .expect("first land should succeed");
        let previous = with_repo_cwd(&repo.path, PreLandMarker::capture);

        write_file(&repo.path, "tracked.txt", "second save\n");
        git(&repo.path, &["add", "tracked.txt"]);
        git(&repo.path, &["commit", "-m", "[kite] save 13:00:00"]);
        let saves_head = git(&repo.path, &["rev-parse", "HEAD"]);
        // A colliding exact transaction ref makes one command in the ref
        // transaction fail. Git must reject the entire transaction rather
        // than exposing a mixture of the old and new marker fields.
        let collision_ref = unique_kite_ref(TRANSACTION_REF_PREFIX);
        git(
            &repo.path,
            &["update-ref", &collision_ref, saves_head.trim()],
        );
        let error = with_repo_cwd(&repo.path, || {
            install_in_progress_marker(
                &previous,
                saves_head.trim(),
                &head_position()?.land_key(),
                &current_worktree_key()?,
                Some(&collision_ref),
            )
        })
        .expect_err("a colliding transaction ref must reject marker installation");
        assert_eq!(
            git(&repo.path, &["rev-parse", &collision_ref]),
            saves_head,
            "the pre-existing branch collision was overwritten or deleted"
        );
        git(
            &repo.path,
            &["update-ref", "-d", &collision_ref, saves_head.trim()],
        );
        assert!(format!("{error:#}").contains("Could not install rollback state"));

        let after = with_repo_cwd(&repo.path, PreLandMarker::capture);
        assert_eq!(after.state_oid, previous.state_oid);
        assert_eq!(after.sha, previous.sha);
        assert_eq!(after.branch, previous.branch);
        assert_eq!(after.head, previous.head);
        assert_eq!(after.worktree, previous.worktree);
        assert_eq!(git(&repo.path, &["rev-parse", "HEAD"]), saves_head);
    }

    #[test]
    fn completion_marker_failure_undoes_the_rewrite_and_restores_the_previous_marker() {
        let _lock = acquire_cwd_lock();
        let repo = init_repo();

        write_file(&repo.path, "tracked.txt", "first save\n");
        git(&repo.path, &["add", "tracked.txt"]);
        git(&repo.path, &["commit", "-m", "[kite] save 12:00:00"]);
        let first_scope = collect_land_scope_in_repo(&repo.path, false)
            .expect("scope")
            .expect("saves");
        execute_land_in_repo(
            &repo.path,
            &first_scope.base,
            &[files_commit("feat: first", &["tracked.txt"])],
        )
        .expect("first land should succeed");
        let previous = with_repo_cwd(&repo.path, PreLandMarker::capture);

        write_file(&repo.path, "tracked.txt", "second save\n");
        git(&repo.path, &["add", "tracked.txt"]);
        git(&repo.path, &["commit", "-m", "[kite] save 13:00:00"]);
        let saves_head = git(&repo.path, &["rev-parse", "HEAD"]);
        let second_scope = collect_land_scope_in_repo(&repo.path, false)
            .expect("scope")
            .expect("saves");

        FAIL_LANDED_HEAD_WRITE.store(true, std::sync::atomic::Ordering::SeqCst);
        let error = execute_land_in_repo(
            &repo.path,
            &second_scope.base,
            &[files_commit("feat: second", &["tracked.txt"])],
        )
        .expect_err("a completion-marker failure must fail the land");
        assert!(format!("{error:#}").contains("The rewrite was undone"));
        assert_eq!(git(&repo.path, &["rev-parse", "HEAD"]), saves_head);
        assert_eq!(
            git(&repo.path, &["log", "-1", "--pretty=%s"]).trim(),
            "[kite] save 13:00:00"
        );

        let after = with_repo_cwd(&repo.path, PreLandMarker::capture);
        assert_eq!(after.sha, previous.sha);
        assert_eq!(after.branch, previous.branch);
        assert_eq!(after.head, previous.head);
        assert_eq!(after.worktree, previous.worktree);
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
