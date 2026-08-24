mod ai;
mod diff;
mod git;
mod land;
mod pr;
mod synth;
mod ui;

#[cfg(test)]
mod test_support;

use anyhow::Result;
use chrono::Local;
use clap::{CommandFactory, Parser, Subcommand};
use colored::*;
use std::process::ExitCode;

use crate::git::{
    Hooks, SAVE_PREFIX, active_git_operation, check_ref, commit_git, execute_git,
    execute_git_quiet, get_default_branch, has_remote, has_staged_changes, has_unmerged_paths,
    is_inside_git_repository, is_staged_status_line, kite_save_stack, lock_current_worktree,
};
use crate::land::{LandOptions, land, publish_current_branch, recovery_blocks_commands, undo};
use crate::pr::{PrOptions, create_pull_request};
use crate::ui::{Spinner, pluralize};

const WORKFLOW_HELP: &str = "\
Everyday flow:
  kt              quicksave everything on the current branch
  kt land         rewrite your saves into reviewable commits
  kt publish      push the branch (force-with-lease)
  kt pr           open a GitHub pull request with gh

Run `kt <command> --help` for details.";

#[derive(Parser)]
#[command(
    name = "kt",
    about = "Fast quicksaves and inspectable AI-assisted landing",
    version,
    after_help = WORKFLOW_HELP
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// Create or checkout a flow branch
    Go { name: String },
    /// Group Kite saves into reviewable local commits
    Land {
        /// Publish the rewritten branch after landing
        #[arg(long)]
        push: bool,
        /// Preserve local worktree changes by stashing them while landing
        #[arg(long)]
        allow_dirty: bool,
        /// Skip the confirmation prompt
        #[arg(short = 'y', long)]
        yes: bool,
        /// Reference tag to append to landed commit titles (e.g. PROJ-123)
        #[arg(short = 't', long)]
        tag: Option<String>,
        /// Skip Git hooks while creating the landed commits
        #[arg(long)]
        no_verify: bool,
    },
    /// Publish the current branch after reviewing local history
    Publish,
    /// Open a GitHub pull request for the current branch (requires gh)
    Pr {
        /// Create the pull request as a draft
        #[arg(long)]
        draft: bool,
        /// Target base branch (defaults to the repository's default branch)
        #[arg(long)]
        base: Option<String>,
        /// Skip the confirmation prompt
        #[arg(short = 'y', long)]
        yes: bool,
    },
    /// Undo the last quicksave, or the last land when there is no quicksave
    Undo,
}

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{} {error:#}", "✗".red());
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> Result<()> {
    let inside_repo = is_inside_git_repository()?;
    let _command_lock = if inside_repo {
        Some(lock_current_worktree()?)
    } else {
        None
    };
    let interrupted_land = inside_repo && recovery_blocks_commands()?;
    if interrupted_land && !matches!(&cli.command, Some(Commands::Undo)) {
        anyhow::bail!(
            "A `kt land` was interrupted in this worktree. Run `kt undo` to recover its saves before doing anything else."
        );
    }

    match cli.command {
        None if !inside_repo => {
            print!("{}", render_help());
            Ok(())
        }
        None => save(),
        Some(Commands::Go { name }) => go(&name),
        Some(Commands::Land {
            push,
            allow_dirty,
            yes,
            tag,
            no_verify,
        }) => block_on(land(LandOptions {
            push,
            yes,
            allow_dirty,
            tag,
            hooks: if no_verify { Hooks::Skip } else { Hooks::Run },
        })),
        Some(Commands::Publish) => publish_current_branch(),
        Some(Commands::Pr { draft, base, yes }) => {
            block_on(create_pull_request(PrOptions { draft, base, yes }))
        }
        Some(Commands::Undo) => undo(),
    }
}

fn block_on(future: impl Future<Output = Result<()>>) -> Result<()> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(future)
}

fn render_help() -> String {
    let mut command = Cli::command();
    format!("{}\n", command.render_help())
}

fn save() -> Result<()> {
    if let Some(operation) = active_git_operation()? {
        anyhow::bail!("Git has a {operation} in progress. Finish or abort it before running `kt`.");
    }

    // -uall lists files inside untracked directories, so the count is honest.
    let status = execute_git(&["status", "--porcelain", "-uall"])?;

    // Committing here would either fail with a wall of git hints or record the
    // conflict markers as if they were real content.
    if has_unmerged_paths(&status) {
        anyhow::bail!(
            "This repository has unresolved merge conflicts. Fix the conflicted files, `git add` them, and finish the merge before saving."
        );
    }

    if status.trim().is_empty() {
        let note = match kite_save_stack()? {
            Some(stack) => format!(
                "nothing new — {} ready to land",
                pluralize(stack.count, "save")
            ),
            None => "nothing to save".to_string(),
        };
        println!("{} {}", "·".dimmed(), note.dimmed());
        return Ok(());
    }

    let has_staged = has_staged_changes(&status);
    let saved_files = if has_staged {
        status.lines().filter(|l| is_staged_status_line(l)).count()
    } else {
        execute_git_quiet(&["add", "-A"])?;
        status.lines().filter(|l| !l.trim().is_empty()).count()
    };

    let msg = format!("{SAVE_PREFIX} {}", Local::now().format("%H:%M:%S"));
    commit_git(&msg, Hooks::Skip)?;

    println!(
        "{} {}",
        "·".dimmed(),
        format!("saved {}", pluralize(saved_files, "file")).dimmed()
    );
    Ok(())
}

