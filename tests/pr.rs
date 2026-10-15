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
    case "$3" in
      "$EXPECTED_REPO") printf '%s\n' "$FETCH_REPO" ;;
      "$UPSTREAM_URL") printf '%s\n' "$UPSTREAM_REPO" ;;
      *) printf '%s\n' "$PUSH_REPO" ;;
    esac
    exit 0 ;;
  'pr list')
    if [ "$GH_FAILURE" = 'true' ]; then echo 'API unavailable' >&2; exit 1; fi
    case "$*" in *'--state open'*)
      if [ "$GH_CHANGE" = 'switch' ]; then git checkout -b private-work >/dev/null 2>&1; fi
      if [ "$GH_CHANGE" = 'commit' ]; then git commit --allow-empty -m 'concurrent commit' >/dev/null 2>&1; fi
      printf '%s\n' "$GH_PRS" ;;
    esac
    exit 0 ;;
  'pr edit') exit 0 ;;
  'pr create')
    if [ "$GH_CREATE_FAILURE" = 'true' ]; then exit 1; fi
    echo 'https://example.invalid/pull/1'; exit 0 ;;
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
        let mut command = self.pr_command_with_default_base(prs, failure);
        command.args(["--base", base]);
        command
    }

    fn pr_command_with_default_base(&self, prs: &str, failure: bool) -> Command {
        let mut command = self.workflow_command("pr", prs, failure);
        command.arg("--yes");
        command
    }

    fn ship_command(&self, prs: &str) -> Command {
        let mut command = self.workflow_command("ship", prs, false);
        command.args(["--base", &self.base]);
        command
    }

    fn workflow_command(&self, workflow: &str, prs: &str, failure: bool) -> Command {
        let mut paths = vec![self.cli.path.clone()];
        paths.extend(std::env::split_paths(&std::env::var_os("PATH").unwrap()));
        let mut command = Command::new(env!("CARGO_BIN_EXE_kt"));
        command
            .arg(workflow)
            .current_dir(&self.repo.path)
            .env("PATH", std::env::join_paths(paths).unwrap())
            // Keeps the developer's own PR writing skills out of the drafts.
            .env("HOME", &self.cli.path)
            .env("GH_CALLS", self.cli.path.join("calls"))
            .env("GH_FAILURE", failure.to_string())
            .env("GH_PRS", prs)
            .env("EXPECTED_REPO", &self.remote.path)
            .env("FETCH_REPO", repository("owner/repo", None))
            .env("PUSH_REPO", repository("owner/repo", None))
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
        let calls = self.calls();
        assert!(!calls.contains("pr create"), "{calls}");
        assert!(!calls.contains("pr edit"), "{calls}");
    }

    fn calls(&self) -> String {
        std::fs::read_to_string(self.cli.path.join("calls")).unwrap()
    }

    /// Turns `origin` into a fork of a new `upstream` whose base branch has
    /// moved on, with the feature rebuilt on the parent's newer base.
    fn add_upstream(&self) -> TempDir {
        let upstream = TempDir::new("kite-pr-upstream");
        git(&upstream.path, &["init", "--bare"]);
        let repo = &self.repo.path;
        git(
            repo,
            &["remote", "add", "upstream", upstream.path.to_str().unwrap()],
        );
        git(repo, &["checkout", &self.base]);
        git(
            repo,
            &["commit", "--allow-empty", "-m", "feat: upstream change"],
        );
        git(repo, &["push", "upstream", &self.base]);
        git(repo, &["reset", "--hard", "HEAD~"]);
        git(repo, &["fetch", "upstream"]);
        git(
            repo,
            &["rebase", &format!("upstream/{}", self.base), "feature"],
        );
        upstream
    }

    fn fork_pr_command(&self, upstream: &TempDir, prs: &str) -> Command {
        let mut command = self.pr_command(&self.base, prs, false);
        command
            .env("FETCH_REPO", repository("me/repo", Some("acme/repo")))
            .env("UPSTREAM_URL", &upstream.path)
            .env("UPSTREAM_REPO", repository("acme/repo", None));
        command
    }
}

