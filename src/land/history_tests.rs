//! Lands that follow history: each window's commits must end exactly on the
//! window's last save, hooks must see the files they check, and the user's
//! own checkout must be left alone.

use std::path::Path;

use super::tests::{collect_land_scope_in_repo, install_pre_commit_hook, undo_in_repo};
use super::*;
use crate::git::changes_between;
use crate::test_support::{
    acquire_cwd_lock, git, init_repo, init_root_kite_repo, with_repo_cwd, write_file,
};

fn save(repo: &Path, label: &str) -> String {
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "-qm", &format!("[kite] save {label}")]);
    git(repo, &["rev-parse", "HEAD"]).trim().to_string()
}

/// Saves that add, edit, delete, and move files, and replace a file with a
/// directory and back, so windows cut through every kind of change. Earlier
/// saves hold contents that later saves replace.
fn make_saves(repo: &Path, count: usize) -> Vec<String> {
    (0..count)
        .map(|index| {
            write_file(
                repo,
                &format!("src/module_{}.rs", index % 7),
                &format!("// revision {index}\n"),
            );
            if index % 5 == 0 {
                write_file(repo, &format!("docs/note_{index}.md"), "a note\n");
            }
            if index % 5 == 3 && index >= 5 {
                let _ = std::fs::remove_file(repo.join(format!("docs/note_{}.md", index - 8)));
            }
            match index {
                2 => write_file(repo, "entry", "a file\n"),
                9 => {
                    std::fs::remove_file(repo.join("entry")).unwrap();
                    write_file(repo, "entry/inner.txt", "now a directory\n");
                }
                17 => {
                    std::fs::remove_dir_all(repo.join("entry")).unwrap();
                    write_file(repo, "entry", "a file again\n");
                }
                21 => {
                    git(repo, &["mv", "src/module_0.rs", "src/renamed.rs"]);
                }
                _ => {}
            }
            save(repo, &format!("12:{:02}:00", index))
        })
        .collect()
}

/// A plan for the given windows of `saves`, splitting each window's files
/// into a docs commit and a code commit when it has both.
fn plan_windows(
    repo: &Path,
    base: &KiteBase,
    saves: &[String],
    ranges: &[std::ops::Range<usize>],
) -> Vec<WindowPlan> {
    with_repo_cwd(repo, || {
        let mut from = base.tree_ish().unwrap();
        ranges
            .iter()
            .map(|range| {
                let save = saves[range.end - 1].clone();
                let (paths, _) = changes_between(&from, &save).unwrap();
                from = save.clone();
                let (docs, code): (Vec<String>, Vec<String>) = paths
                    .into_iter()
                    .partition(|path| path.starts_with("docs/"));
                let commits = [("docs", docs), ("feat", code)]
                    .into_iter()
                    .filter(|(_, files)| !files.is_empty())
                    .map(|(kind, files)| CommitGroup {
                        message: format!("{kind}: saves {}", windows::describe_range(range)),
                        files,
                    })
                    .collect();
                WindowPlan {
                    saves: range.clone(),
                    save,
                    commits,
                }
            })
            .collect()
    })
}

fn land_windows_in_repo(repo: &Path, base: &KiteBase, windows: &[WindowPlan]) -> Result<()> {
    with_repo_cwd(repo, || execute_windowed_land(base, windows, Hooks::Run))
}

fn tree(repo: &Path, revision: &str) -> String {
    git(repo, &["rev-parse", &format!("{revision}^{{tree}}")])
        .trim()
        .to_string()
}

/// Every landed commit, oldest first, as (subject, tree). Without a base,
/// the whole history landed.
fn landed_commits(repo: &Path, base: Option<&str>) -> Vec<(String, String)> {
    let range = base.map_or("HEAD".to_string(), |base| format!("{base}..HEAD"));
    git(repo, &["log", "--reverse", "--format=%s %T", &range])
        .lines()
        .map(|line| {
            let (subject, tree) = line.rsplit_once(' ').unwrap();
            (subject.to_string(), tree.to_string())
        })
        .collect()
}

