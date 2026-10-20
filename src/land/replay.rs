//! Build a land that follows history, window by window.
//!
//! Each window's commits are made in a private checkout that holds the
//! window's saved files, so commit hooks see exactly what is being committed.
//! The user's own checkout is attached to the land transaction like any land,
//! but its index and files stay as they were saved: they already hold the
//! final state, and other tools or people may be using them.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use super::commits::{Staging, changed_paths, create_commits};
use super::state::TRANSACTION_REF_PREFIX;
use super::windows::{WindowPlan, describe_range};
use crate::git::{
    Hooks, KiteBase, check_ref, current_worktree_key, empty_tree, execute_git, execute_git_with,
    short_sha,
};

/// Private checkouts live in a folder named after their land transaction, so
/// recovery can find one left behind by an interrupted land.
const CHECKOUT_PREFIX: &str = "kite-land-";

/// Builds every window's commits on `transaction_ref` and returns the files
/// commit hooks changed, which are then updated in the user's checkout.
pub(super) fn land_windows(
    base: &KiteBase,
    pre_land_sha: &str,
    transaction_ref: &str,
    windows: &[WindowPlan],
    hooks: Hooks,
) -> Result<HashSet<String>> {
    if check_ref(transaction_ref).as_deref() != Some(pre_land_sha) {
        anyhow::bail!("Kite's reserved land transaction ref moved before rewriting began");
    }
    execute_git(&["symbolic-ref", "HEAD", transaction_ref])?;

    let checkout = PrivateCheckout::add(pre_land_sha, transaction_ref)?;
    let built = checkout.run(|| build_windows(base, pre_land_sha, transaction_ref, windows, hooks));
    checkout.remove();
    let hook_changes = built?;

    // A two-way switch from the saved tip to the landed history touches only
    // the files hooks changed, and refuses to overwrite one edited meanwhile.
    // Refresh first so files whose timestamps changed are not mistaken for edits.
    let _ = execute_git(&["update-index", "-q", "--refresh"]);
    execute_git(&["read-tree", "-m", "-u", pre_land_sha, "HEAD"])
        .context("Could not update the files commit hooks changed")?;
    Ok(hook_changes)
}

/// Removes the private checkout an interrupted land left behind, if any.
/// Best effort, like removing it after a land: it holds nothing the saves
/// do not, and `git worktree prune` forgets it once its directory is gone.
pub(super) fn remove_abandoned_checkout(transaction_ref: &str) {
    let Some(name) = checkout_name(transaction_ref) else {
        return;
    };
    let Ok(listed) = execute_git(&["worktree", "list", "--porcelain"]) else {
        return;
    };
    for path in listed
        .lines()
        .filter_map(|line| line.strip_prefix("worktree "))
    {
        let path = Path::new(path);
        if let Some(folder) = path
            .parent()
            .filter(|folder| folder.file_name().is_some_and(|file| file == name.as_str()))
        {
            remove_checkout(path, folder);
        }
    }
}

fn checkout_name(transaction_ref: &str) -> Option<String> {
    transaction_ref
        .strip_prefix(TRANSACTION_REF_PREFIX)
        .map(|id| format!("{CHECKOUT_PREFIX}{id}"))
}

/// Runs inside the private checkout.
fn build_windows(
    base: &KiteBase,
    pre_land_sha: &str,
    transaction_ref: &str,
    windows: &[WindowPlan],
    hooks: Hooks,
) -> Result<HashSet<String>> {
    // Share the user's checkout's transaction ref, so every commit made here
    // is recorded where recovery already looks for it.
    execute_git(&["symbolic-ref", "HEAD", transaction_ref])?;
    match base {
        KiteBase::Commit(base_sha) => {
            execute_git(&[
                "-c",
                "submodule.recurse=false",
                "reset",
                "-q",
                "--hard",
                base_sha,
            ])?;
        }
        // An unborn transaction branch makes the first landed commit a root.
        KiteBase::Root => {
            execute_git(&["update-ref", "-d", transaction_ref, pre_land_sha])?;
        }
    }

    let mut hook_changes = HashSet::new();
    let mut previous_save = base.tree_ish()?;
    let last = windows.len().saturating_sub(1);
    for (position, window) in windows.iter().enumerate() {
        let changed = land_window(&previous_save, window, hooks).with_context(|| {
            let mut context = format!("Could not land saves {}", describe_range(&window.saves));
            if position < last {
                context.push_str(
                    ". Earlier saves land exactly as they were made; if a hook rejects work a later save fixed, land with --no-verify or a coarser --granularity",
                );
            }
            context
        })?;
        hook_changes.extend(changed);
        previous_save = window.save.clone();
    }
    Ok(hook_changes)
}