/// `gh repo view --json url,nameWithOwner,parent` for `owner/name`.
fn repository(name_with_owner: &str, parent: Option<&str>) -> String {
    let parent = parent.map(|parent| {
        let (owner, name) = parent.split_once('/').unwrap();
        serde_json::json!({ "name": name, "owner": { "login": owner } })
    });
    serde_json::json!({
        "url": format!("https://example.invalid/{name_with_owner}"),
        "nameWithOwner": name_with_owner,
        "parent": parent,
    })
    .to_string()
}

/// An open pull request as `gh pr list` reports it, from the `head` repository.
fn open_pr(number: u32, title: &str, base: &str, head: &str) -> serde_json::Value {
    let (owner, name) = head.split_once('/').unwrap();
    serde_json::json!({
        "url": format!("https://example.invalid/pull/{number}"),
        "title": title,
        "body": format!("{title} notes"),
        "baseRefName": base,
        "headRepository": { "name": name },
        "headRepositoryOwner": { "login": owner },
    })
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
    let existing = serde_json::json!([open_pr(1, "Existing", "release", "owner/repo")]);
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
    let fork = serde_json::json!([open_pr(99, "Their work", &repo.base, "them/repo")]);
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
        open_pr(99, "Their work", &repo.base, "them/repo"),
        open_pr(1, "Our work", &repo.base, "owner/repo"),
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
        .env("PUSH_REPO", repository("elsewhere/repo", None))
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
fn fork_pr_targets_upstream_from_the_fork_against_the_parent_base() {
    let repo = PrRepo::new();
    let upstream = repo.add_upstream();

    let output = repo.fork_pr_command(&upstream, "[]").output().unwrap();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stdout)
            .contains(&format!("feature → acme/repo:{}", repo.base)),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    // Published to the fork, never to the parent.
    assert_eq!(
        git(&repo.repo.path, &["rev-parse", "HEAD"]),
        git(&repo.remote.path, &["rev-parse", "feature"])
    );
    assert!(git(&upstream.path, &["branch", "--list", "feature"]).is_empty());

    let calls = repo.calls();
    assert!(calls.contains("pr create --head me:feature"), "{calls}");
    assert!(
        !calls.contains(&format!("--repo {}", repo.remote.path.display())),
        "{calls}"
    );
    assert_eq!(
        calls
            .matches(&format!("--repo {}", upstream.path.display()))
            .count(),
        3,
        "{calls}"
    );
    // Measured against the parent's base, not the fork's stale copy of it.
    assert!(calls.contains("- feat: add feature"), "{calls}");
    assert!(!calls.contains("upstream change"), "{calls}");
}

#[test]
fn fork_pr_defaults_to_a_freshly_added_parents_own_default_branch() {
    let repo = PrRepo::new();
    let upstream = TempDir::new("kite-pr-upstream");
    git(&upstream.path, &["init", "--bare"]);
    let path = &repo.repo.path;
    // The parent's default is `release`, not the fork's, and the remote was
    // just added, so no tracking refs exist yet.
    git(
        path,
        &[
            "push",
            upstream.path.to_str().unwrap(),
            "HEAD~:refs/heads/release",
        ],
    );
    git(
        &upstream.path,
        &["symbolic-ref", "HEAD", "refs/heads/release"],
    );
    git(
        path,
        &["remote", "add", "upstream", upstream.path.to_str().unwrap()],
    );

    let output = repo
        .pr_command_with_default_base("[]", false)
        .env("FETCH_REPO", repository("me/repo", Some("acme/repo")))
        .env("UPSTREAM_URL", &upstream.path)
        .env("UPSTREAM_REPO", repository("acme/repo", None))
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let calls = repo.calls();
    assert!(calls.contains("pr create --head me:feature"), "{calls}");
    assert!(calls.contains("--base release"), "{calls}");
}

#[test]
fn fork_refresh_selects_only_the_pull_request_from_origin() {
    let repo = PrRepo::new();
    let upstream = repo.add_upstream();
    let prs = serde_json::json!([
        open_pr(1, "Maintainer work", &repo.base, "acme/repo"),
        open_pr(2, "Their work", &repo.base, "them/repo"),
        open_pr(3, "Our work", &repo.base, "ME/repo"),
    ]);

    let output = repo
        .fork_pr_command(&upstream, &prs.to_string())
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("Already open: https://example.invalid/pull/3"),
        "{stdout}"
    );
    let calls = repo.calls();
    assert!(!calls.contains("pr create"), "{calls}");
    assert!(!calls.contains("pr edit"), "{calls}");
}

