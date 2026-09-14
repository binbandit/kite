#![cfg(unix)]

#[path = "../src/test_support.rs"]
#[allow(dead_code)]
mod support;

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::process::{Command, Output};
use std::time::{Duration, Instant};
use support::{TempDir, git, init_repo, write_file};

struct PrRepo {
    repo: TempDir,
    remote: TempDir,
    cli: TempDir,
    base: String,
}

impl PrRepo {
    fn new() -> Self {
        let repo = init_repo();
        let remote = TempDir::new("kite-pr-remote");
        let cli = TempDir::new("kite-pr-gh");
        git(&remote.path, &["init", "--bare"]);
        git(
            &repo.path,
            &["remote", "add", "origin", remote.path.to_str().unwrap()],
        );
        let base = git(&repo.path, &["branch", "--show-current"])
            .trim()
            .to_string();
        git(&repo.path, &["push", "origin", &base]);
        git(&repo.path, &["checkout", "-b", "feature"]);
        write_file(&repo.path, "feature.txt", "feature\n");
        git(&repo.path, &["add", "feature.txt"]);
        git(&repo.path, &["commit", "-m", "feat: add feature"]);
        write_file(
            &cli.path,
            "gh",
            r#"#!/bin/sh
printf '%s\n' "$*" >> "$GH_CALLS"
case "$1 $2" in
  'repo view')
    if [ "$3" = "$EXPECTED_REPO" ]; then echo "$FETCH_REPO"; else echo "$PUSH_REPO"; fi
    exit 0 ;;
  'pr list')
    if [ "$GH_FAILURE" = 'true' ]; then echo 'API unavailable' >&2; exit 1; fi
    case "$*" in *'--state open'*)
      if [ "$GH_CHANGE" = 'switch' ]; then git checkout -b private-work >/dev/null 2>&1; fi
      if [ "$GH_CHANGE" = 'commit' ]; then git commit --allow-empty -m 'concurrent commit' >/dev/null 2>&1; fi
      printf '%s\n' "$GH_PRS" ;;
    esac
    exit 0 ;;
  'pr create') echo 'https://example.invalid/pull/1'; exit 0 ;;
esac
echo 'unexpected gh command' >&2
exit 1
"#,
        );
        std::fs::set_permissions(cli.path.join("gh"), std::fs::Permissions::from_mode(0o755))
            .unwrap();
        Self {
            repo,
            remote,
            cli,
            base,
        }
    }

    fn pr_command(&self, base: &str, prs: &str, failure: bool) -> Command {
        let mut paths = vec![self.cli.path.clone()];
        paths.extend(std::env::split_paths(&std::env::var_os("PATH").unwrap()));
        let mut command = Command::new(env!("CARGO_BIN_EXE_kt"));
        command
            .args(["pr", "--yes", "--base", base])
            .current_dir(&self.repo.path)
            .env("PATH", std::env::join_paths(paths).unwrap())
            .env("GH_CALLS", self.cli.path.join("calls"))
            .env("GH_FAILURE", failure.to_string())
            .env("GH_PRS", prs)
            .env("EXPECTED_REPO", &self.remote.path)
            .env("FETCH_REPO", "https://example.invalid/owner/repo")
            .env("PUSH_REPO", "https://example.invalid/owner/repo")
            .env("KITE_OPENAI_URL", "http://127.0.0.1:1")
            .env("KITE_OPENAI_TIMEOUT_SECS", "1");
        for key in [
            "KITE_OPENAI_API_KEY",
            "OPENAI_API_KEY",
            "KITE_API_KEY",
            "OPENAI_KEY",
            "AI_GATEWAY_API_KEY",
        ] {
            command.env_remove(key);
        }
        command
    }

    fn pr(&self, base: &str, prs: &str, failure: bool) -> Output {
        self.pr_command(base, prs, failure).output().unwrap()
    }

    fn assert_unpublished(&self) {
        assert!(git(&self.remote.path, &["branch", "--list", "feature"]).is_empty());
        let calls = std::fs::read_to_string(self.cli.path.join("calls")).unwrap();
        assert!(!calls.contains("pr create"), "{calls}");
        assert!(!calls.contains("pr edit"), "{calls}");
    }
}