/// Lands one window: checks out its last save's files, carrying committed
/// hook edits to files the window leaves alone, then commits its groups
/// from the save's exact entries.
fn land_window(previous_save: &str, window: &WindowPlan, hooks: Hooks) -> Result<HashSet<String>> {
    let entries = saved_entries(previous_save, &window.save)?;
    let current = head_tree()?;
    let target = apply_entries(&current, &entries)?;
    // `--reset` also discards edits a hook left unstaged in an earlier
    // window. They were never committed, and this checkout is Kite's alone.
    // Submodules stay as they are: their pointers are staged from the save.
    execute_git(&[
        "-c",
        "submodule.recurse=false",
        "read-tree",
        "-u",
        "--reset",
        &target,
    ])?;
    // Unstage again: each group stages its own files.
    match check_ref("HEAD") {
        Some(_) => execute_git(&["-c", "submodule.recurse=false", "reset", "-q"])?,
        None => execute_git(&["read-tree", "--empty"])?,
    };

    let hook_changes = create_commits(&window.commits, hooks, &Staging::Saved(&entries))?;
    for path in changed_paths(&target, &head_tree()?)? {
        if !hook_changes.contains(&path) {
            anyhow::bail!(
                "The landed contents of {path:?} differ from save {} without a committed hook change.",
                short_sha(&window.save)
            );
        }
    }
    Ok(hook_changes)
}

fn head_tree() -> Result<String> {
    match check_ref("HEAD^{tree}") {
        Some(tree) => Ok(tree),
        None => empty_tree(),
    }
}

/// Each path that changed between saves `from` and `to`, with the
/// `--index-info` record that sets it to its entry in `to`.
fn saved_entries(from: &str, to: &str) -> Result<HashMap<String, String>> {
    let changes = execute_git(&[
        "diff-tree",
        "-r",
        "-z",
        "--no-renames",
        "--ignore-submodules=none",
        from,
        to,
    ])?;
    index_records(&changes)
}

/// `tree` with `entries` applied, written through a scratch index so the
/// checkout's own index is untouched.
fn apply_entries(tree: &str, entries: &HashMap<String, String>) -> Result<String> {
    if entries.is_empty() {
        return Ok(tree.to_string());
    }
    // Sorted, so a file replaced by a directory is removed before the
    // directory's files are added, as `diff-tree` lists them.
    let mut paths: Vec<&String> = entries.keys().collect();
    paths.sort();
    let records: String = paths
        .into_iter()
        .map(|path| entries[path].as_str())
        .collect();

    let index = PathBuf::from(current_worktree_key()?).join("kite-window-index");
    let index_path = index
        .to_str()
        .context("Git's directory path must be valid UTF-8")?;
    let env = [("GIT_INDEX_FILE", index_path)];
    let written = (|| -> Result<String> {
        execute_git_with(&["read-tree", tree], &env, None)?;
        execute_git_with(
            &["update-index", "-z", "--index-info"],
            &env,
            Some(&records),
        )?;
        Ok(execute_git_with(&["write-tree"], &env, None)?
            .trim()
            .to_string())
    })();
    let _ = std::fs::remove_file(&index);
    written
}

/// Turns `diff-tree -z` records (`:old new old-oid new-oid status`, then the
/// path) into `--index-info` records that set each path to its new entry. A
/// deleted path has mode 0, which removes it; a submodule keeps mode 160000
/// and its commit.
fn index_records(raw: &str) -> Result<HashMap<String, String>> {
    let mut fields = raw.split('\0');
    let mut records = HashMap::new();
    while let Some(header) = fields.next().filter(|header| !header.is_empty()) {
        let path = fields
            .next()
            .with_context(|| format!("diff-tree record {header:?} has no path"))?;
        let parts: Vec<&str> = header.trim_start_matches(':').split(' ').collect();
        let [_, mode, _, oid, _] = parts.as_slice() else {
            anyhow::bail!("Unexpected diff-tree record {header:?}");
        };
        records.insert(path.to_string(), format!("{mode} {oid}\t{path}\0"));
    }
    Ok(records)
}