#[test]
fn github_fork_without_a_base_remote_asks_where_the_pr_belongs() {
    let repo = PrRepo::new();
    let output = repo
        .pr_command(&repo.base, "[]", false)
        .env("FETCH_REPO", repository("me/repo", Some("acme/repo")))
        .output()
        .unwrap();

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("fork of `acme/repo`"), "{stderr}");
    assert!(
        stderr.contains("git remote add upstream https://example.invalid/acme/repo"),
        "{stderr}"
    );
    assert!(stderr.contains("gh repo set-default origin"), "{stderr}");
    repo.assert_unpublished();

    // `gh repo set-default origin` keeps pull requests inside the fork.
    git(
        &repo.repo.path,
        &["config", "remote.origin.gh-resolved", "base"],
    );
    let output = repo
        .pr_command(&repo.base, "[]", false)
        .env("FETCH_REPO", repository("me/repo", Some("acme/repo")))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let calls = repo.calls();
    assert!(calls.contains("pr create --head feature"), "{calls}");
}

#[test]
fn gh_default_repository_overrides_the_upstream_convention() {
    let repo = PrRepo::new();
    let upstream = repo.add_upstream();
    git(
        &repo.repo.path,
        &["config", "remote.origin.gh-resolved", "base"],
    );

    let output = repo.fork_pr_command(&upstream, "[]").output().unwrap();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let calls = repo.calls();
    assert!(calls.contains("pr create --head feature"), "{calls}");
    assert!(
        !calls.contains(&format!("--repo {}", upstream.path.display())),
        "{calls}"
    );
}