/// The planned commits landed in order, and each window's last commit holds
/// exactly the tree of the save the window ends on.
fn assert_windows_end_on_their_saves(repo: &Path, base: Option<&str>, windows: &[WindowPlan]) {
    let landed = landed_commits(repo, base);
    let planned: Vec<&CommitGroup> = windows.iter().flat_map(|window| &window.commits).collect();
    let subjects: Vec<&str> = landed.iter().map(|(subject, _)| subject.as_str()).collect();
    let messages: Vec<&str> = planned
        .iter()
        .map(|commit| commit.message.as_str())
        .collect();
    assert_eq!(subjects, messages);
    let mut position = 0;
    for window in windows {
        position += window.commits.len();
        let (subject, landed_tree) = &landed[position - 1];
        assert_eq!(landed_tree, &tree(repo, &window.save), "{subject}");
    }
}

fn assert_no_private_checkouts(repo: &Path) {
    let worktrees = git(repo, &["worktree", "list", "--porcelain"]);
    assert_eq!(
        worktrees.matches("worktree ").count(),
        1,
        "a private checkout was left behind: {worktrees}"
    );
    assert_eq!(git(repo, &["branch", "--list", "kite-landing-*"]), "");
}

#[test]
fn a_long_history_lands_window_by_window_on_each_saved_tree() {
    let _lock = acquire_cwd_lock();
    let repo = init_repo();
    let base_sha = git(&repo.path, &["rev-parse", "HEAD"]).trim().to_string();
    let saves = make_saves(&repo.path, 40);
    let tip = saves.last().unwrap().clone();
    let branch = git(&repo.path, &["symbolic-ref", "HEAD"]);

    let scope = collect_land_scope_in_repo(&repo.path, false)
        .unwrap()
        .unwrap();
    assert_eq!(scope.save_count, 40);
    let ranges = [0..1, 1..9, 9..10, 10..22, 22..23, 23..40];
    let windows = plan_windows(&repo.path, &scope.base, &saves, &ranges);
    land_windows_in_repo(&repo.path, &scope.base, &windows).expect("history should land");

    // The final tree is exactly the saved tip, and the user is back on it.
    assert_eq!(tree(&repo.path, "HEAD"), tree(&repo.path, &tip));
    assert_eq!(git(&repo.path, &["symbolic-ref", "HEAD"]), branch);
    assert_eq!(git(&repo.path, &["status", "--porcelain"]), "");
    assert_no_private_checkouts(&repo.path);

    assert_windows_end_on_their_saves(&repo.path, Some(&base_sha), &windows);
    assert!(windows.iter().any(|window| window.commits.len() == 2));

    // And the land is one undoable step.
    undo_in_repo(&repo.path).expect("the land should undo");
    assert_eq!(git(&repo.path, &["rev-parse", "HEAD"]).trim(), tip);
}

#[test]
fn a_history_that_starts_at_the_root_lands_a_root_commit_first() {
    let _lock = acquire_cwd_lock();
    let repo = init_root_kite_repo();
    let first = git(&repo.path, &["rev-parse", "HEAD"]).trim().to_string();
    let mut saves = vec![first];
    saves.extend(make_saves(&repo.path, 6));

    let scope = collect_land_scope_in_repo(&repo.path, false)
        .unwrap()
        .unwrap();
    assert_eq!(scope.base, KiteBase::Root);
    let windows = plan_windows(&repo.path, &scope.base, &saves, &[0..2, 2..7]);
    land_windows_in_repo(&repo.path, &scope.base, &windows).expect("history should land");

    assert_windows_end_on_their_saves(&repo.path, None, &windows);
    assert_eq!(
        git(&repo.path, &["rev-list", "--max-parents=0", "HEAD"])
            .lines()
            .count(),
        1
    );
    assert_eq!(tree(&repo.path, "HEAD"), tree(&repo.path, &saves[6]));
    assert_eq!(git(&repo.path, &["status", "--porcelain"]), "");
    assert_no_private_checkouts(&repo.path);
}

/// Native-style hook: it formats staged files in place, so it refuses staged
/// files whose working copy differs. A land that built old saves in the
/// user's checkout would trip it on every commit.
#[cfg(unix)]
#[test]
fn hooks_see_exactly_the_files_each_commit_lands() {
    let _lock = acquire_cwd_lock();
    let repo = init_repo();
    let saves = make_saves(&repo.path, 12);
    install_pre_commit_hook(
        &repo.path,
        "#!/bin/sh\nstaged=$(git diff --cached --name-only --diff-filter=ACMR)\n[ -n \"$staged\" ] || exit 0\nif [ -n \"$(git diff --name-only -- $staged)\" ]; then echo 'staged files have unstaged changes' >&2; exit 1; fi\n",
    );

    let scope = collect_land_scope_in_repo(&repo.path, false)
        .unwrap()
        .unwrap();
    let windows = plan_windows(&repo.path, &scope.base, &saves, &[0..4, 4..8, 8..12]);
    land_windows_in_repo(&repo.path, &scope.base, &windows).expect("hooks should pass");

    assert_eq!(tree(&repo.path, "HEAD"), tree(&repo.path, &saves[11]));
}

