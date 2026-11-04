//! Build planned commits, accepting hook edits only to each commit's files.

use std::collections::{HashMap, HashSet};

use anyhow::{Context, Result};

use crate::git::{
    Hooks, changed_paths, check_ref, commit_git, execute_git, execute_git_with, stage_paths,
};
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

/// The commit history is being built on, if any yet, and its tree. The index
/// holds that tree before every commit: the caller starts it that way, and
/// each commit proves it still does, so a tip is never asked for twice.
#[derive(Clone, Debug)]
pub(super) struct Tip {
    pub(super) commit: Option<String>,
    pub(super) tree: String,
}

impl Tip {
    /// `HEAD` and the tree in the index, which match before commits are built.
    pub(super) fn current() -> Result<Self> {
        Ok(Self {
            commit: check_ref("HEAD"),
            tree: execute_git(&["write-tree"])?.trim().to_string(),
        })
    }
}

/// Builds `commits` on `tip`, returning where history then ends and the files
/// commit hooks changed.
pub(super) fn create_commits(
    commits: &[CommitGroup],
    hooks: Hooks,
    staging: &Staging,
    mut tip: Tip,
) -> Result<(Tip, HashSet<String>)> {
    let mut hook_changes = HashSet::new();
    for commit in commits {
        // Large lands check thousands of paths against thousands of files.
        let planned: HashSet<&str> = commit.files.iter().map(String::as_str).collect();
        if !commit.files.is_empty() {
            staging.stage(&commit.files)?;
        }
        let staged_tree = execute_git(&["write-tree"])?.trim().to_string();
        // Even literal Git paths include descendants when a saved file was
        // replaced by a directory. Do not silently absorb a later group.
        for path in changed_paths(&tip.tree, &staged_tree)? {
            if !planned.contains(path.as_str()) {
                anyhow::bail!(
                    "Staging included {path:?} outside this commit's planned files. Keep a file-to-directory replacement in one commit."
                );
            }
        }
        // Formatting can cancel every change in this group. Let hooks finish,
        // then remove an empty result instead of asking for another save.
        commit_git(&commit.message, hooks)?;
        let committed = execute_git(&["rev-parse", "HEAD", "HEAD^{tree}"])?;
        let Some((head, committed_tree)) = committed.trim().split_once('\n') else {
            anyhow::bail!("Could not resolve the commit just created");
        };

        // A successful formatter may replace staged contents. It must not
        // pull other groups or unrelated files into this commit.
        if committed_tree != staged_tree {
            for path in changed_paths(&staged_tree, committed_tree)? {
                if !planned.contains(path.as_str()) {
                    anyhow::bail!(
                        "A commit hook changed {path:?}, which is outside this commit's planned files. Keep hook staging limited to the current commit's files."
                    );
                }
                hook_changes.insert(path);
            }
        }
        // Post-commit hooks run after Git writes the commit. Do not let their
        // staged leftovers slip into the next group as if Kite staged them.
        if hooks == Hooks::Run && execute_git(&["write-tree"])?.trim() != committed_tree {
            anyhow::bail!(
                "A commit hook left staged changes after committing. Those changes were not included; adjust the hook to stage them before the commit is written."
            );
        }
        match &tip.commit {
            Some(parent) if committed_tree == tip.tree => {
                execute_git(&["reset", "--soft", parent])?;
            }
            _ => {
                tip = Tip {
                    commit: Some(head.to_string()),
                    tree: committed_tree.to_string(),
                };
            }
        }
    }
    Ok((tip, hook_changes))
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