#[test]
fn commits_made_during_ai_drafting_stop_pr_creation() {
    let repo = PrRepo::new();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    listener.set_nonblocking(true).unwrap();
    let path = repo.repo.path.clone();
    let server = std::thread::spawn(move || {
        let mut stream = accept_model_request(&listener);
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

fn accept_model_request(listener: &TcpListener) -> std::net::TcpStream {
    let deadline = Instant::now() + Duration::from_secs(10);
    let stream = loop {
        match listener.accept() {
            Ok((stream, _)) => break stream,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                assert!(Instant::now() < deadline, "CLI never requested AI output");
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(error) => panic!("{error}"),
        }
    };
    stream.set_nonblocking(false).unwrap();
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
    stream
}

/// A bounded local Responses endpoint, used for the land and PR in one CLI run.
fn model_server(replies: Vec<serde_json::Value>) -> (String, std::thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    listener.set_nonblocking(true).unwrap();
    let server = std::thread::spawn(move || {
        for reply in replies {
            let mut stream = accept_model_request(&listener);
            let body = serde_json::json!({"output_text": reply.to_string()}).to_string();
            write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
        }
    });
    (url, server)
}

fn ship_plan() -> serde_json::Value {
    serde_json::json!({"groups": [{"message": "feat: ship latest edits", "files": ["tracked.txt", "new.txt"]}]})
}

fn ship_draft() -> serde_json::Value {
    serde_json::json!({"title": "feat: ship latest edits", "body": "Adds the latest edits."})
}

fn edit_for_ship(repo: &PrRepo) {
    write_file(&repo.repo.path, "tracked.txt", "latest edits\n");
    write_file(&repo.repo.path, "new.txt", "new file\n");
}

fn hook(repo: &PrRepo, name: &str, body: &str) {
    let file = format!(".git/hooks/{name}");
    write_file(&repo.repo.path, &file, &format!("#!/bin/sh\n{body}\n"));
    std::fs::set_permissions(
        repo.repo.path.join(file),
        std::fs::Permissions::from_mode(0o755),
    )
    .unwrap();
}

#[test]
fn ship_saves_lands_publishes_once_and_creates_pr_without_prompts() {
    let repo = PrRepo::new();
    edit_for_ship(&repo);
    hook(&repo, "pre-push", "echo pushed >> .git/push-count");
    hook(&repo, "pre-commit", "echo hooked >> .git/commit-count");
    let (url, server) = model_server(vec![ship_plan(), ship_draft()]);
    let output = repo
        .workflow_command("ship", "[]", false)
        .args(["--draft", "--tag", "PROJ-123"])
        .env("KITE_OPENAI_URL", url)
        .env("KITE_OPENAI_API_KEY", "local-test-key")
        .output()
        .unwrap();
    server.join().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(git(&repo.repo.path, &["status", "--porcelain"]).is_empty());
    assert_eq!(
        git(&repo.repo.path, &["log", "-1", "--format=%s"]).trim(),
        "feat: ship latest edits [PROJ-123]"
    );
    let save = git(&repo.repo.path, &["rev-parse", "refs/kite/pre_land"]);
    assert!(
        git(&repo.repo.path, &["log", "-1", "--format=%s", save.trim()]).starts_with("[kite] save")
    );
    assert_eq!(
        git(&repo.repo.path, &["rev-parse", "HEAD^{tree}"]),
        git(
            &repo.repo.path,
            &["rev-parse", &format!("{}^{{tree}}", save.trim())]
        )
    );
    assert_eq!(
        git(&repo.repo.path, &["rev-parse", "HEAD"]),
        git(&repo.remote.path, &["rev-parse", "feature"])
    );
    assert_eq!(
        std::fs::read_to_string(repo.repo.path.join(".git/push-count")).unwrap(),
        "pushed\n"
    );
    assert_eq!(
        std::fs::read_to_string(repo.repo.path.join(".git/commit-count")).unwrap(),
        "hooked\n"
    );
    let calls = repo.calls();
    assert!(calls.contains("pr create --head feature"), "{calls}");
    assert!(calls.contains("--draft"), "{calls}");
    assert!(calls.contains("Adds the latest edits."), "{calls}");
}

#[test]
fn ship_refreshes_an_existing_pr_with_no_new_saves() {
    let repo = PrRepo::new();
    let before = git(&repo.repo.path, &["rev-parse", "HEAD"]);
    let prs = serde_json::json!([open_pr(1, "Old title", &repo.base, "owner/repo")]);
    let (url, server) = model_server(vec![ship_draft()]);
    let output = repo
        .workflow_command("ship", &prs.to_string(), false)
        .env("KITE_OPENAI_URL", url)
        .env("KITE_OPENAI_API_KEY", "local-test-key")
        .output()
        .unwrap();
    server.join().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(git(&repo.repo.path, &["rev-parse", "HEAD"]), before);
    assert_eq!(git(&repo.remote.path, &["rev-parse", "feature"]), before);
    let calls = repo.calls();
    assert!(
        calls.contains("pr edit https://example.invalid/pull/1"),
        "{calls}"
    );
    assert!(!calls.contains("pr create"), "{calls}");
}

#[test]
fn ship_rejects_invalid_destination_before_saving() {
    for base in ["missing", "feature"] {
        let repo = PrRepo::new();
        edit_for_ship(&repo);
        let before = git(&repo.repo.path, &["rev-parse", "HEAD"]);
        let status = git(&repo.repo.path, &["status", "--porcelain"]);
        let output = repo
            .workflow_command("ship", "[]", false)
            .args(["--base", base])
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert_eq!(git(&repo.repo.path, &["rev-parse", "HEAD"]), before);
        assert_eq!(git(&repo.repo.path, &["status", "--porcelain"]), status);
        repo.assert_unpublished();
    }
}

#[test]
fn ship_stops_when_ai_cannot_land_and_keeps_the_save() {
    let repo = PrRepo::new();
    edit_for_ship(&repo);
    let output = repo.ship_command("[]").output().unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("Rerun `kt land` without --yes"));
    assert!(git(&repo.repo.path, &["log", "-1", "--format=%s"]).starts_with("[kite] save"));
    assert!(git(&repo.repo.path, &["status", "--porcelain"]).is_empty());
    repo.assert_unpublished();
}

#[test]
fn ship_preserves_the_staged_selection_and_stops_with_unsaved_work() {
    let repo = PrRepo::new();
    edit_for_ship(&repo);
    git(&repo.repo.path, &["add", "tracked.txt"]);
    let output = repo.ship_command("[]").output().unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("Working directory must be clean"));
    assert_eq!(
        git(
            &repo.repo.path,
            &["show", "--format=", "--name-only", "HEAD"]
        )
        .trim(),
        "tracked.txt"
    );
    assert_eq!(
        git(&repo.repo.path, &["status", "--porcelain"]).trim(),
        "?? new.txt"
    );
    repo.assert_unpublished();
}

#[test]
fn ship_stops_on_failed_commit_or_push_hook() {
    for name in ["pre-commit", "pre-push"] {
        let repo = PrRepo::new();
        edit_for_ship(&repo);
        hook(&repo, name, "echo 'hook blocked shipping' >&2; exit 1");
        let (url, server) = model_server(vec![ship_plan()]);
        let output = repo
            .ship_command("[]")
            .env("KITE_OPENAI_URL", url)
            .env("KITE_OPENAI_API_KEY", "local-test-key")
            .output()
            .unwrap();
        server.join().unwrap();
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("hook blocked shipping"));
        let subject = git(&repo.repo.path, &["log", "-1", "--format=%s"]);
        assert_eq!(subject.starts_with("[kite] save"), name == "pre-commit");
        assert_eq!(
            std::fs::read_to_string(repo.repo.path.join("new.txt")).unwrap(),
            "new file\n"
        );
        repo.assert_unpublished();
    }
}