fn go(name: &str) -> Result<()> {
    if let Some(operation) = active_git_operation()? {
        anyhow::bail!(
            "Git has a {operation} in progress. Finish or abort it before running `kt go`."
        );
    }

    let local_ref = format!("refs/heads/{name}");
    if check_ref(&local_ref).is_some() {
        execute_git(&["checkout", name])?;
        println!("{} Switched to {}", "✓".green(), name.bold());
        return Ok(());
    }

    let remote_ref = format!("refs/remotes/origin/{name}");
    let has_remote = has_remote();

    // One fetch, covering both questions this command has to answer: does the
    // branch already exist on the remote, and where is the default branch. It
    // replaces the single-branch fetch the create path did anyway, so looking
    // for an existing remote branch costs no extra round trip.
    if has_remote && check_ref(&remote_ref).is_none() {
        let spinner = Spinner::start("Checking origin");
        let _ = execute_git(&["fetch", "origin"]);
        spinner.stop();
    }

    // A branch that exists only on the remote is someone's work in progress —
    // possibly your own from another machine. Branching off the default branch
    // instead would silently create a divergent branch with the same name, and
    // the next `kt publish` would overwrite theirs.
    if has_remote && check_ref(&remote_ref).is_some() {
        execute_git(&["checkout", "--track", &format!("origin/{name}")])?;
        println!(
            "{} Switched to {} {}",
            "✓".green(),
            name.bold(),
            "tracking origin".dimmed()
        );
        return Ok(());
    }

    let default_branch = get_default_branch()?;
    let remote_base = format!("origin/{default_branch}");

    let base = if has_remote {
        match execute_git(&["checkout", "-b", name, &remote_base]) {
            Ok(_) => remote_base,
            Err(_) => {
                execute_git(&["checkout", "-b", name, &default_branch])?;
                default_branch
            }
        }
    } else {
        execute_git(&["checkout", "-b", name, &default_branch])?;
        default_branch
    };

    println!(
        "{} Created {} {}",
        "✓".green(),
        name.bold(),
        format!("from {base}").dimmed()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{TempDir, acquire_cwd_lock, git, init_repo, write_file};

    fn run_save_in_repo(repo: &std::path::Path) -> Result<()> {
        crate::test_support::with_repo_cwd(repo, save)
    }

    fn run_go_in_repo(repo: &std::path::Path, name: &str) -> Result<()> {
        crate::test_support::with_repo_cwd(repo, || go(name))
    }

    fn run_default_in_cwd(path: &std::path::Path) -> Result<Option<String>> {
        crate::test_support::with_repo_cwd(path, || {
            if !is_inside_git_repository()? {
                return Ok(Some(render_help()));
            }

            save()?;
            Ok(None)
        })
    }

    #[test]
    fn save_commits_only_pre_staged_changes() {
        let _lock = acquire_cwd_lock();
        let repo = init_repo();

        write_file(&repo.path, "tracked.txt", "staged change\n");
        write_file(&repo.path, "other.txt", "left unstaged\n");
        git(&repo.path, &["add", "tracked.txt"]);

        run_save_in_repo(&repo.path).expect("save should succeed");

        let saved_files = git(
            &repo.path,
            &["show", "--name-only", "--pretty=format:", "HEAD"],
        );
        assert_eq!(saved_files.trim(), "tracked.txt");

        let status = git(&repo.path, &["status", "--porcelain"]);
        assert!(status.contains(" M other.txt"));
    }

    #[test]
    fn save_stages_everything_when_index_is_empty() {
        let _lock = acquire_cwd_lock();
        let repo = init_repo();

        write_file(&repo.path, "tracked.txt", "modified without staging\n");
        write_file(&repo.path, "new.txt", "brand new file\n");

        run_save_in_repo(&repo.path).expect("save should succeed");

        let saved_files = git(
            &repo.path,
            &["show", "--name-only", "--pretty=format:", "HEAD"],
        );
        assert!(saved_files.lines().any(|line| line == "tracked.txt"));
        assert!(saved_files.lines().any(|line| line == "new.txt"));

        let status = git(&repo.path, &["status", "--porcelain"]);
        assert!(
            status.trim().is_empty(),
            "expected clean status, got: {status}"
        );
    }

    #[test]
    fn save_on_clean_tree_is_a_safe_no_op() {
        let _lock = acquire_cwd_lock();
        let repo = init_repo();
        let head_before = git(&repo.path, &["rev-parse", "HEAD"]);

        run_save_in_repo(&repo.path).expect("clean-tree save should succeed");

        let head_after = git(&repo.path, &["rev-parse", "HEAD"]);
        assert_eq!(head_before, head_after);
    }

    #[test]
    fn go_switches_to_existing_branch_without_recreating_it() {
        let _lock = acquire_cwd_lock();
        let repo = init_repo();
        let default_branch = git(&repo.path, &["rev-parse", "--abbrev-ref", "HEAD"]);
        let default_branch = default_branch.trim();

        git(&repo.path, &["checkout", "-b", "existing-flow"]);
        write_file(&repo.path, "tracked.txt", "existing branch work\n");
        git(&repo.path, &["add", "tracked.txt"]);
        git(&repo.path, &["commit", "-m", "feat: existing flow work"]);
        let existing_head = git(&repo.path, &["rev-parse", "HEAD"]);

        git(&repo.path, &["checkout", default_branch]);

        run_go_in_repo(&repo.path, "existing-flow").expect("go should switch branches");

        let current_branch = git(&repo.path, &["rev-parse", "--abbrev-ref", "HEAD"]);
        let current_head = git(&repo.path, &["rev-parse", "HEAD"]);
        assert_eq!(current_branch.trim(), "existing-flow");
        assert_eq!(current_head, existing_head);
    }

    #[test]
    fn go_checks_out_a_remote_only_branch_instead_of_forking_over_it() {
        let _lock = acquire_cwd_lock();
        let (repo, _remote) = crate::test_support::init_repo_with_remote_branch("teammate-work");

        run_go_in_repo(&repo.path, "teammate-work").expect("go should adopt the remote branch");

        let current_branch = git(&repo.path, &["rev-parse", "--abbrev-ref", "HEAD"]);
        assert_eq!(current_branch.trim(), "teammate-work");

        // The colleague's commit must be on the branch, not orphaned on the
        // remote waiting to be force-pushed away by the next `kt publish`.
        let subjects = git(&repo.path, &["log", "--format=%s"]);
        assert!(
            subjects.lines().any(|line| line == "feat: teammate work"),
            "expected the remote branch's history, got: {subjects}"
        );

        let upstream = git(
            &repo.path,
            &["rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{u}"],
        );
        assert_eq!(upstream.trim(), "origin/teammate-work");
    }

    #[test]
    fn save_refuses_to_snapshot_an_unresolved_merge() {
        let _lock = acquire_cwd_lock();
        let repo = init_repo();

        git(&repo.path, &["checkout", "-q", "-b", "side"]);
        write_file(&repo.path, "tracked.txt", "side change\n");
        git(&repo.path, &["commit", "-qam", "feat: side"]);
        git(&repo.path, &["checkout", "-q", "-"]);
        write_file(&repo.path, "tracked.txt", "main change\n");
        git(&repo.path, &["commit", "-qam", "feat: main"]);

        // Conflicting merge, left unresolved.
        let _ = std::process::Command::new("git")
            .args(["merge", "side"])
            .current_dir(&repo.path)
            .output();

        let err = run_save_in_repo(&repo.path).expect_err("save should refuse a conflicted tree");
        assert!(format!("{err:#}").contains("merge in progress"));
    }

    #[test]
    fn dispatch_blocks_an_incomplete_recovery_marker_before_saving() {
        let _lock = acquire_cwd_lock();
        let repo = init_repo();
        let head_before = git(&repo.path, &["rev-parse", "HEAD"]);
        let branch = git(&repo.path, &["branch", "--show-current"]);
        write_file(&repo.path, "tracked.txt", "must stay uncommitted\n");

        // A pre-atomic interrupted marker with no owner is ambiguous. The
        // default command must stop at dispatch rather than creating another
        // save on top of history it cannot safely classify.
        git(
            &repo.path,
            &["update-ref", "refs/kite/pre_land", head_before.trim()],
        );
        git(
            &repo.path,
            &["config", "--local", "kite.preland.branch", branch.trim()],
        );

        let error = crate::test_support::with_repo_cwd(&repo.path, || run(Cli { command: None }))
            .expect_err("an incomplete marker must stop command dispatch");
        assert!(format!("{error:#}").contains("rollback marker is incomplete"));
        assert_eq!(git(&repo.path, &["rev-parse", "HEAD"]), head_before);
        assert!(
            git(&repo.path, &["status", "--porcelain"]).contains("tracked.txt"),
            "the pending edit was unexpectedly committed"
        );
    }

    #[test]
    fn default_command_shows_help_outside_git_repo() {
        let _lock = acquire_cwd_lock();
        let dir = TempDir::new("kite-test-non-repo");

        let help = run_default_in_cwd(&dir.path)
            .expect("default command should succeed outside a git repo")
            .expect("non-git repo should render help");

        assert!(help.contains("Fast quicksaves and inspectable AI-assisted landing"));
        assert!(help.contains("Usage: kt [COMMAND]"));
        assert!(help.contains("Everyday flow:"));
    }
}
