mod git;
mod land;
mod synth;

#[cfg(test)]
mod test_support;

use anyhow::Result;
use chrono::Local;
use clap::{Parser, Subcommand};
use colored::*;

use crate::git::{
    execute_git, execute_git_quiet, get_default_branch, has_remote, has_staged_changes,
};
use crate::land::{land, publish_current_branch, undo};

#[derive(Parser)]
#[command(
    name = "kt",
    about = "Fast quicksaves and inspectable AI-assisted landing",
    version
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// Start a new flow
    Go { name: String },
    /// Intelligently chunk Kite saves into local commits
    Land {
        /// Publish the rewritten branch after landing
        #[arg(long)]
        push: bool,
        /// Skip the confirmation prompt
        #[arg(short = 'y', long)]
        yes: bool,
    },
    /// Publish the current branch after reviewing local history
    Publish,
    /// Instantly revert the last land operation
    Undo,
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    match &cli.command {
        Some(Commands::Go { name }) => go(name),
        Some(Commands::Land { push, yes }) => run_land(*push, *yes),
        Some(Commands::Publish) => publish_current_branch(),
        Some(Commands::Undo) => undo(),
        None => save(),
    }
}

fn run_land(push: bool, auto_confirm: bool) -> Result<()> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(land(push, auto_confirm))
}

fn save() -> Result<()> {
    let status = execute_git(&["status", "--porcelain"])?;
    if status.trim().is_empty() {
        return Ok(());
    }

    if !has_staged_changes(&status) {
        execute_git_quiet(&["add", "-A"])?;
    }

    let msg = format!("[kite] save {}", Local::now().format("%H:%M:%S"));
    execute_git_quiet(&["commit", "-m", &msg, "--no-verify"])?;

    println!("{} {}", "·".dimmed(), "saved".dimmed());
    Ok(())
}

fn go(name: &str) -> Result<()> {
    let default_branch = get_default_branch()?;

    if has_remote() {
        let _ = execute_git(&["fetch", "origin", &default_branch]);
        execute_git(&[
            "checkout",
            "-b",
            name,
            &format!("origin/{}", default_branch),
        ])
        .or_else(|_| execute_git(&["checkout", "-b", name, &default_branch]))?;
    } else {
        execute_git(&["checkout", "-b", name, &default_branch])?;
    }

    println!("{} Flow started: {}", "·".cyan(), name.bold());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{acquire_cwd_lock, git, init_repo, write_file};

    fn run_save_in_repo(repo: &std::path::Path) -> Result<()> {
        crate::test_support::with_repo_cwd(repo, save)
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
}
