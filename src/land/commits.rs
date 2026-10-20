//! Build planned commits, accepting hook edits only to each commit's files.

use std::collections::{HashMap, HashSet};

use anyhow::{Context, Result};

use crate::git::{Hooks, check_ref, commit_git, execute_git, execute_git_with, stage_paths};
use crate::synth::CommitGroup;

/// Where each commit's planned files are staged from.
pub(super) enum Staging<'a> {
    /// The working tree, which holds the saved files.
    Worktree,
    /// Exact saved entries, as `update-index --index-info` records keyed by
    /// path. Submodule pointers stage correctly even where the submodule is
    /// not checked out, and no file passes through a filter again.
    Saved(&'a HashMap<String, String>),
}

impl Staging<'_> {
    fn stage(&self, files: &[String]) -> Result<()> {
        match self {
            Staging::Worktree => stage_paths(files),
            Staging::Saved(entries) => {
                let mut records = String::new();
                for path in files {
                    let record = entries
                        .get(path)
                        .with_context(|| format!("{path:?} has no saved entry to stage"))?;
                    records.push_str(record);
                }
                execute_git_with(&["update-index", "-z", "--index-info"], &[], Some(&records))?;
                // Entries written this way carry no file timestamps. Refresh
                // them so hooks comparing the index with the files see no
                // phantom edits.
                let _ = execute_git(&["update-index", "-q", "--refresh"]);
                Ok(())
            }
        }
    }
}

pub(super) fn create_commits(
    commits: &[CommitGroup],
    hooks: Hooks,
    staging: &Staging,
) -> Result<HashSet<String>> {
    let mut hook_changes = HashSet::new();
    for commit in commits {
        let previous_tree = execute_git(&["write-tree"])?;
        if !commit.files.is_empty() {
            staging.stage(&commit.files)?;
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

pub(super) fn changed_paths(before: &str, after: &str) -> Result<Vec<String>> {
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