#[test]
fn invalid_base_does_not_publish_branch() {
    let repo = PrRepo::new();
    let output = repo.pr("missing", "[]", false);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("Could not fetch base branch"));
    repo.assert_unpublished();
}

#[test]
fn failed_pr_lookup_does_not_publish_or_create() {
    let repo = PrRepo::new();
    let output = repo.pr(&repo.base, "[]", true);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("API unavailable"));
    repo.assert_unpublished();
}

#[test]
fn malformed_pr_lookup_does_not_publish_or_create() {
    let repo = PrRepo::new();
    let output = repo.pr(&repo.base, "not json", false);
    assert!(!output.status.success());
    repo.assert_unpublished();
}

#[test]
fn creates_pr_with_explicit_head_after_validating_base() {
    let repo = PrRepo::new();
    let output = repo.pr(&repo.base, "[]", false);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        git(&repo.repo.path, &["rev-parse", "HEAD"]),
        git(&repo.remote.path, &["rev-parse", "feature"])
    );
    let calls = std::fs::read_to_string(repo.cli.path.join("calls")).unwrap();
    assert!(calls.contains("pr create --head feature"), "{calls}");
}

#[test]
fn explicit_base_mismatch_does_not_silently_refresh_another_base() {
    let repo = PrRepo::new();
    let existing = serde_json::json!([{
        "url": "https://example.invalid/pull/1", "title": "Existing", "body": "Notes", "baseRefName": "release", "isCrossRepository": false
    }]);
    let output = repo.pr(&repo.base, &existing.to_string(), false);
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("existing pull request targets `release`")
    );
    repo.assert_unpublished();
}

#[test]
fn deleted_remote_branch_is_republished_despite_stale_tracking_ref() {
    let repo = PrRepo::new();
    git(&repo.repo.path, &["push", "origin", "feature"]);
    // Delete directly at the remote, as another clone or GitHub would do.
    git(
        &repo.remote.path,
        &["update-ref", "-d", "refs/heads/feature"],
    );
    let output = repo.pr(&repo.base, "[]", false);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!git(&repo.remote.path, &["branch", "--list", "feature"]).is_empty());
}

#[test]
fn buried_saves_are_rejected_before_publishing() {
    let repo = PrRepo::new();
    git(
        &repo.repo.path,
        &["commit", "--allow-empty", "-m", "[kite] save 12:00:00"],
    );
    git(
        &repo.repo.path,
        &["commit", "--allow-empty", "-m", "docs: explanation"],
    );
    let output = repo.pr(&repo.base, "[]", false);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("unlanded save"));
    repo.assert_unpublished();
}

#[test]
fn branch_or_commit_changes_during_lookup_stop_before_publishing() {
    for change in ["switch", "commit"] {
        let repo = PrRepo::new();
        let output = repo
            .pr_command(&repo.base, "[]", false)
            .env("GH_CHANGE", change)
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("branch or HEAD changed"));
        repo.assert_unpublished();
        assert!(git(&repo.remote.path, &["branch", "--list", "private-work"]).is_empty());
    }
}

#[test]
fn same_name_fork_is_not_selected_for_refresh() {
    let repo = PrRepo::new();
    let fork = serde_json::json!([{
        "url": "https://example.invalid/pull/99", "title": "Their work", "body": "Their notes",
        "baseRefName": repo.base, "isCrossRepository": true
    }]);
    let output = repo.pr(&repo.base, &fork.to_string(), false);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let calls = std::fs::read_to_string(repo.cli.path.join("calls")).unwrap();
    assert!(calls.contains("pr create --head feature"), "{calls}");
    assert!(!calls.contains("pr edit"), "{calls}");
    assert_eq!(
        calls
            .matches(&format!("--repo {}", repo.remote.path.display()))
            .count(),
        3,
        "{calls}"
    );
}