#[cfg(unix)]
#[test]
fn the_users_files_are_never_rewritten() {
    use std::os::unix::fs::MetadataExt;
    let identity = |path: &str| {
        let metadata = std::fs::metadata(path).unwrap();
        (metadata.ino(), metadata.modified().unwrap())
    };

    let _lock = acquire_cwd_lock();
    let repo = init_repo();
    // An early save tracks a file that is later deleted and ignored. The
    // user's own copy must survive the land replaying that early save.
    write_file(&repo.path, "settings.env", "saved by mistake\n");
    let mut saves = vec![save(&repo.path, "12:00:00")];
    std::fs::remove_file(repo.path.join("settings.env")).unwrap();
    write_file(&repo.path, ".gitignore", "settings.env\n");
    saves.extend(make_saves(&repo.path, 8));
    write_file(&repo.path, "settings.env", "the user's real settings\n");
    let tracked = repo.path.join("tracked.txt").display().to_string();
    let edited = repo.path.join("src/module_3.rs").display().to_string();
    let before = [identity(&tracked), identity(&edited)];

    let scope = collect_land_scope_in_repo(&repo.path, false)
        .unwrap()
        .unwrap();
    let windows = plan_windows(&repo.path, &scope.base, &saves, &[0..1, 1..5, 5..9]);
    land_windows_in_repo(&repo.path, &scope.base, &windows).expect("history should land");

    assert_eq!(
        std::fs::read_to_string(repo.path.join("settings.env")).unwrap(),
        "the user's real settings\n"
    );
    // Not even rewritten with the same contents.
    assert_eq!([identity(&tracked), identity(&edited)], before);
    assert_eq!(git(&repo.path, &["status", "--porcelain"]), "");
}

/// A formatter's edits land in the window it ran in, carry into later
/// windows that leave the file alone, and end up in the user's files.
#[cfg(unix)]
#[test]
fn hook_formatting_carries_forward_and_reaches_the_users_files() {
    let _lock = acquire_cwd_lock();
    let repo = init_repo();
    let base_sha = git(&repo.path, &["rev-parse", "HEAD"]).trim().to_string();
    write_file(&repo.path, "style.txt", "unformatted\n");
    let mut saves = vec![save(&repo.path, "12:00:00")];
    saves.extend(make_saves(&repo.path, 6));
    install_pre_commit_hook(
        &repo.path,
        "#!/bin/sh\nif git diff --cached --name-only | grep -qx style.txt; then printf 'formatted\\n' > style.txt; git add style.txt; fi\n",
    );

    let scope = collect_land_scope_in_repo(&repo.path, false)
        .unwrap()
        .unwrap();
    let windows = plan_windows(&repo.path, &scope.base, &saves, &[0..1, 1..4, 4..7]);
    land_windows_in_repo(&repo.path, &scope.base, &windows).expect("history should land");

    let first = git(
        &repo.path,
        &["rev-list", "--reverse", &format!("{base_sha}..HEAD")],
    );
    let first = first.lines().next().unwrap();
    assert_eq!(
        git(&repo.path, &["show", &format!("{first}:style.txt")]),
        "formatted\n"
    );
    assert_eq!(git(&repo.path, &["show", "HEAD:style.txt"]), "formatted\n");
    assert_eq!(
        std::fs::read_to_string(repo.path.join("style.txt")).unwrap(),
        "formatted\n"
    );
    // Everything else is exactly as saved.
    assert_eq!(
        git(&repo.path, &["diff", "--name-only", &saves[6], "HEAD"]).trim(),
        "style.txt"
    );
    assert_eq!(git(&repo.path, &["status", "--porcelain"]), "");
}

