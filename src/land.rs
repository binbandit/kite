use anyhow::{anyhow, Result};
use chrono::Local;
use colored::*;
use std::collections::HashSet;
use std::io::{self, Write};
use std::process::{Command, Stdio};

use crate::git::{
    KiteBase, changed_files_for_base, check_ref, commit_git, diff_for_base, execute_git,
    get_current_branch, get_kite_base, has_head_commit, has_remote, sorted_files,
};
use crate::synth::{
    CommitGroup, ProviderFailure, flatten_error, normalize_groups, synthesize_groups,
};

#[derive(Clone, Debug)]
struct LandScope {
    base: KiteBase,
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

        print!("{} Synthesizing... ", "·".cyan());
        io::stdout().flush()?;

        let (raw_groups, provider_label) =
            match synthesize_groups(&scope.diff, &scope.actual_files).await {
                Ok((groups, provider_label)) => {
                    println!("({provider_label})");
                    (groups, provider_label)
                }
                Err(failures) => {
                    println!("{}", "unavailable".dimmed());
                    let Some(message) = prompt_manual_commit_message(&failures)? else {
                        println!("{} Aborted. No history changed.", "·".red());
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

        println!();
        print_land_plan(&groups, provider_label);

        if !auto_confirm && !confirm_land(push)? {
            println!("{} Aborted. No history changed.", "·".red());
            return Ok(());
        }

        execute_land(&scope.base, &groups)?;

        if push {
            publish_current_branch()?;
            println!("{}\n", render_tree_tail("Landed and published").green());
        } else {
            if has_remote() {
                let current_branch = get_current_branch()?;
                println!(
                    "{} Review the rewritten history, then run `kt publish` or `git push --force-with-lease origin {}`.",
                    "·".dimmed(),
                    current_branch
                );
            }
            println!("{}\n", render_tree_tail("Landed locally").green());
        }

        Ok(())
    })
    .await;

    if stashed {
        if let Err(restore_error) = restore_dirty_worktree_for_land() {
            return match land_result {
                Ok(_) => Err(restore_error),
                Err(land_error) => Err(anyhow!(
                    "{land_error}\n\nIn addition, restoring your stashed changes failed: {restore_error}"
                )),
            };
        }
    }

    land_result
}

pub(crate) fn publish_current_branch() -> Result<()> {
    if !has_remote() {
        println!(
            "{} No remote configured. Landed history is local only.",
            "·".dimmed()
        );
        return Ok(());
    }

    let current_branch = get_current_branch()?;

    print!(
        "{} ",
        render_tree_line(&format!("{}", "│".dimmed()), "Pulling remote changes...")
    );
    io::stdout().flush()?;
    match execute_git(&["pull", "--rebase", "origin", &current_branch]) {
        Ok(_) => println!("Done"),
        Err(_) => println!("{}", "Skipped (no upstream or nothing to pull)".dimmed()),
    }

    print!(
        "{} ",
        render_tree_line(&format!("{}", "│".dimmed()), "Publishing to remote...")
    );
    io::stdout().flush()?;

    match execute_git(&[
        "push",
        "--set-upstream",
        "origin",
        &current_branch,
        "--force-with-lease",
    ]) {
        Ok(_) => println!("Done"),
        Err(_) => println!("{}", "Failed (You may need to push manually)".yellow()),
    }

    Ok(())
}

pub(crate) fn undo() -> Result<()> {
    let pre_land_sha = match check_ref("refs/kite/pre_land") {
        Some(sha) => sha,
        None => {
            println!(
                "{} Nothing to undo. No previous land operation found.",
                "·".yellow()
            );
            return Ok(());
        }
    };

    let status = execute_git(&["status", "--porcelain"])?;
    if !status.trim().is_empty() {
        anyhow::bail!(
            "Working directory is not clean. Please `kt save` or stash your changes before undoing."
        );
    }

    print!("{} Rewinding timeline... ", "·".cyan());
    io::stdout().flush()?;
    execute_git(&["reset", "--hard", &pre_land_sha])?;
    execute_git(&["update-ref", "-d", "refs/kite/pre_land"])?;
    println!("Done");

    if has_remote() {
        let current_branch = get_current_branch()?;
        print!("{} Reverting remote... ", "·".cyan());
        io::stdout().flush()?;

        match Command::new("git")
            .args(["push", "--force-with-lease", "origin", &current_branch])
            .stderr(Stdio::null())
            .stdout(Stdio::null())
            .status()
        {
            Ok(status) if status.success() => println!("Done"),
            _ => println!("{}", "Failed (Remote may have diverged)".yellow()),
        }
    }

    println!("  {}\n", "└─ Restored previous saves".green());
    Ok(())
}

fn collect_land_scope(allow_dirty: bool) -> Result<Option<LandScope>> {
    if !has_head_commit() {
        println!(
            "{} Repository has no commits yet. Create an initial commit before running `kt land`.",
            "·".yellow()
        );
        return Ok(None);
    }

    if !allow_dirty {
        let status = execute_git(&["status", "--porcelain"])?;
        if !status.trim().is_empty() {
            anyhow::bail!(
                "Working directory must be clean before `kt land`. Run `kt` to snapshot current work or stash unrelated changes first."
            );
        }
    }

    let Some(base) = get_kite_base()? else {
        println!(
            "{} Nothing to land. Create one or more contiguous `[kite] save` commits first.",
            "·".dimmed()
        );
        return Ok(None);
    };

    let diff = diff_for_base(&base)?;
    let actual_files = changed_files_for_base(&base)?;

    if actual_files.is_empty() {
        println!(
            "{} Nothing to land. No file changes were found.",
            "·".dimmed()
        );
        return Ok(None);
    }

    Ok(Some(LandScope {
        base,
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

fn print_land_plan(groups: &[CommitGroup], provider_label: &str) {
    println!(
        "{} Proposed history using {} synthesis:",
        "·".cyan(),
        provider_label
    );

    for group in groups {
        println!(
            "{}",
            render_tree_line(
                &format!("{}", "│".dimmed()),
                &format!(
                    "{} ({})",
                    group.message,
                    pluralize(group.files.len(), "file")
                ),
            )
        );
        for file in &group.files {
            println!(
                "{}",
                render_tree_line(&format!("{} {}", "│".dimmed(), "├─".dimmed()), file)
            );
        }
    }
}

fn confirm_land(push: bool) -> Result<bool> {
    let action = if push {
        "rewrite local history and publish it"
    } else {
        "rewrite local history"
    };

    print!("{} Proceed and {}? [y/N]: ", "·".cyan(), action);
    io::stdout().flush()?;

    let mut response = String::new();
    io::stdin().read_line(&mut response)?;

    Ok(matches!(response.trim(), "y" | "Y" | "yes" | "YES"))
}

fn prompt_manual_commit_message(failures: &[ProviderFailure]) -> Result<Option<String>> {
    println!();
    println!("{} Automatic synthesis was unavailable:", "·".yellow());

    for failure in failures {
        println!(
            "{}",
            render_tree_line(
                &format!("{}", "│".dimmed()),
                &format!("{}: {}", failure.provider, flatten_error(&failure.error)),
            )
        );
    }

    print!(
        "{} Single commit message (leave blank to abort): ",
        "·".cyan()
    );
    io::stdout().flush()?;

    let mut msg = String::new();
    io::stdin().read_line(&mut msg)?;

    let msg = msg.trim();
    if msg.is_empty() {
        return Ok(None);
    }

    Ok(Some(msg.to_string()))
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
        println!(
            "{}",
            render_tree_line(&format!("{}", "│".dimmed()), &group.message)
        );
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

fn render_tree_line(prefix: &str, message: &str) -> String {
    format!("  {} {}", prefix, message)
}

fn render_tree_tail(message: &str) -> String {
    format!("  └─ {}", message)
}

fn pluralize(count: usize, singular: &str) -> String {
    if count == 1 {
        format!("1 {singular}")
    } else {
        format!("{count} {singular}s")
    }
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
    fn render_tree_lines_match_land_summary_layout() {
        assert_eq!(
            render_tree_line("│", "Publishing to remote... Done"),
            "  │ Publishing to remote... Done"
        );
        assert_eq!(render_tree_tail("Landed"), "  └─ Landed");
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

        let scope = with_repo_cwd(&nested, collect_land_scope)
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