#[test]
fn existing_origin_pr_survives_unavailable_ai_and_same_name_forks() {
    let repo = PrRepo::new();
    let prs = serde_json::json!([
        { "url": "https://example.invalid/pull/99", "title": "Their work", "body": "Their notes",
          "baseRefName": repo.base, "isCrossRepository": true },
        { "url": "https://example.invalid/pull/1", "title": "Our work", "body": "Preserve these notes",
          "baseRefName": repo.base, "isCrossRepository": false }
    ]);
    let output = repo.pr(&repo.base, &prs.to_string(), false);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("Already open: https://example.invalid/pull/1"),
        "{stdout}"
    );
    let calls = std::fs::read_to_string(repo.cli.path.join("calls")).unwrap();
    assert!(!calls.contains("pr create"), "{calls}");
    assert!(!calls.contains("pr edit"), "{calls}");
}

#[test]
fn different_fetch_and_push_repositories_are_rejected() {
    let repo = PrRepo::new();
    git(
        &repo.repo.path,
        &[
            "remote",
            "set-url",
            "--push",
            "origin",
            "git@example.invalid:elsewhere/repo.git",
        ],
    );
    let output = repo
        .pr_command(&repo.base, "[]", false)
        .env("PUSH_REPO", "https://example.invalid/elsewhere/repo")
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("same GitHub repository"));
    repo.assert_unpublished();
}

#[test]
fn equivalent_fetch_and_push_urls_are_accepted() {
    let repo = PrRepo::new();
    // Different spellings of the same local path exercise the canonical GitHub
    // identity check without letting a test push reach a real remote.
    let push_url = format!("{}/.", repo.remote.path.display());
    git(
        &repo.repo.path,
        &["remote", "set-url", "--push", "origin", &push_url],
    );
    let output = repo.pr(&repo.base, "[]", false);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!git(&repo.remote.path, &["branch", "--list", "feature"]).is_empty());
}

#[test]
fn commits_made_during_ai_drafting_stop_pr_creation() {
    let repo = PrRepo::new();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    listener.set_nonblocking(true).unwrap();
    let path = repo.repo.path.clone();
    let server = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(10);
        let (mut stream, _) = loop {
            match listener.accept() {
                Ok(connection) => break connection,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(Instant::now() < deadline, "CLI never requested a draft");
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(error) => panic!("{error}"),
            }
        };
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let mut reader = BufReader::new(&stream);
        let mut length = 0;
        loop {
            let mut line = String::new();
            assert_ne!(reader.read_line(&mut line).unwrap(), 0);
            if line == "\r\n" {
                break;
            }
            if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                length = value.trim().parse().unwrap();
            }
        }
        reader.read_exact(&mut vec![0; length]).unwrap();
        git(
            &path,
            &[
                "commit",
                "--allow-empty",
                "-m",
                "feat: added while drafting",
            ],
        );
        let body = serde_json::json!({"output_text": serde_json::json!({
            "title": "feat: initial feature", "body": "Describes the original commit."
        }).to_string()})
        .to_string();
        write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
    });
    let output = repo
        .pr_command(&repo.base, "[]", false)
        .env("KITE_OPENAI_URL", format!("http://{address}"))
        .env("KITE_OPENAI_API_KEY", "local-test-key")
        .env("KITE_OPENAI_TIMEOUT_SECS", "10")
        .output()
        .unwrap();
    server.join().unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("branch or HEAD changed"));
    let calls = std::fs::read_to_string(repo.cli.path.join("calls")).unwrap();
    assert!(!calls.contains("pr create"), "{calls}");
    assert!(!calls.contains("pr edit"), "{calls}");
    assert_ne!(
        git(&repo.repo.path, &["rev-parse", "HEAD"]),
        git(&repo.remote.path, &["rev-parse", "feature"])
    );
}