/// A linked worktree outside the repository, inside an owner-only folder
/// named after the land.
struct PrivateCheckout {
    folder: PathBuf,
    path: PathBuf,
}

impl PrivateCheckout {
    fn add(pre_land_sha: &str, transaction_ref: &str) -> Result<Self> {
        let name = checkout_name(transaction_ref)
            .context("Kite generated an invalid land transaction ref")?;
        let folder = std::env::temp_dir().join(name);
        create_private_folder(&folder)?;
        let path = folder.join("checkout");
        let path_arg = path
            .to_str()
            .context("The temporary directory path must be valid UTF-8")?;
        // Detached and empty: it attaches to the transaction ref and fills
        // its files itself, and no checkout hook needs to run for that.
        let added = execute_git(&[
            "-c",
            "core.hooksPath=/dev/null",
            "worktree",
            "add",
            "--detach",
            "--no-checkout",
            path_arg,
            pre_land_sha,
        ]);
        if let Err(error) = added {
            remove_checkout(&path, &folder);
            return Err(error.context("Could not create a private checkout to land the saves in"));
        }
        Ok(Self { folder, path })
    }

    /// Runs `work` with Git commands aimed at this checkout.
    fn run<T>(&self, work: impl FnOnce() -> Result<T>) -> Result<T> {
        let original = std::env::current_dir().context("Failed to resolve current directory")?;
        std::env::set_current_dir(&self.path)
            .with_context(|| format!("Could not enter {}", self.path.display()))?;
        let result = work();
        std::env::set_current_dir(&original)
            .with_context(|| format!("Could not return to {}", original.display()))?;
        result
    }

    fn remove(self) {
        remove_checkout(&self.path, &self.folder);
    }
}

/// The checkout holds the repository's files, including saves that later
/// deleted a secret, and the system temp directory can be shared. So the
/// folder is owner-only before anything is written into it, whatever the
/// umask. Creating rather than reusing it also refuses a folder or link
/// someone else put there first.
fn create_private_folder(folder: &Path) -> Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        builder.mode(0o700);
        builder
            .create(folder)
            .with_context(|| format!("Could not create {}", folder.display()))?;
        // A umask can remove the owner's own bits too; set them exactly.
        std::fs::set_permissions(folder, std::fs::Permissions::from_mode(0o700))
            .with_context(|| format!("Could not protect {}", folder.display()))?;
    }
    #[cfg(not(unix))]
    builder
        .create(folder)
        .with_context(|| format!("Could not create {}", folder.display()))?;
    Ok(())
}

/// Best effort: a leftover checkout holds nothing the saves do not.
fn remove_checkout(path: &Path, folder: &Path) {
    let removed = path.to_str().is_some_and(|path| {
        execute_git(&["worktree", "remove", "--force", "--force", path]).is_ok()
    });
    let _ = std::fs::remove_dir_all(folder);
    if !removed {
        let _ = execute_git(&["worktree", "prune"]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diff_tree_records_become_index_records() {
        let zero = "0".repeat(40);
        let old = "1".repeat(40);
        let new = "2".repeat(40);
        let raw = format!(
            ":100644 100644 {old} {new} M\0src/a.rs\0:100644 000000 {old} {zero} D\0gone\nline.txt\0:000000 120000 {zero} {new} A\0link\0:160000 160000 {old} {new} M\0module\0"
        );

        let records = index_records(&raw).unwrap();
        assert_eq!(records.len(), 4);
        assert_eq!(records["src/a.rs"], format!("100644 {new}\tsrc/a.rs\0"));
        assert_eq!(
            records["gone\nline.txt"],
            format!("000000 {zero}\tgone\nline.txt\0")
        );
        assert_eq!(records["link"], format!("120000 {new}\tlink\0"));
        // A submodule pointer is staged as the commit it was saved at.
        assert_eq!(records["module"], format!("160000 {new}\tmodule\0"));
        assert!(index_records("").unwrap().is_empty());
        index_records(":100644 100644 x\0").expect_err("a malformed record must not be guessed at");
    }

    #[test]
    fn checkouts_are_named_after_their_transaction() {
        assert_eq!(
            checkout_name("refs/heads/kite-landing-20261020-1-2").as_deref(),
            Some("kite-land-20261020-1-2")
        );
        assert_eq!(checkout_name("refs/heads/main"), None);
    }
}
