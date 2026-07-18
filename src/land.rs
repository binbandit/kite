use anyhow::{Context, Result, anyhow};
use chrono::Local;
use colored::*;
use std::collections::HashSet;

use crate::ai::flatten_error;
use crate::git::{
    KiteBase, apply_cached_patch, check_ref, commit_git, diff_for_base, execute_git,
    execute_git_with, get_current_branch, has_head_commit, has_remote, kite_save_stack,
};
use crate::hunks::{DiffUnits, FileStat, parse_diff};
use crate::synth::{CommitGroup, normalize_groups, synthesize_groups};
use crate::ui::{Spinner, confirm, pluralize, print_ai_unavailable, prompt_line};

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

pub(crate) async fn land(push: bool, auto_confirm: bool, allow_dirty: bool) -> Result<()> {
    let stashed = if allow_dirty {
        stash_dirty_worktree_for_land()?
    } else {
        false
    };

    let land_result = (async {
        let Some(scope) = collect_land_scope(allow_dirty)? else {
            return Ok(());
        };

        let spinner = Spinner::start("Synthesizing");
        let synthesized = synthesize_groups(&scope.units).await;
        spinner.stop();

        let commits = match synthesized {
            Ok(raw_groups) => {
                let groups = normalize_groups(raw_groups, &scope.units.unit_ids());
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
                vec![whole_tree_commit(&scope.units, message)]
            }
        };

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

        execute_land(&scope.base, &commits)?;

        if push {
            println!("{} Landed", "✓".green());
            publish_current_branch().context("Landed locally, but publishing failed")?;
        } else if has_remote() {
            println!(
                "{} Landed — review, then {} or {}",
                "✓".green(),
                "kt publish".bold(),
                "kt pr".bold()
            );
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

/// Pushes with `--force-with-lease` and nothing else. Deliberately no
/// `pull --rebase` first: after a land the remote holds the old saves, and
/// rebasing onto them would resurrect the history we just rewrote. The lease
/// protects anything pushed by someone else — the push is rejected and the
/// user decides.
pub(crate) fn publish_current_branch() -> Result<()> {
    if !has_remote() {
        println!("{} No remote — history stays local", "·".dimmed());
        return Ok(());
    }

    let branch = get_current_branch()?;

    let spinner = Spinner::start(format!("Publishing {branch}"));
    let pushed = execute_git(&[
        "push",
        "--set-upstream",
        "origin",
        &branch,
        "--force-with-lease",
    ]);
    spinner.stop();

    pushed.with_context(|| {
        format!(
            "Push rejected. If `origin/{branch}` has commits you don't have, fetch and reconcile them first."
        )
    })?;
    println!("{} Published {}", "✓".green(), branch.bold());
    Ok(())
}

pub(crate) fn undo() -> Result<()> {
    let Some(pre_land_sha) = check_ref("refs/kite/pre_land") else {
        println!("{} Nothing to undo — no land recorded", "·".yellow());
        return Ok(());
    };

    let status = execute_git(&["status", "--porcelain"])?;
    if !status.trim().is_empty() {
        anyhow::bail!(
            "Working directory is not clean. Please `kt` your changes or stash them before undoing."
        );
    }

    execute_git(&["reset", "--hard", &pre_land_sha])?;
    execute_git(&["update-ref", "-d", "refs/kite/pre_land"])?;

    if has_remote() {
        let branch = get_current_branch()?;
        let spinner = Spinner::start("Reverting remote");
        let reverted = execute_git(&["push", "--force-with-lease", "origin", &branch]);
        spinner.stop();
        if reverted.is_err() {
            println!(
                "{} Remote not reverted — it may have diverged",
                "·".yellow()
            );
        }
    }

    println!("{} Restored pre-land saves", "✓".green());
    Ok(())
}

fn collect_land_scope(allow_dirty: bool) -> Result<Option<LandScope>> {
    if !has_head_commit() {
        println!(
            "{} No commits yet — make an initial commit before landing",
            "·".yellow()
        );
        return Ok(None);
    }

    if !allow_dirty {
        let status = execute_git(&["status", "--porcelain"])?;
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
    let units = parse_diff(&diff);

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

        commits.push(LandCommit {
            message: group.message.clone(),
            files: files.iter().cloned().map(FileStat::whole).collect(),
            stage: StageOp::WholeFiles(files),
        });
    }

    commits
}

/// Manual fallback: every changed file in one commit.
fn whole_tree_commit(units: &DiffUnits, message: String) -> LandCommit {
    let files = units.all_files();
    LandCommit {
        message,
        files: files.iter().cloned().map(FileStat::whole).collect(),
        stage: StageOp::WholeFiles(files),
    }
}

/// Renders the proposed history as a numbered list of commits, each with a
/// small file tree underneath. Files split across commits show how many of
/// their hunks each commit takes.
fn render_land_plan(commits: &[LandCommit], save_count: usize) -> String {
    let mut plan = format!(
        "{} Plan: {} {} {}\n\n",
        "·".cyan(),
        pluralize(save_count, "save"),
        "→".dimmed(),
        pluralize(commits.len(), "commit"),
    );

    for (index, commit) in commits.iter().enumerate() {
        plan.push_str(&format!("  {}. {}\n", index + 1, commit.message.bold()));
        for (position, file) in commit.files.iter().enumerate() {
            let glyph = if position + 1 == commit.files.len() {
                "└─"
            } else {
                "├─"
            };
            let mut line = file.path.clone();
            if file.selected < file.total {
                line.push_str(&format!(" ({}/{} hunks)", file.selected, file.total));
            }
            plan.push_str(&format!("     {} {}\n", glyph.dimmed(), line));
        }
    }

    plan.push('\n');
    plan
}

fn execute_land(base: &KiteBase, commits: &[LandCommit]) -> Result<()> {
    let original_branch = get_current_branch()?;
    let pre_land_sha = execute_git(&["rev-parse", "HEAD"])?;
    let recovery_branch = format!("kite-recovery-{}", Local::now().format("%Y%m%d%H%M%S"));

    execute_git(&["update-ref", "refs/kite/pre_land", pre_land_sha.trim()])?;

    if let Err(err) = prepare_landing_branch(base, &recovery_branch).and_then(|_| {
        create_commits(commits)?;
        finalize_landed_branch(&original_branch, &recovery_branch)
    }) {
        anyhow::bail!(
            "{}",
            render_land_failure(&err, &original_branch, &recovery_branch)
        );
    }

    Ok(())
}

fn render_land_failure(
    err: &anyhow::Error,
    original_branch: &str,
    recovery_branch: &str,
) -> String {
    let mut message = format!(
        "{err}\n\nLanding stopped before updating `{original_branch}`.\nKite kept the in-progress landing state on recovery branch `{recovery_branch}` so partial commits or staged changes are not lost."
    );

    if get_current_branch()
        .map(|branch| branch == recovery_branch)
        .unwrap_or(false)
    {
        message.push_str(&format!("\nYou are currently on `{recovery_branch}`."));
    }

    message.push_str(&format!(
        "\nFix the issue there and rerun `kt land`, or run `git switch {original_branch}` if you want to abandon this landing attempt.\nRecovery ref: `refs/kite/pre_land`."
    ));

    message
}

fn prepare_landing_branch(base: &KiteBase, temp_branch: &str) -> Result<()> {
    match base {
        KiteBase::Commit(base_sha) => {
            execute_git(&["checkout", "-b", temp_branch])?;
            execute_git(&["reset", "--soft", base_sha])?;
            execute_git(&["reset"])?;
        }
        KiteBase::Root => {
            execute_git(&["checkout", "--orphan", temp_branch, "HEAD"])?;
            execute_git(&["read-tree", "--empty"])?;
        }
    }

    Ok(())
}

fn create_commits(commits: &[LandCommit]) -> Result<()> {
    for commit in commits {
        match &commit.stage {
            StageOp::Patch(patch) => apply_cached_patch(patch)?,
            StageOp::WholeFiles(files) => {
                for file in files {
                    execute_git(&["add", "--", file])?;
                }
            }
        }
        commit_git(&commit.message)?;
    }

    Ok(())
}

fn finalize_landed_branch(original_branch: &str, temp_branch: &str) -> Result<()> {
    let new_head = execute_git(&["rev-parse", "HEAD"])?;
    execute_git(&["branch", "-f", original_branch, new_head.trim()])?;
    execute_git(&["checkout", original_branch])?;
    execute_git(&["branch", "-D", temp_branch])?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{
        acquire_cwd_lock, git, init_repo, init_root_kite_repo, with_repo_cwd, write_file,
    };

    fn collect_land_scope_in_repo(
        repo: &std::path::Path,
        allow_dirty: bool,
    ) -> Result<Option<LandScope>> {
        with_repo_cwd(repo, || collect_land_scope(allow_dirty))
    }

    fn execute_land_in_repo(
        repo: &std::path::Path,
        base: &KiteBase,
        commits: &[LandCommit],
    ) -> Result<()> {
        with_repo_cwd(repo, || execute_land(base, commits))
    }

    fn undo_in_repo(repo: &std::path::Path) -> Result<()> {
        with_repo_cwd(repo, undo)
    }

    fn whole_files_commit(message: &str, files: &[&str]) -> LandCommit {
        LandCommit {
            message: message.to_string(),
            files: files
                .iter()
                .map(|file| FileStat::whole(file.to_string()))
                .collect(),
            stage: StageOp::WholeFiles(files.iter().map(|file| file.to_string()).collect()),
        }
    }

    fn group(message: &str, hunks: &[&str]) -> CommitGroup {
        CommitGroup {
            message: message.to_string(),
            hunks: hunks.iter().map(|id| id.to_string()).collect(),
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
            &[whole_files_commit(
                "feat: land tracked change",
                &["tracked.txt"],
            )],
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
            &[whole_files_commit(
                "feat: land tracked change",
                &["tracked.txt"],
            )],
        )
        .expect("land should succeed");

        undo_in_repo(&repo.path).expect("undo should succeed");

        let restored_head = git(&repo.path, &["rev-parse", "HEAD"]);
        assert_eq!(restored_head.trim(), pre_land_sha);

        let status = git(&repo.path, &["status", "--porcelain"]);
        assert!(status.trim().is_empty());
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

        let scope = with_repo_cwd(&nested, || collect_land_scope(false))
            .expect("land scope should collect")
            .expect("kite saves should be landable");

        with_repo_cwd(&nested, || {
            execute_land(
                &scope.base,
                &[whole_files_commit(
                    "feat: land nested change",
                    &["nested/feature.txt"],
                )],
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
            &[whole_files_commit(
                "feat: land tracked change",
                &["missing.txt"],
            )],
        )
        .expect_err("land should fail when a grouped file cannot be staged");

        let rendered = format!("{err:#}");
        assert!(rendered.contains(&format!(
            "Landing stopped before updating `{original_branch}`"
        )));
        assert!(rendered.contains("recovery branch `kite-recovery-"));
        assert!(rendered.contains("Fix the issue there and rerun `kt land`"));

        let current_branch = git(&repo.path, &["rev-parse", "--abbrev-ref", "HEAD"]);
        assert!(current_branch.trim().starts_with("kite-recovery-"));
    }
}