#[test]
fn ship_can_retry_after_pr_creation_fails() {
    let repo = PrRepo::new();
    edit_for_ship(&repo);
    let (url, server) = model_server(vec![ship_plan(), ship_draft()]);
    let output = repo
        .ship_command("[]")
        .env("GH_CREATE_FAILURE", "true")
        .env("KITE_OPENAI_URL", url)
        .env("KITE_OPENAI_API_KEY", "local-test-key")
        .output()
        .unwrap();
    server.join().unwrap();
    assert!(!output.status.success());
    let head = git(&repo.repo.path, &["rev-parse", "HEAD"]);
    assert_eq!(git(&repo.remote.path, &["rev-parse", "feature"]), head);
    let output = repo.ship_command("[]").output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(git(&repo.repo.path, &["rev-parse", "HEAD"]), head);
    assert_eq!(repo.calls().matches("pr create").count(), 2);
}

#[test]
fn ship_refuses_detached_head_or_missing_origin_before_saving() {
    for detached in [true, false] {
        let repo = PrRepo::new();
        if detached {
            git(&repo.repo.path, &["checkout", "--detach"]);
        } else {
            git(&repo.repo.path, &["remote", "remove", "origin"]);
        }
        edit_for_ship(&repo);
        let before = git(&repo.repo.path, &["rev-parse", "HEAD"]);
        let status = git(&repo.repo.path, &["status", "--porcelain"]);
        let output = repo.ship_command("[]").output().unwrap();
        assert!(!output.status.success());
        assert_eq!(git(&repo.repo.path, &["rev-parse", "HEAD"]), before);
        assert_eq!(git(&repo.repo.path, &["status", "--porcelain"]), status);
        assert!(!repo.cli.path.join("calls").exists());
    }
}

#[test]
fn ship_to_parent_publishes_only_to_fork_and_can_skip_commit_hooks() {
    let repo = PrRepo::new();
    let upstream = repo.add_upstream();
    edit_for_ship(&repo);
    hook(&repo, "pre-commit", "exit 1");
    hook(&repo, "pre-push", "echo pushed >> .git/push-count");
    let (url, server) = model_server(vec![ship_plan(), ship_draft()]);
    let output = repo
        .ship_command("[]")
        .arg("--no-verify")
        .env("FETCH_REPO", repository("me/repo", Some("acme/repo")))
        .env("UPSTREAM_URL", &upstream.path)
        .env("UPSTREAM_REPO", repository("acme/repo", None))
        .env("KITE_OPENAI_URL", url)
        .env("KITE_OPENAI_API_KEY", "local-test-key")
        .output()
        .unwrap();
    server.join().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        git(&repo.repo.path, &["rev-parse", "HEAD"]),
        git(&repo.remote.path, &["rev-parse", "feature"])
    );
    assert!(git(&upstream.path, &["branch", "--list", "feature"]).is_empty());
    assert_eq!(
        std::fs::read_to_string(repo.repo.path.join(".git/push-count")).unwrap(),
        "pushed\n"
    );
    assert!(repo.calls().contains("pr create --head me:feature"));
}
