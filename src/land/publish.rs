//! Publish a reviewed branch using an explicit lease when history diverges.

use super::state::{PreLandState, pre_land_state};
use crate::git::{
    branch_to_publish, check_ref, execute_git, has_remote, is_ancestor, subjects_missing_from_head,
};
use crate::ui::{Spinner, confirm, overflow_note, pluralize};
use anyhow::{Result, anyhow};
use colored::*;

const MAX_DROPPED_COMMITS_SHOWN: usize = 10;

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
        println!("{} No remote - history stays local", "·".dimmed());
        return Ok(());
    }

    let branch = branch_to_publish()?;
    let remote_ref = format!("refs/remotes/origin/{branch}");

    // No fetch on the common path. The lease below is checked by git against
    // the remote's real state at push time, so a tracking ref that has gone
    // stale causes a safe rejection, never a clobber. Only when there is no
    // tracking ref at all — nothing to lease against, which is precisely the
    // dangerous case — is it worth a round trip to find out what is there.
    let remote_sha = check_ref(&remote_ref).or_else(|| fetch_remote_tip(&branch));

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
        // remote URLs and ref arrows only bury them. Everything else - auth,
        // a missing remote, a hook - still needs git's own words.
        if remote_moved(&error) {
            anyhow!(
                "Push rejected - `origin/{branch}` has moved since you last fetched it. Run `git fetch origin {branch}` to see what changed, then rerun `kt publish`."
            )
        } else {
            error.context(format!("Could not publish `{branch}`"))
        }
    })?;
    println!("{} Published {}", "✓".green(), branch.bold());
    Ok(())
}

/// Fetches `branch`'s tip from origin when no tracking ref records it. A
/// single-branch or shallow clone has no fetch refspec for the branch, so the
/// fetch only writes `FETCH_HEAD`; reading it there keeps the user's refspecs
/// as they are. A branch origin does not have fails to fetch, which leaves
/// nothing to lease against.
fn fetch_remote_tip(branch: &str) -> Option<String> {
    let spinner = Spinner::start(format!("Checking origin/{branch}"));
    let fetched = execute_git(&["fetch", "origin", &format!("refs/heads/{branch}")]);
    spinner.stop();
    fetched.ok()?;
    check_ref(&format!("refs/remotes/origin/{branch}")).or_else(|| check_ref("FETCH_HEAD"))
}

/// Whether a push was refused because the remote ref no longer matches what
/// this clone last saw, as opposed to auth, network, or hook failures.
pub(super) fn remote_moved(push_error: &anyhow::Error) -> bool {
    let detail = format!("{push_error:#}");
    ["stale info", "fetch first", "non-fast-forward"]
        .iter()
        .any(|marker| detail.contains(marker))
}

/// The remote has commits this branch does not. When the last land on this
/// branch started from them, they are the saves it just rewrote, and
/// replacing them is the whole point. Saves pushed from another machine or by
/// someone else look the same but are not in that land, so anything else
/// needs an explicit yes.
fn confirm_discarding_remote_commits(branch: &str, remote_sha: &str) -> Result<()> {
    if matches!(pre_land_state(), PreLandState::Completed(land)
        if land.target == branch && is_ancestor(remote_sha, &land.pre_land_sha))
    {
        return Ok(());
    }
    let dropped = subjects_missing_from_head(remote_sha)?;

    println!(
        "{} `origin/{branch}` has {} that {} does not:",
        "!".yellow(),
        pluralize(dropped.len(), "commit"),
        "your branch".bold()
    );
    for subject in dropped.iter().take(MAX_DROPPED_COMMITS_SHOWN) {
        println!("     {} {}", "-".dimmed(), subject);
    }
    if let Some(note) = overflow_note(dropped.len(), MAX_DROPPED_COMMITS_SHOWN) {
        println!("     {}", note.dimmed());
    }

    if confirm("Publishing will discard them from the remote. Continue?")? {
        return Ok(());
    }

    anyhow::bail!(
        "Publish cancelled - `origin/{branch}` untouched. Run `git fetch origin {branch}` and reconcile if you want to keep those commits."
    )
}