/// A formatter that edits the files it checks without staging the result
/// leaves the private checkout dirty. A later window changing those files
/// must not trip over the edits.
#[cfg(unix)]
#[test]
fn unstaged_hook_edits_do_not_block_later_windows() {
    let _lock = acquire_cwd_lock();
    let repo = init_repo();
    let base_sha = git(&repo.path, &["rev-parse", "HEAD"]).trim().to_string();
    let saves = make_saves(&repo.path, 12);
    install_pre_commit_hook(
        &repo.path,
        "#!/bin/sh\nif git diff --cached --name-only | grep -qx src/module_1.rs; then printf 'scribbled\\n' >> src/module_1.rs; fi\n",
    );

    let scope = collect_land_scope_in_repo(&repo.path, false)
        .unwrap()
        .unwrap();
    let windows = plan_windows(&repo.path, &scope.base, &saves, &[0..4, 4..8, 8..12]);
    land_windows_in_repo(&repo.path, &scope.base, &windows).expect("history should land");

    assert_windows_end_on_their_saves(&repo.path, Some(&base_sha), &windows);
    assert_eq!(git(&repo.path, &["status", "--porcelain"]), "");
}

#[cfg(unix)]
#[test]
fn a_hook_rejecting_a_middle_window_puts_everything_back() {
    let _lock = acquire_cwd_lock();
    let repo = init_repo();
    let saves = make_saves(&repo.path, 12);
    let tip = saves.last().unwrap().clone();
    let branch = git(&repo.path, &["symbolic-ref", "HEAD"]);
    install_pre_commit_hook(
        &repo.path,
        "#!/bin/sh\nif grep -q 'revision 5' src/module_5.rs 2>/dev/null; then echo 'lint failed on revision 5' >&2; exit 1; fi\n",
    );

    let scope = collect_land_scope_in_repo(&repo.path, false)
        .unwrap()
        .unwrap();
    let windows = plan_windows(&repo.path, &scope.base, &saves, &[0..4, 4..8, 8..12]);
    let error = land_windows_in_repo(&repo.path, &scope.base, &windows)
        .expect_err("the hook rejects the second window");

    let rendered = format!("{error:#}");
    assert!(rendered.contains("lint failed on revision 5"), "{rendered}");
    assert!(rendered.contains("Could not land saves 5-8"), "{rendered}");
    assert!(rendered.contains("--no-verify"), "{rendered}");
    assert!(rendered.contains("every save intact"), "{rendered}");
    assert_eq!(git(&repo.path, &["rev-parse", "HEAD"]).trim(), tip);
    assert_eq!(git(&repo.path, &["symbolic-ref", "HEAD"]), branch);
    assert_eq!(git(&repo.path, &["status", "--porcelain"]), "");
    assert_no_private_checkouts(&repo.path);
    assert!(with_repo_cwd(&repo.path, recovery_blocks_commands).is_ok_and(|blocked| !blocked));
}

#[test]
fn a_land_that_stopped_with_its_private_checkout_is_cleaned_up_by_undo() {
    let _lock = acquire_cwd_lock();
    let repo = init_repo();
    let saves = make_saves(&repo.path, 3);
    let tip = saves.last().unwrap().clone();
    let scope = collect_land_scope_in_repo(&repo.path, false)
        .unwrap()
        .unwrap();

    // Install the transaction and its checkout as a crash would leave them.
    let folder = with_repo_cwd(&repo.path, || {
        let transaction_ref = unique_kite_ref(TRANSACTION_REF_PREFIX);
        install_in_progress_marker(
            &PreLandMarker::capture(),
            &tip,
            &head_position().unwrap().land_key(),
            &current_worktree_key().unwrap(),
            &transaction_ref,
        )
        .unwrap();
        let folder = std::env::temp_dir()
            .join(transaction_ref.replace(TRANSACTION_REF_PREFIX, "kite-land-"));
        let checkout = folder.join("checkout");
        execute_git(&[
            "worktree",
            "add",
            "--detach",
            checkout.to_str().unwrap(),
            &tip,
        ])
        .unwrap();
        execute_git(&["symbolic-ref", "HEAD", &transaction_ref]).unwrap();
        if let KiteBase::Commit(base) = &scope.base {
            execute_git(&["update-ref", &transaction_ref, base]).unwrap();
        }
        folder
    });
    assert!(with_repo_cwd(&repo.path, recovery_blocks_commands).unwrap());

    undo_in_repo(&repo.path).expect("undo should recover the interrupted land");

    assert_eq!(git(&repo.path, &["rev-parse", "HEAD"]).trim(), tip);
    assert_eq!(git(&repo.path, &["status", "--porcelain"]), "");
    assert_no_private_checkouts(&repo.path);
    assert!(!folder.exists(), "{}", folder.display());
}
