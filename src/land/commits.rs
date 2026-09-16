//! Build planned commits, accepting hook edits only to each commit's files.

use std::collections::HashSet;

use anyhow::Result;

use crate::git::{Hooks, check_ref, commit_git, execute_git, stage_paths};
use crate::synth::CommitGroup;

pub(super) fn create_commits(commits: &[CommitGroup], hooks: Hooks) -> Result<HashSet<String>> {
    let mut hook_changes = HashSet::new();
    for commit in commits {
        let previous_tree = execute_git(&["write-tree"])?;
        if !commit.files.is_empty() {
            stage_paths(&commit.files)?;
        }
        let staged_tree = execute_git(&["write-tree"])?;
        // Even literal Git paths include descendants when a saved file was
        // replaced by a directory. Do not silently absorb a later group.
        for path in changed_paths(previous_tree.trim(), staged_tree.trim())? {
            if !commit.files.contains(&path) {
                anyhow::bail!(
                    "Staging included {path:?} outside this commit's planned files. Keep a file-to-directory replacement in one commit."
                );
            }
        }
        let parent = check_ref("HEAD");
        // Formatting can cancel every change in this group. Let hooks finish,
        // then remove an empty result instead of asking for another save.
        commit_git(&commit.message, hooks, true)?;

        // A successful formatter may replace staged contents. It must not
        // pull other groups or unrelated files into this commit.
        for path in changed_paths(staged_tree.trim(), "HEAD")? {
            if !commit.files.contains(&path) {
                anyhow::bail!(
                    "A commit hook changed {path:?}, which is outside this commit's planned files. Keep hook staging limited to the current commit's files."
                );
            }
            hook_changes.insert(path);
        }
        // Post-commit hooks run after Git writes the commit. Do not let their
        // staged leftovers slip into the next group as if Kite staged them.
        if execute_git(&["write-tree"])? != execute_git(&["rev-parse", "HEAD^{tree}"])? {
            anyhow::bail!(
                "A commit hook left staged changes after committing. Those changes were not included; adjust the hook to stage them before the commit is written."
            );
        }
        if let Some(parent) = parent
            && changed_paths(&parent, "HEAD")?.is_empty()
        {
            execute_git(&["reset", "--soft", &parent])?;
        }
    }
    Ok(hook_changes)
}

pub(super) fn verify_saved_tree(saved: &str, hook_changes: &HashSet<String>) -> Result<()> {
    for path in changed_paths(saved, "HEAD")? {
        if !hook_changes.contains(&path) {
            anyhow::bail!(
                "The landed contents of {path:?} differ from the saves without a committed hook change. Save those changes before retrying."
            );
        }
    }
    Ok(())
}

fn changed_paths(before: &str, after: &str) -> Result<Vec<String>> {
    Ok(execute_git(&[
        "diff",
        "--no-ext-diff",
        "--no-textconv",
        "--ignore-submodules=none",
        "--no-renames",
        "--name-only",
        "-z",
        before,
        after,
        "--",
    ])?
    .split('\0')
    .filter(|path| !path.is_empty())
    .map(str::to_string)
    .collect())
}
