use super::recovery::*;
use super::*;
use crate::git::{DETACHED_TARGET, config_get, head_symbolic_ref, short_sha};
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
    commits: &[CommitGroup],
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
            &transaction_ref,
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

fn files_commit(message: &str, files: &[&str]) -> CommitGroup {
    CommitGroup {
        message: message.to_string(),
        files: files.iter().map(|file| file.to_string()).collect(),
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

/// One save touching two files, so a plan can spread them over two commits.
fn save_two_file_change(repo: &std::path::Path) {
    write_file(repo, "code.txt", "alpha\n");
    write_file(repo, "docs.txt", "notes\n");
    git(repo, &["add", "code.txt", "docs.txt"]);
    git(repo, &["commit", "-m", "chore: add code"]);

    write_file(repo, "code.txt", "ALPHA\n");
    write_file(repo, "docs.txt", "NOTES\n");
    git(repo, &["add", "code.txt", "docs.txt"]);
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
fn render_land_plan_numbers_commits_and_lists_their_files() {
    // Pin colors off so we assert the structural layout, not ANSI codes.
    colored::control::set_override(false);

    let plan = render_land_plan(
        &[
            files_commit("feat(api): add webhooks", &["src/api.rs", "src/hooks.rs"]),
            files_commit("docs: refresh readme", &["README.md"]),
        ],
        3,
    );

    assert!(plan.contains("Plan: 3 saves → 2 commits"));
    assert!(plan.contains("  1. feat(api): add webhooks\n"));
    assert!(plan.contains("     ├─ src/api.rs\n"));
    assert!(plan.contains("     └─ src/hooks.rs\n"));
    assert!(plan.contains("  2. docs: refresh readme\n"));
    assert!(plan.contains("     └─ README.md\n"));
}

/// The state a repository landed by a pre-atomic Kite is left in: a
/// pointer with nothing recorded beside it. It used to classify as a
/// broken marker, which blocked every command in the repository —
/// including the `kt undo` the error told you to run.
#[test]
fn a_bare_pre_land_pointer_does_not_block_commands() {
    let _lock = acquire_cwd_lock();
    let repo = init_repo();
    let head = git(&repo.path, &["rev-parse", "HEAD"]);

    git(&repo.path, &["update-ref", PRE_LAND_REF, head.trim()]);

    let (state, blocks) = with_repo_cwd(&repo.path, || {
        (pre_land_state(), recovery_blocks_commands())
    });

    assert!(matches!(state, PreLandState::Empty { .. }), "{state:?}");
    assert!(!blocks.expect("a bare pointer must not fail the recovery check"));
}

#[test]
fn landing_replaces_a_bare_pre_land_pointer() {
    let _lock = acquire_cwd_lock();
    let repo = init_repo();
    let stale = git(&repo.path, &["rev-parse", "HEAD"]).trim().to_string();
    git(&repo.path, &["update-ref", PRE_LAND_REF, &stale]);

    save_two_file_change(&repo.path);
    let pre_land_sha = git(&repo.path, &["rev-parse", "HEAD"]).trim().to_string();

    let scope = collect_land_scope_in_repo(&repo.path, false)
        .expect("land scope should collect")
        .expect("kite saves should be landable");
    execute_land_in_repo(
        &repo.path,
        &scope.base,
        &[files_commit(
            "feat: land over the stale pointer",
            &["code.txt", "docs.txt"],
        )],
    )
    .expect("a stale pointer must not stop a land");

    // The pointer now describes this land, and `kt undo` can use it.
    assert_eq!(
        check_ref_in(&repo.path, PRE_LAND_REF).as_deref(),
        Some(pre_land_sha.as_str())
    );
    undo_in_repo(&repo.path).expect("the landed history should undo");
    assert_eq!(git(&repo.path, &["rev-parse", "HEAD"]).trim(), pre_land_sha);
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
    assert!(scope.files.paths().contains(&"tracked.txt".to_string()));
}

#[test]
fn land_groups_whole_files_into_separate_commits_and_preserves_the_tree() {
    let _lock = acquire_cwd_lock();
    let repo = init_repo();
    save_two_file_change(&repo.path);

    let pre_land_tree = git(&repo.path, &["rev-parse", "HEAD^{tree}"]);

    let scope = collect_land_scope_in_repo(&repo.path, false)
        .expect("land scope should collect")
        .expect("kite saves should be landable");
    assert_eq!(scope.files.paths(), vec!["code.txt", "docs.txt"]);

    // Deliberately out of diff order: the second file lands first.
    let commits = [
        files_commit("docs: update notes", &["docs.txt"]),
        files_commit("feat: upcase code", &["code.txt"]),
    ];
    execute_land_in_repo(&repo.path, &scope.base, &commits).expect("land should succeed");

    let messages = git(&repo.path, &["log", "--pretty=%s", "-n", "2"]);
    assert_eq!(
        messages.lines().collect::<Vec<_>>(),
        vec!["feat: upcase code", "docs: update notes"]
    );

    // The intermediate commit carries docs.txt whole and leaves code.txt
    // exactly as the base had it: no half-applied file for a hook to trip
    // over.
    assert_eq!(git(&repo.path, &["show", "HEAD^:docs.txt"]), "NOTES\n");
    assert_eq!(git(&repo.path, &["show", "HEAD^:code.txt"]), "alpha\n");

    // The landed branch reproduces the saved tree exactly.
    let landed_tree = git(&repo.path, &["rev-parse", "HEAD^{tree}"]);
    assert_eq!(landed_tree, pre_land_tree);

    let status = git(&repo.path, &["status", "--porcelain"]);
    assert!(status.trim().is_empty(), "expected clean tree: {status}");
}

#[test]
fn landing_stages_a_deleted_file_whole() {
    let _lock = acquire_cwd_lock();
    let repo = init_repo();

    std::fs::remove_file(repo.path.join("other.txt")).expect("file should be removed");
    git(&repo.path, &["add", "-A"]);
    git(&repo.path, &["commit", "-m", "[kite] save 12:00:00"]);

    let pre_land_tree = git(&repo.path, &["rev-parse", "HEAD^{tree}"]);
    let scope = collect_land_scope_in_repo(&repo.path, false)
        .expect("land scope should collect")
        .expect("kite saves should be landable");
    assert_eq!(scope.files.paths(), vec!["other.txt"]);

    execute_land_in_repo(
        &repo.path,
        &scope.base,
        &[files_commit("chore: drop other", &["other.txt"])],
    )
    .expect("a deletion should land");

    assert_eq!(
        git(&repo.path, &["rev-parse", "HEAD^{tree}"]),
        pre_land_tree
    );
    let listed = git(&repo.path, &["ls-tree", "--name-only", "HEAD"]);
    assert!(
        !listed.lines().any(|line| line == "other.txt"),
        "the deleted file survived landing: {listed}"
    );
}

#[test]
fn landing_stages_a_file_force_added_past_an_ignore_rule() {
    let _lock = acquire_cwd_lock();
    let repo = init_repo();

    write_file(&repo.path, ".gitignore", "secret.txt\n");
    git(&repo.path, &["add", ".gitignore"]);
    git(&repo.path, &["commit", "-m", "chore: ignore secrets"]);

    // Deliberately saved past the ignore rule; landing must be able to
    // stage it back, or the save would be impossible to land.
    write_file(&repo.path, "secret.txt", "saved anyway\n");
    git(&repo.path, &["add", "-f", "secret.txt"]);
    git(&repo.path, &["commit", "-m", "[kite] save 12:00:00"]);

    let scope = collect_land_scope_in_repo(&repo.path, false)
        .expect("land scope should collect")
        .expect("kite saves should be landable");

    execute_land_in_repo(
        &repo.path,
        &scope.base,
        &[files_commit("feat: add secret", &["secret.txt"])],
    )
    .expect("an ignored but saved file should still land");

    assert_eq!(
        git(&repo.path, &["show", "HEAD:secret.txt"]),
        "saved anyway\n"
    );
}

/// A rename reaches landing as a deletion plus an addition. Both halves
/// have to be staged, or the old file survives into the landed tree.
#[test]
fn landing_a_rename_drops_the_old_path_and_preserves_the_tree() {
    let _lock = acquire_cwd_lock();
    let repo = init_repo();

    write_file(&repo.path, "old.txt", "one\ntwo\nthree\nfour\nfive\n");
    git(&repo.path, &["add", "-A"]);
    git(&repo.path, &["commit", "-m", "chore: add old"]);

    git(&repo.path, &["mv", "old.txt", "new.txt"]);
    git(&repo.path, &["commit", "-m", "[kite] save 12:00:00"]);

    let pre_land_tree = git(&repo.path, &["rev-parse", "HEAD^{tree}"]);
    let scope = collect_land_scope_in_repo(&repo.path, false)
        .expect("land scope should collect")
        .expect("kite saves should be landable");
    assert_eq!(scope.files.paths(), ["new.txt", "old.txt"]);

    execute_land_in_repo(
        &repo.path,
        &scope.base,
        &[files_commit(
            "refactor: rename old to new",
            &["new.txt", "old.txt"],
        )],
    )
    .expect("a rename should land");

    assert_eq!(
        git(&repo.path, &["rev-parse", "HEAD^{tree}"]),
        pre_land_tree
    );
    let listed = git(&repo.path, &["ls-tree", "--name-only", "HEAD"]);
    assert!(listed.lines().any(|line| line == "new.txt"), "{listed}");
    assert!(!listed.lines().any(|line| line == "old.txt"), "{listed}");
}

/// A mode change carries no hunks at all, so there is nothing but the
/// path to go on.
#[cfg(unix)]
#[test]
fn landing_a_mode_only_change_stages_it() {
    use std::os::unix::fs::PermissionsExt;

    let _lock = acquire_cwd_lock();
    let repo = init_repo();

    write_file(&repo.path, "run.sh", "echo hi\n");
    git(&repo.path, &["add", "-A"]);
    git(&repo.path, &["commit", "-m", "chore: add script"]);

    std::fs::set_permissions(
        repo.path.join("run.sh"),
        std::fs::Permissions::from_mode(0o755),
    )
    .expect("script should become executable");
    git(&repo.path, &["add", "-A"]);
    git(&repo.path, &["commit", "-m", "[kite] save 12:00:00"]);

    let pre_land_tree = git(&repo.path, &["rev-parse", "HEAD^{tree}"]);
    let scope = collect_land_scope_in_repo(&repo.path, false)
        .expect("land scope should collect")
        .expect("kite saves should be landable");
    assert_eq!(scope.files.paths(), ["run.sh"]);

    execute_land_in_repo(
        &repo.path,
        &scope.base,
        &[files_commit("chore: make run.sh executable", &["run.sh"])],
    )
    .expect("a mode change should land");

    assert_eq!(
        git(&repo.path, &["rev-parse", "HEAD^{tree}"]),
        pre_land_tree
    );
    assert_eq!(
        git(&repo.path, &["ls-tree", "HEAD", "run.sh"])
            .split_whitespace()
            .next(),
        Some("100755")
    );
}

#[test]
fn collect_land_scope_keeps_cancelled_saves_available_for_cleanup() {
    let _lock = acquire_cwd_lock();
    let repo = init_repo();

    write_file(&repo.path, "scratch.txt", "temporary\n");
    git(&repo.path, &["add", "-A"]);
    git(&repo.path, &["commit", "-m", "[kite] save 12:00:00"]);

    std::fs::remove_file(repo.path.join("scratch.txt")).expect("file should be removed");
    git(&repo.path, &["add", "-A"]);
    git(&repo.path, &["commit", "-m", "[kite] save 12:00:01"]);

    // A zero-file scope still needs landing so its saves can be removed.
    let scope = collect_land_scope_in_repo(&repo.path, false).expect("land scope should resolve");
    let scope = scope.expect("cancelled saves still need cleanup");
    assert_eq!(scope.save_count, 2);
    assert!(scope.files.paths().is_empty());
}

/// `core.quotepath` is on by default, so a non-ASCII path git prints
/// without `-z` comes back C-escaped and names no file at all.
#[test]
fn landing_stages_non_ascii_paths_unescaped() {
    let _lock = acquire_cwd_lock();
    let repo = init_repo();

    write_file(&repo.path, "café/résumé.md", "base\n");
    git(&repo.path, &["add", "-A"]);
    git(&repo.path, &["commit", "-m", "chore: add notes"]);

    write_file(&repo.path, "café/résumé.md", "updated\n");
    git(&repo.path, &["add", "-A"]);
    git(&repo.path, &["commit", "-m", "[kite] save 12:00:00"]);

    let pre_land_tree = git(&repo.path, &["rev-parse", "HEAD^{tree}"]);
    let scope = collect_land_scope_in_repo(&repo.path, false)
        .expect("land scope should collect")
        .expect("kite saves should be landable");
    assert_eq!(scope.files.paths(), ["café/résumé.md"]);

    execute_land_in_repo(
        &repo.path,
        &scope.base,
        &[files_commit("docs: update notes", &["café/résumé.md"])],
    )
    .expect("a non-ASCII path should land");

    assert_eq!(
        git(&repo.path, &["rev-parse", "HEAD^{tree}"]),
        pre_land_tree
    );
}

/// Names git would otherwise read as a pattern rather than a path: a
/// glob, a pathspec-magic prefix, and one git has to quote in the diff
/// header. Each used to either fail the land outright or drag an
/// unassigned file into the wrong commit.
#[cfg(unix)]
#[test]
fn landing_stages_awkward_paths_literally() {
    let _lock = acquire_cwd_lock();
    let repo = init_repo();

    write_file(&repo.path, "axxb.txt", "the file a*b.txt would match\n");
    git(&repo.path, &["add", "-A"]);
    git(&repo.path, &["commit", "-m", "chore: add the decoy"]);

    write_file(&repo.path, "a*b.txt", "glob-shaped name\n");
    write_file(&repo.path, ":colon.txt", "pathspec-magic-shaped name\n");
    write_file(&repo.path, "quote\".txt", "quoted in the diff header\n");
    write_file(
        &repo.path,
        "axxb.txt",
        "changed too, and assigned elsewhere\n",
    );
    git(&repo.path, &["add", "-A"]);
    git(&repo.path, &["commit", "-m", "[kite] save 12:00:00"]);

    let pre_land_tree = git(&repo.path, &["rev-parse", "HEAD^{tree}"]);
    let scope = collect_land_scope_in_repo(&repo.path, false)
        .expect("land scope should collect")
        .expect("kite saves should be landable");
    assert_eq!(
        scope.files.paths(),
        [":colon.txt", "a*b.txt", "axxb.txt", "quote\".txt"]
    );

    let commits = [
        files_commit(
            "feat: awkward names",
            &["a*b.txt", ":colon.txt", "quote\".txt"],
        ),
        files_commit("chore: update the decoy", &["axxb.txt"]),
    ];
    execute_land_in_repo(&repo.path, &scope.base, &commits).expect("land should succeed");

    // The glob-shaped name must not have swallowed the file it matches.
    let first = git(
        &repo.path,
        &["show", "--name-only", "--pretty=format:", "HEAD^"],
    );
    assert!(!first.lines().any(|line| line == "axxb.txt"), "{first}");
    assert_eq!(
        git(&repo.path, &["show", "HEAD:axxb.txt"]),
        "changed too, and assigned elsewhere\n"
    );
    assert_eq!(
        git(&repo.path, &["rev-parse", "HEAD^{tree}"]),
        pre_land_tree
    );
}

/// `git add` re-runs the repository's clean filters, so a landed file has
/// to hash back to the blob its save recorded.
#[test]
fn landing_a_normalized_file_reproduces_the_saved_tree() {
    let _lock = acquire_cwd_lock();
    let repo = init_repo();

    // `text=auto` makes `git add` normalize CRLF to LF on the way into the
    // index, while the worktree keeps its carriage returns.
    write_file(&repo.path, ".gitattributes", "* text=auto\n");
    git(&repo.path, &["add", ".gitattributes"]);
    git(
        &repo.path,
        &["commit", "-m", "chore: normalize line endings"],
    );

    write_file(&repo.path, "code.txt", "alpha\r\nbeta\r\n");
    git(&repo.path, &["add", "code.txt"]);
    git(&repo.path, &["commit", "-m", "[kite] save 12:00:00"]);

    let pre_land_tree = git(&repo.path, &["rev-parse", "HEAD^{tree}"]);
    let scope = collect_land_scope_in_repo(&repo.path, false)
        .expect("land scope should collect")
        .expect("kite saves should be landable");

    execute_land_in_repo(
        &repo.path,
        &scope.base,
        &[files_commit("feat: add code", &["code.txt"])],
    )
    .expect("land should succeed");

    assert_eq!(
        git(&repo.path, &["rev-parse", "HEAD^{tree}"]),
        pre_land_tree
    );
}

#[test]
fn render_land_plan_lists_every_file_at_exactly_the_cap() {
    colored::control::set_override(false);

    let many: Vec<String> = (0..MAX_PLAN_FILES_SHOWN)
        .map(|index| format!("src/f{index}.rs"))
        .collect();
    let listed: Vec<&str> = many.iter().map(String::as_str).collect();
    let plan = render_land_plan(&[files_commit("chore: sweep", &listed)], 1);

    assert!(plan.contains(&format!("└─ src/f{}.rs\n", MAX_PLAN_FILES_SHOWN - 1)));
    assert!(!plan.contains("… and"));
}

#[test]
fn render_land_plan_summarizes_a_commit_with_too_many_files() {
    colored::control::set_override(false);

    let many: Vec<String> = (0..30).map(|index| format!("src/f{index}.rs")).collect();
    let listed: Vec<&str> = many.iter().map(String::as_str).collect();
    let plan = render_land_plan(&[files_commit("chore: sweep", &listed)], 1);

    assert!(plan.contains("     ├─ src/f0.rs\n"));
    assert!(plan.contains("     ├─ src/f11.rs\n"));
    assert!(!plan.contains("src/f12.rs"));
    assert!(plan.contains("… and 18 more"));
}

#[test]
fn render_land_plan_keeps_multi_line_messages_out_of_the_file_tree() {
    colored::control::set_override(false);

    let plan = render_land_plan(
        &[files_commit(
            "feat(api): add webhooks\n\nExplains the change\nover several lines.",
            &["src/api.rs"],
        )],
        1,
    );

    assert!(plan.contains("  1. feat(api): add webhooks\n"));
    assert!(plan.contains("+ 2 body lines"));
    assert!(!plan.contains("\nExplains the change"));
    assert!(plan.contains("     └─ src/api.rs\n"));
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
        with_repo_cwd(&repo.path, head_symbolic_ref).is_none(),
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
        with_repo_cwd(&linked, head_symbolic_ref).is_none(),
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

    let commits = [files_commit("feat: bootstrap project", &["tracked.txt"])];
    execute_land_in_repo(&repo.path, &scope.base, &commits)
        .expect("a detached root land should succeed");

    // A root rewrite has to build on an orphan branch; HEAD must come back
    // off it, and the branch must not be left behind.
    assert!(
        with_repo_cwd(&repo.path, head_symbolic_ref).is_none(),
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
        with_repo_cwd(&repo.path, head_symbolic_ref).is_none(),
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
    assert!(with_repo_cwd(&repo.path, head_symbolic_ref).is_none());
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
        with_repo_cwd(&repo.path, head_symbolic_ref).is_none(),
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
    let err = undo_in_repo(&repo.path).expect_err("unrelated detached history must never be reset");
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
    let git_dir = with_repo_cwd(&repo.path, current_worktree_key).expect("git dir should resolve");
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
    let git_dir = with_repo_cwd(&repo.path, current_worktree_key).expect("git dir should resolve");
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

    let commits = [files_commit("feat: bootstrap project", &["tracked.txt"])];
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
        "You are back on `{original_branch}` with every save intact"
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
    save_two_file_change(&repo.path);

    let scope = collect_land_scope_in_repo(&repo.path, false)
        .expect("scope")
        .expect("saves");

    // A hook is the only way to observe the repository mid-land.
    install_pre_commit_hook(
        &repo.path,
        "#!/bin/sh\ngit branch --format='%(refname:short)' >> .git/seen\n",
    );

    let commits = [
        files_commit("feat: upcase code", &["code.txt"]),
        files_commit("docs: update notes", &["docs.txt"]),
    ];
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
    assert!(with_repo_cwd(&repo.path, head_symbolic_ref).is_none());
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
            &transaction_ref,
        )
        .expect("marker should install");
        prepare_landing_head(&KiteBase::Root, pre_land_sha.trim(), &transaction_ref)
            .expect("root rewrite should become unborn");
        transaction
    });
    assert!(check_ref_in(&repo.path, "HEAD").is_none());

    undo_in_repo(&repo.path).expect("unborn transaction should recover");
    assert!(with_repo_cwd(&repo.path, head_symbolic_ref).is_none());
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
            &collision_ref,
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

    install_pre_commit_hook(
        &repo.path,
        "#!/bin/sh\nprintf 'reformatted by a hook\\n' > tracked.txt\nexit 1\n",
    );

    execute_land_in_repo(
        &repo.path,
        &scope.base,
        &[files_commit("feat: land", &["tracked.txt"])],
    )
    .expect_err("land should fail");

    let content =
        std::fs::read_to_string(repo.path.join("tracked.txt")).expect("worktree file should exist");
    assert_eq!(content, "reformatted by a hook\n");
}
