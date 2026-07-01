use anyhow::{Context, Result, anyhow};
use chrono::Local;
use colored::*;
use std::collections::HashSet;

use crate::git::{
    KiteBase, changed_files_for_base, check_ref, commit_git, diff_for_base, execute_git,
    get_current_branch, has_head_commit, has_remote, kite_save_stack, sorted_files,
};
use crate::synth::{CommitGroup, normalize_groups, synthesize_groups};
use crate::ui::{Spinner, confirm, pluralize, print_provider_failures, prompt_line};

#[derive(Clone, Debug)]
struct LandScope {
    base: KiteBase,
    save_count: usize,
    diff: String,
    actual_files: HashSet<String>,
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
        let synthesized = synthesize_groups(&scope.diff, &scope.actual_files).await;
        spinner.stop();

        let (raw_groups, provider_label) = match synthesized {
            Ok(result) => result,
            Err(failures) => {
                print_provider_failures(&failures);
                let Some(message) = prompt_line("One commit message (blank to abort)")? else {
                    println!("{} Aborted — no history changed", "·".red());
                    return Ok(());
                };
                (
                    vec![CommitGroup {
                        message,
                        files: sorted_files(&scope.actual_files),
                    }],
                    "manual",
                )
            }
        };

        let groups = normalize_groups(raw_groups, &scope.actual_files);
        if groups.is_empty() {
            anyhow::bail!("No changed files were assigned to landed commit groups.");
        }

        print!(
            "{}",
            render_land_plan(&groups, provider_label, scope.save_count)
        );

        if !auto_confirm && !confirm("Rewrite history?")? {
            println!("{} Aborted — no history changed", "·".red());
            return Ok(());
        }

        execute_land(&scope.base, &groups)?;

        if push && has_remote() {
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

pub(crate) fn publish_current_branch() -> Result<()> {
    if !has_remote() {
        println!("{} No remote — history stays local", "·".dimmed());
        return Ok(());
    }

    let branch = get_current_branch()?;

    let spinner = Spinner::start(format!("Publishing {branch}"));
    let _ = execute_git(&["pull", "--rebase", "origin", &branch]);
    let pushed = execute_git(&[
        "push",
        "--set-upstream",
        "origin",
        &branch,
        "--force-with-lease",
    ]);
    spinner.stop();

    pushed?;
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
    let actual_files = changed_files_for_base(&stack.base)?;

    if actual_files.is_empty() {
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
        diff,
        actual_files,
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

/// Renders the proposed history as a numbered list of commits, each with a
/// small file tree underneath.
fn render_land_plan(groups: &[CommitGroup], provider_label: &str, save_count: usize) -> String {
    let mut plan = format!(
        "{} Plan ({provider_label}): {} {} {}\n\n",
        "·".cyan(),
        pluralize(save_count, "save"),
        "→".dimmed(),
        pluralize(groups.len(), "commit"),
    );

    for (index, group) in groups.iter().enumerate() {
        plan.push_str(&format!("  {}. {}\n", index + 1, group.message.bold()));
        for (position, file) in group.files.iter().enumerate() {
            let glyph = if position + 1 == group.files.len() {
                "└─"
            } else {
                "├─"
            };
            plan.push_str(&format!("     {} {}\n", glyph.dimmed(), file));
        }
    }

    plan.push('\n');
    plan
}

fn execute_land(base: &KiteBase, groups: &[CommitGroup]) -> Result<()> {
    let original_branch = get_current_branch()?;
    let pre_land_sha = execute_git(&["rev-parse", "HEAD"])?;
    let recovery_branch = format!("kite-recovery-{}", Local::now().format("%Y%m%d%H%M%S"));

    execute_git(&["update-ref", "refs/kite/pre_land", pre_land_sha.trim()])?;

    if let Err(err) = prepare_landing_branch(base, &recovery_branch).and_then(|_| {
        commit_groups(groups)?;
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

fn commit_groups(groups: &[CommitGroup]) -> Result<()> {
    for group in groups {
        for file in &group.files {
            execute_git(&["add", "--", file])?;
        }
        commit_git(&group.message)?;
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
        groups: &[CommitGroup],
    ) -> Result<()> {
        with_repo_cwd(repo, || execute_land(base, groups))
    }

    fn undo_in_repo(repo: &std::path::Path) -> Result<()> {
        with_repo_cwd(repo, undo)
    }

    #[test]
    fn render_land_plan_numbers_commits_and_closes_file_trees() {
        // Pin colors off so we assert the structural layout, not ANSI codes.
        colored::control::set_override(false);

        let plan = render_land_plan(
            &[
                CommitGroup {
                    message: "feat(api): add webhooks".to_string(),
                    files: vec!["src/api.rs".to_string(), "src/hooks.rs".to_string()],
                },
                CommitGroup {
                    message: "docs: refresh readme".to_string(),
                    files: vec!["README.md".to_string()],
                },
            ],
            "local",
            3,
        );

        assert!(plan.contains("Plan (local): 3 saves → 2 commits"));
        assert!(plan.contains("  1. feat(api): add webhooks\n"));
        assert!(plan.contains("     ├─ src/api.rs\n"));
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
        assert!(scope.actual_files.contains("tracked.txt"));
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
            &[CommitGroup {
                message: "feat: land tracked change".to_string(),
                files: vec!["tracked.txt".to_string()],
            }],
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
            &[CommitGroup {
                message: "feat: land tracked change".to_string(),
                files: vec!["tracked.txt".to_string()],
            }],
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

        execute_land_in_repo(
            &repo.path,
            &scope.base,
            &[CommitGroup {
                message: "feat: bootstrap project".to_string(),
                files: vec!["tracked.txt".to_string()],
            }],
        )
        .expect("root land should succeed");

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
                &[CommitGroup {
                    message: "feat: land nested change".to_string(),
                    files: vec!["nested/feature.txt".to_string()],
                }],
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
            &[CommitGroup {
                message: "feat: land tracked change".to_string(),
                files: vec!["missing.txt".to_string()],
            }],
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
