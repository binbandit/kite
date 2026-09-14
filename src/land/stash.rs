//! Keep temporary work recoverable even if Kite stops before rewriting history.

use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};
use colored::*;
use serde::{Deserialize, Serialize};

use super::state::unique_kite_ref;
use crate::git::{current_worktree_key, execute_git, head_position};

#[derive(Serialize, Deserialize)]
struct PendingWork {
    message: String,
    target: String,
    tree: String,
}

fn marker_path() -> Result<PathBuf> {
    Ok(PathBuf::from(current_worktree_key()?).join("kite-pending-work.json"))
}

pub(super) fn is_pending() -> Result<bool> {
    Ok(marker_path()?.try_exists()?)
}

pub(super) fn save() -> Result<bool> {
    if execute_git(&["status", "--porcelain", "--ignore-submodules=none"])?
        .trim()
        .is_empty()
    {
        return Ok(false);
    }
    let path = marker_path()?;
    if path.try_exists()? {
        anyhow::bail!("An earlier land has pending work. Run `kt undo` to restore it first.");
    }
    let pending = PendingWork {
        message: unique_kite_ref("kt land temporary work "),
        target: head_position()?.land_key(),
        tree: execute_git(&["rev-parse", "HEAD^{tree}"])?
            .trim()
            .to_string(),
    };
    // Record intent before stashing. Its unique message identifies the exact
    // stash even if Kite stops before Git returns or another worktree stashes.
    let temporary = path.with_extension("tmp");
    fs::write(&temporary, serde_json::to_vec(&pending)?)?;
    fs::rename(&temporary, &path)?;
    let result = execute_git(&[
        "stash",
        "push",
        "--include-untracked",
        "-m",
        &pending.message,
    ]);
    let stash = find_stash(&pending)?;
    if stash.is_none() {
        fs::remove_file(path)?;
    }
    result?;
    if stash.is_none() {
        anyhow::bail!(
            "Git could not stash the remaining changes. Save or stash changes inside submodules before landing."
        );
    }
    Ok(true)
}

fn find_stash(pending: &PendingWork) -> Result<Option<String>> {
    let suffix = format!(": {}", pending.message);
    let stashes = execute_git(&["stash", "list", "--format=%H%x00%gs"])?;
    Ok(stashes.lines().find_map(|line| {
        let (oid, subject) = line.split_once('\0')?;
        subject.ends_with(&suffix).then(|| oid.to_string())
    }))
}

pub(super) fn restore() -> Result<bool> {
    let path = marker_path()?;
    if !path.try_exists()? {
        return Ok(false);
    }
    let pending: PendingWork = serde_json::from_slice(&fs::read(&path)?)
        .with_context(|| format!("Could not read pending work at {}", path.display()))?;
    let Some(stash) = find_stash(&pending)? else {
        // Stashing never completed, or the user already restored its entry.
        fs::remove_file(path)?;
        return Ok(true);
    };
    if head_position()?.land_key() != pending.target
        || execute_git(&["rev-parse", "HEAD^{tree}"])?.trim() != pending.tree
    {
        anyhow::bail!(
            "Your temporary work remains in stash {stash}. Return to the checkout where landing started, then run `kt undo` to restore it."
        );
    }
    execute_git(&["stash", "apply", "--index", &stash]).with_context(|| {
        format!("Your changes remain in stash {stash}. Resolve the worktree conflict before restoring it with `git stash apply --index {stash}`. After restoring it manually, remove {}", path.display())
    })?;
    // Stash positions are shared with linked worktrees and can change at any
    // moment. Keep this backup rather than risk dropping somebody else's entry.
    fs::remove_file(path)?;
    println!(
        "{} Restored your uncommitted work and staging. Backup retained in `git stash`: {stash}",
        "✓".green()
    );
    Ok(true)
}
