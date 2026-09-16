#[path = "../src/test_support.rs"]
#[allow(dead_code)]
mod support;

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::process::{Command, Output};
use std::time::{Duration, Instant};
use support::{TempDir, git, init_repo, write_file};

fn kt(repo: &Path, args: &[&str]) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_kt"));
    command.args(args).current_dir(repo);
    command
}

fn save(repo: &Path) {
    write_file(repo, "tracked.txt", "saved\n");
    git(repo, &["add", "tracked.txt"]);
    git(repo, &["commit", "-qm", "[kite] save 12:00:00"]);
}

fn read_plan_request(listener: &TcpListener) -> TcpStream {
    listener.set_nonblocking(true).unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    let (stream, _) = loop {
        match listener.accept() {
            Ok(connection) => break connection,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                assert!(Instant::now() < deadline, "CLI never requested a plan");
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

fn send_plan(mut stream: TcpStream) {
    let body = serde_json::json!({"output_text": serde_json::json!({"groups": [
        {"message": "feat: landed", "files": ["tracked.txt"]}
    ]}).to_string()})
    .to_string();
    write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
}

// Mutate the repository while the actual CLI is waiting for its AI response.
fn land(repo: &Path, dirty: bool, during_request: impl FnOnce() + Send + 'static) -> Output {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let server = std::thread::spawn(move || {
        let stream = read_plan_request(&listener);
        during_request();
        send_plan(stream);
    });
    let mut command = kt(repo, &["land", "--yes"]);
    if dirty {
        command.arg("--allow-dirty");
    }
    let output = command
        .env("KITE_OPENAI_URL", format!("http://{address}"))
        .env("KITE_OPENAI_API_KEY", "local-test-key")
        .env("KITE_OPENAI_TIMEOUT_SECS", "10")
        .output()
        .unwrap();
    server.join().unwrap();
    output
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn undoing_the_root_save_does_not_leave_a_stale_staged_snapshot() {
    let repo = TempDir::new("kite-root-save");
    git(&repo.path, &["init", "-q"]);
    git(&repo.path, &["config", "user.name", "Kite Test"]);
    git(&repo.path, &["config", "user.email", "kite@example.com"]);
    write_file(&repo.path, "first.txt", "first snapshot\n");
    assert_success(&kt(&repo.path, &[]).output().unwrap());

    assert_success(&kt(&repo.path, &["undo"]).output().unwrap());
    assert_eq!(git(&repo.path, &["ls-files", "--stage"]), "");
    write_file(&repo.path, "first.txt", "latest work\n");
    write_file(&repo.path, "second.txt", "new file\n");
    assert_success(&kt(&repo.path, &[]).output().unwrap());

    assert_eq!(
        git(&repo.path, &["show", "HEAD:first.txt"]),
        "latest work\n"
    );
    assert_eq!(git(&repo.path, &["show", "HEAD:second.txt"]), "new file\n");
    assert_eq!(git(&repo.path, &["status", "--porcelain"]), "");
}

#[test]
fn cancelled_saves_land_without_ai_and_undo_restores_them() {
    let repo = init_repo();
    let base = git(&repo.path, &["rev-parse", "HEAD"]);
    write_file(&repo.path, "tracked.txt", "temporary work\n");
    assert_success(&kt(&repo.path, &[]).output().unwrap());
    write_file(&repo.path, "tracked.txt", "base\n");
    assert_success(&kt(&repo.path, &[]).output().unwrap());
    let saves = git(&repo.path, &["rev-parse", "HEAD"]);
    let branch = git(&repo.path, &["symbolic-ref", "HEAD"]);

    // This endpoint never responds: a net-zero plan must not need an AI call.
    let unavailable_ai = TcpListener::bind("127.0.0.1:0").unwrap();
    let output = kt(&repo.path, &["land", "--yes"])
        .env(
            "KITE_OPENAI_URL",
            format!("http://{}", unavailable_ai.local_addr().unwrap()),
        )
        .env("KITE_OPENAI_API_KEY", "local-test-key")
        .env("KITE_OPENAI_TIMEOUT_SECS", "1")
        .output()
        .unwrap();
    assert_success(&output);
    assert_eq!(git(&repo.path, &["rev-parse", "HEAD"]), base);
    assert_eq!(git(&repo.path, &["symbolic-ref", "HEAD"]), branch);
    assert_eq!(git(&repo.path, &["status", "--porcelain"]), "");

    assert_success(&kt(&repo.path, &["undo"]).output().unwrap());
    assert_eq!(git(&repo.path, &["rev-parse", "HEAD"]), saves);
    assert_eq!(git(&repo.path, &["status", "--porcelain"]), "");
}

#[test]
fn cancelled_root_saves_land_as_one_empty_commit_and_remain_undoable() {
    let repo = support::init_root_kite_repo();
    std::fs::remove_file(repo.path.join("tracked.txt")).unwrap();
    assert_success(&kt(&repo.path, &[]).output().unwrap());
    let saves = git(&repo.path, &["rev-parse", "HEAD"]);
    let branch = git(&repo.path, &["symbolic-ref", "HEAD"]);

    let unavailable_ai = TcpListener::bind("127.0.0.1:0").unwrap();
    let output = kt(&repo.path, &["land", "--yes", "--tag", "PROJ-123"])
        .env(
            "KITE_OPENAI_URL",
            format!("http://{}", unavailable_ai.local_addr().unwrap()),
        )
        .env("KITE_OPENAI_API_KEY", "local-test-key")
        .env("KITE_OPENAI_TIMEOUT_SECS", "1")
        .output()
        .unwrap();
    assert_success(&output);

    assert_eq!(
        git(&repo.path, &["rev-list", "--count", "HEAD"]).trim(),
        "1"
    );
    let message = git(&repo.path, &["log", "-1", "--format=%s"]);
    assert_eq!(message.trim(), "chore: empty initial snapshot [PROJ-123]");
    assert_eq!(git(&repo.path, &["ls-tree", "--name-only", "HEAD"]), "");
    assert_eq!(git(&repo.path, &["symbolic-ref", "HEAD"]), branch);

    assert_success(&kt(&repo.path, &["undo"]).output().unwrap());
    assert_eq!(git(&repo.path, &["rev-parse", "HEAD"]), saves);
    assert_eq!(git(&repo.path, &["status", "--porcelain"]), "");
}

#[cfg(unix)]
fn interrupted_dirty_land_recovers(in_hook: bool) {
    use std::os::unix::{fs::PermissionsExt, process::CommandExt};
    use std::process::Stdio;

    let repo = init_repo();
    save(&repo.path);
    let saved = git(&repo.path, &["rev-parse", "HEAD"]);
    let branch = git(&repo.path, &["symbolic-ref", "HEAD"]);
    write_file(&repo.path, "other.txt", "staged work\n");
    git(&repo.path, &["add", "other.txt"]);
    write_file(&repo.path, "other.txt", "unstaged work\n");
    write_file(&repo.path, "pending.txt", "untracked work\n");
    let status = git(&repo.path, &["status", "--porcelain"]);

    let hook = repo.path.join(".git/hooks/pre-commit");
    let hook_ready = repo.path.join(".git/kite-test-hook-ready");
    if in_hook {
        std::fs::write(
            &hook,
            "#!/bin/sh\n: > .git/kite-test-hook-ready\nwhile :; do sleep 1; done\n",
        )
        .unwrap();
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let mut command = kt(&repo.path, &["land", "--allow-dirty", "--yes"]);
    let child = command
        .env(
            "KITE_OPENAI_URL",
            format!("http://{}", listener.local_addr().unwrap()),
        )
        .env("KITE_OPENAI_API_KEY", "local-test-key")
        .env("KITE_OPENAI_TIMEOUT_SECS", "10")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .spawn()
        .unwrap();

    // Always clean up this child's isolated process group, even if setup fails.
    let reached_interruption = std::panic::catch_unwind(|| {
        let stream = read_plan_request(&listener);
        if in_hook {
            send_plan(stream);
            let deadline = Instant::now() + Duration::from_secs(10);
            while !hook_ready.exists() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(10));
            }
            assert!(hook_ready.exists(), "CLI never reached the commit hook");
            None
        } else {
            Some(stream)
        }
    });
    let killed = Command::new("/bin/kill")
        .args(["-KILL", "--", &format!("-{}", child.id())])
        .output()
        .unwrap();
    let interrupted = child.wait_with_output().unwrap();
    reached_interruption.unwrap();
    assert_success(&killed);
    assert!(!interrupted.status.success());
    if in_hook {
        std::fs::remove_file(hook).unwrap();
    }

    let blocked = kt(&repo.path, &[]).output().unwrap();
    assert!(
        !blocked.status.success(),
        "interrupted work must block a new save"
    );
    assert!(String::from_utf8_lossy(&blocked.stderr).contains("kt undo"));
    assert_success(&kt(&repo.path, &["undo"]).output().unwrap());
    assert_eq!(git(&repo.path, &["symbolic-ref", "HEAD"]), branch);
    assert_eq!(git(&repo.path, &["rev-parse", "HEAD"]), saved);
    assert_eq!(git(&repo.path, &["status", "--porcelain"]), status);
    assert_eq!(git(&repo.path, &["show", ":other.txt"]), "staged work\n");
    assert_eq!(
        std::fs::read_to_string(repo.path.join("other.txt")).unwrap(),
        "unstaged work\n"
    );
    assert_eq!(
        std::fs::read_to_string(repo.path.join("pending.txt")).unwrap(),
        "untracked work\n"
    );
    assert!(git(&repo.path, &["stash", "list"]).contains("kt land temporary work"));
}

#[cfg(unix)]
#[test]
fn dirty_work_recovers_after_interruption_during_planning() {
    interrupted_dirty_land_recovers(false);
}

#[cfg(unix)]
#[test]
fn dirty_work_recovers_after_interruption_during_a_hook() {
    interrupted_dirty_land_recovers(true);
}

#[test]
fn edits_made_while_planning_are_not_committed() {
    let repo = init_repo();
    save(&repo.path);
    let head = git(&repo.path, &["rev-parse", "HEAD"]);
    let path = repo.path.clone();
    let output = land(&repo.path, false, move || {
        write_file(&path, "tracked.txt", "unsaved\n")
    });
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("worktree changed"));
    assert_eq!(git(&repo.path, &["rev-parse", "HEAD"]), head);
    assert_eq!(
        std::fs::read_to_string(repo.path.join("tracked.txt")).unwrap(),
        "unsaved\n"
    );
}

#[test]
fn commits_made_while_planning_are_not_rewritten() {
    let repo = init_repo();
    save(&repo.path);
    let path = repo.path.clone();
    let output = land(&repo.path, false, move || {
        write_file(&path, "other.txt", "new commit\n");
        git(&path, &["add", "other.txt"]);
        git(&path, &["commit", "-qm", "feat: new work"]);
    });
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("HEAD changed"));
    assert_eq!(
        git(&repo.path, &["log", "-1", "--format=%s"]).trim(),
        "feat: new work"
    );
    assert_eq!(git(&repo.path, &["status", "--porcelain"]), "");
}

#[test]
fn dirty_land_restores_its_own_stash_when_another_is_added() {
    let repo = init_repo();
    save(&repo.path);
    write_file(&repo.path, "pending.txt", "my pending work\n");
    let path = repo.path.clone();
    let output = land(&repo.path, true, move || {
        write_file(&path, "unrelated.txt", "another stash\n");
        git(&path, &["stash", "push", "-u", "-m", "unrelated stash"]);
    });
    assert_success(&output);
    assert_eq!(
        std::fs::read_to_string(repo.path.join("pending.txt")).unwrap(),
        "my pending work\n"
    );
    assert!(!repo.path.join("unrelated.txt").exists());
    assert!(git(&repo.path, &["stash", "list", "-1"]).contains("unrelated stash"));
}

#[test]
fn dirty_land_restores_staging_and_keeps_a_named_backup() {
    let repo = init_repo();
    save(&repo.path);
    write_file(&repo.path, "other.txt", "staged\n");
    git(&repo.path, &["add", "other.txt"]);
    write_file(&repo.path, "other.txt", "unstaged\n");
    write_file(&repo.path, "pending.txt", "untracked\n");
    let status = git(&repo.path, &["status", "--porcelain"]);
    let output = land(&repo.path, true, || {});
    assert_success(&output);
    assert_eq!(git(&repo.path, &["status", "--porcelain"]), status);
    assert_eq!(git(&repo.path, &["show", ":other.txt"]), "staged\n");
    assert_eq!(
        std::fs::read_to_string(repo.path.join("other.txt")).unwrap(),
        "unstaged\n"
    );
    assert!(git(&repo.path, &["stash", "list"]).contains("kt land temporary work"));
    assert!(String::from_utf8_lossy(&output.stdout).contains("Backup retained in `git stash`"));
}

#[cfg(unix)]
#[test]
fn dirty_land_cleanup_cannot_discard_another_worktrees_pending_work() {
    use std::os::unix::fs::PermissionsExt;

    let repo = init_repo();
    let peer_holder = TempDir::new("kite-peer-worktree");
    let peer = peer_holder.path.join("checkout");
    git(
        &repo.path,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "peer",
            peer.to_str().unwrap(),
        ],
    );
    write_file(&peer, "precious.txt", "unrelated work\n");
    save(&repo.path);
    write_file(&repo.path, "pending.txt", "my pending work\n");

    let real_git = std::env::split_paths(&std::env::var_os("PATH").unwrap())
        .map(|path| path.join("git"))
        .find(|path| path.is_file())
        .unwrap();
    let wrappers = TempDir::new("kite-stash-race");
    let wrapper = wrappers.path.join("git");
    // Interleave a linked-worktree stash after the lookup the old cleanup used
    // to authorize dropping stash@{0}. Safe cleanup must preserve this work
    // whether it remains in the peer checkout or has moved into its stash.
    std::fs::write(&wrapper, r#"#!/bin/sh
if [ "$1" = rev-parse ] && [ "$2" = --verify ] && [ "$3" = refs/stash ]; then
  "$KITE_TEST_REAL_GIT" "$@" || exit $?
  "$KITE_TEST_REAL_GIT" -C "$KITE_TEST_PEER" stash push -u -m 'concurrent peer backup' >/dev/null || exit $?
  exit 0
fi
exec "$KITE_TEST_REAL_GIT" "$@"
"#).unwrap();
    std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut paths = vec![wrappers.path.clone()];
    paths.extend(std::env::split_paths(&std::env::var_os("PATH").unwrap()));

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let server = std::thread::spawn(move || send_plan(read_plan_request(&listener)));
    let output = kt(&repo.path, &["land", "--yes", "--allow-dirty"])
        .env("PATH", std::env::join_paths(paths).unwrap())
        .env("KITE_TEST_REAL_GIT", real_git)
        .env("KITE_TEST_PEER", &peer)
        .env("KITE_OPENAI_URL", format!("http://{address}"))
        .env("KITE_OPENAI_API_KEY", "local-test-key")
        .env("KITE_OPENAI_TIMEOUT_SECS", "10")
        .output()
        .unwrap();
    server.join().unwrap();
    assert_success(&output);

    let stashes = git(&repo.path, &["stash", "list", "--format=%H %gs"]);
    let peer_stash = stashes
        .lines()
        .find(|line| line.ends_with(": concurrent peer backup"))
        .and_then(|line| line.split_once(' '))
        .map(|(oid, _)| oid);
    let peer_contents = std::fs::read_to_string(peer.join("precious.txt"))
        .ok()
        .or_else(|| {
            peer_stash.map(|oid| git(&repo.path, &["show", &format!("{oid}^3:precious.txt")]))
        });
    assert_eq!(
        peer_contents.as_deref(),
        Some("unrelated work\n"),
        "another worktree's work disappeared: {stashes}"
    );
    assert!(stashes.contains("kt land temporary work"));
    assert_eq!(
        std::fs::read_to_string(repo.path.join("pending.txt")).unwrap(),
        "my pending work\n"
    );
}

#[test]
fn undo_does_not_overwrite_fetched_remote_work() {
    let repo = init_repo();
    let remote = TempDir::new("kite-undo-remote");
    git(&remote.path, &["init", "--bare"]);
    git(
        &repo.path,
        &["remote", "add", "origin", remote.path.to_str().unwrap()],
    );
    let branch = git(&repo.path, &["branch", "--show-current"])
        .trim()
        .to_string();
    git(&repo.path, &["push", "origin", &branch]);
    save(&repo.path);
    let saved = git(&repo.path, &["rev-parse", "HEAD"]);
    assert_success(&land(&repo.path, false, || {}));
    let landed = git(&repo.path, &["rev-parse", "HEAD"]);
    git(&repo.path, &["push", "origin", &branch]);
    write_file(&repo.path, "other.txt", "teammate work\n");
    git(&repo.path, &["add", "other.txt"]);
    git(&repo.path, &["commit", "-qm", "feat: teammate work"]);
    git(&repo.path, &["push", "origin", &branch]);
    let newer = git(&repo.path, &["rev-parse", "HEAD"]);
    git(&repo.path, &["reset", "--hard", landed.trim()]);
    assert_success(&kt(&repo.path, &["undo"]).output().unwrap());
    assert_eq!(git(&repo.path, &["rev-parse", "HEAD"]), saved);
    assert_eq!(git(&remote.path, &["rev-parse", &branch]), newer);
}

#[test]
fn interrupted_undo_preserves_edits_made_after_the_interruption() {
    let repo = init_repo();
    save(&repo.path);
    assert_success(&land(&repo.path, false, || {}));
    let head = git(&repo.path, &["rev-parse", "HEAD"]);
    let record = git(&repo.path, &["cat-file", "blob", "refs/kite/land_state"]);
    let mut record: serde_json::Value = serde_json::from_str(&record).unwrap();
    record["phase"] = "undoing".into();
    record["from_head"] = head.trim().into();
    record["owner"] = record["land"]["owner"].clone();
    let mut hash = Command::new("git")
        .args(["hash-object", "-w", "--stdin"])
        .current_dir(&repo.path)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    hash.stdin
        .take()
        .unwrap()
        .write_all(record.to_string().as_bytes())
        .unwrap();
    let hash = hash.wait_with_output().unwrap();
    assert_success(&hash);
    let oid = String::from_utf8(hash.stdout).unwrap();
    git(
        &repo.path,
        &["update-ref", "refs/kite/land_state", oid.trim()],
    );
    write_file(&repo.path, "tracked.txt", "work after crash\n");
    assert_success(&kt(&repo.path, &["undo"]).output().unwrap());
    assert_eq!(
        std::fs::read_to_string(repo.path.join("tracked.txt")).unwrap(),
        "work after crash\n"
    );
}

#[cfg(unix)]
#[test]
fn successful_formatter_lands_in_one_pass_and_remains_undoable() {
    use std::os::unix::fs::PermissionsExt;
    let repo = init_repo();
    write_file(&repo.path, "tracked.txt", "editor formatting\n");
    let hook = repo.path.join(".git/hooks/pre-commit");
    std::fs::write(
        &hook,
        "#!/bin/sh\nprintf 'formatted\\n' > tracked.txt\ngit add tracked.txt\n",
    )
    .unwrap();
    std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert_success(&kt(&repo.path, &[]).output().unwrap());
    let saved = git(&repo.path, &["rev-parse", "HEAD"]);
    let branch = git(&repo.path, &["symbolic-ref", "HEAD"]);
    let output = land(&repo.path, false, || {});
    assert_success(&output);
    assert_eq!(git(&repo.path, &["symbolic-ref", "HEAD"]), branch);
    assert_eq!(
        git(&repo.path, &["show", "HEAD:tracked.txt"]),
        "formatted\n"
    );
    assert_eq!(git(&repo.path, &["status", "--porcelain"]), "");
    assert_eq!(
        std::fs::read_to_string(repo.path.join("tracked.txt")).unwrap(),
        "formatted\n"
    );
    assert_success(&kt(&repo.path, &["undo"]).output().unwrap());
    assert_eq!(git(&repo.path, &["rev-parse", "HEAD"]), saved);
    assert_eq!(
        git(&repo.path, &["show", "HEAD:tracked.txt"]),
        "editor formatting\n"
    );
}

#[cfg(unix)]
#[test]
fn successful_formatter_restores_dirty_work_and_staging() {
    use std::os::unix::fs::PermissionsExt;
    let repo = init_repo();
    save(&repo.path);
    let hook = repo.path.join(".git/hooks/pre-commit");
    std::fs::write(
        &hook,
        "#!/bin/sh\nprintf 'formatted\\n' > tracked.txt\ngit add tracked.txt\n",
    )
    .unwrap();
    std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
    write_file(&repo.path, "other.txt", "staged work\n");
    git(&repo.path, &["add", "other.txt"]);
    write_file(&repo.path, "other.txt", "unstaged work\n");
    write_file(&repo.path, "pending.txt", "untracked work\n");
    let status = git(&repo.path, &["status", "--porcelain"]);

    assert_success(&land(&repo.path, true, || {}));
    assert_eq!(
        git(&repo.path, &["show", "HEAD:tracked.txt"]),
        "formatted\n"
    );
    assert_eq!(git(&repo.path, &["status", "--porcelain"]), status);
    assert_eq!(git(&repo.path, &["show", ":other.txt"]), "staged work\n");
    assert_eq!(
        std::fs::read_to_string(repo.path.join("other.txt")).unwrap(),
        "unstaged work\n"
    );
    assert_eq!(
        std::fs::read_to_string(repo.path.join("pending.txt")).unwrap(),
        "untracked work\n"
    );
}

#[cfg(unix)]
#[test]
fn formatter_only_change_is_removed_in_one_land() {
    use std::os::unix::fs::PermissionsExt;
    let repo = init_repo();
    let base = git(&repo.path, &["rev-parse", "HEAD"]);
    save(&repo.path);
    let saved = git(&repo.path, &["rev-parse", "HEAD"]);
    let hook = repo.path.join(".git/hooks/pre-commit");
    std::fs::write(
        &hook,
        "#!/bin/sh\nprintf 'base\\n' > tracked.txt\ngit add tracked.txt\n",
    )
    .unwrap();
    std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();

    assert_success(&land(&repo.path, false, || {}));
    assert_eq!(git(&repo.path, &["rev-parse", "HEAD"]), base);
    assert_eq!(git(&repo.path, &["status", "--porcelain"]), "");
    assert_success(&kt(&repo.path, &["undo"]).output().unwrap());
    assert_eq!(git(&repo.path, &["rev-parse", "HEAD"]), saved);
}

#[cfg(unix)]
#[test]
fn formatter_conflict_keeps_the_pending_work_backup() {
    use std::os::unix::fs::PermissionsExt;
    let repo = init_repo();
    save(&repo.path);
    let hook = repo.path.join(".git/hooks/pre-commit");
    std::fs::write(
        &hook,
        "#!/bin/sh\nprintf 'formatted\\n' > tracked.txt\ngit add tracked.txt\n",
    )
    .unwrap();
    std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
    write_file(&repo.path, "tracked.txt", "pending work\n");
    git(&repo.path, &["add", "tracked.txt"]);

    let output = land(&repo.path, true, || {});
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("Your changes remain in stash"));
    assert_eq!(
        git(&repo.path, &["show", "HEAD:tracked.txt"]),
        "formatted\n"
    );
    assert_eq!(
        git(&repo.path, &["show", "stash@{0}:tracked.txt"]),
        "pending work\n"
    );
    assert!(repo.path.join(".git/kite-pending-work.json").exists());
}

#[cfg(unix)]
#[test]
fn successful_hook_cannot_stage_files_outside_the_plan() {
    use std::os::unix::fs::PermissionsExt;
    let repo = init_repo();
    save(&repo.path);
    let saved = git(&repo.path, &["rev-parse", "HEAD"]);
    let hook = repo.path.join(".git/hooks/pre-commit");
    std::fs::write(&hook, "#!/bin/sh\nprintf 'formatted\\n' > tracked.txt\nprintf 'unrelated\\n' > other.txt\ngit add tracked.txt other.txt\n").unwrap();
    std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();

    let output = land(&repo.path, false, || {});
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("outside this commit's planned files")
    );
    assert_eq!(git(&repo.path, &["rev-parse", "HEAD"]), saved);
    assert_eq!(
        std::fs::read_to_string(repo.path.join("other.txt")).unwrap(),
        "unrelated\n"
    );
    assert_eq!(git(&repo.path, &["diff", "--cached", "--name-only"]), "");
}

#[cfg(unix)]
#[test]
fn post_commit_staging_is_preserved_without_being_reported_as_landed() {
    use std::os::unix::fs::PermissionsExt;
    let repo = init_repo();
    save(&repo.path);
    let saved = git(&repo.path, &["rev-parse", "HEAD"]);
    let hook = repo.path.join(".git/hooks/post-commit");
    std::fs::write(
        &hook,
        "#!/bin/sh\nprintf 'too late\\n' > tracked.txt\ngit add tracked.txt\n",
    )
    .unwrap();
    std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();

    let output = land(&repo.path, false, || {});
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("left staged changes"));
    assert_eq!(git(&repo.path, &["rev-parse", "HEAD"]), saved);
    assert_eq!(
        std::fs::read_to_string(repo.path.join("tracked.txt")).unwrap(),
        "too late\n"
    );
    assert_eq!(git(&repo.path, &["diff", "--cached", "--name-only"]), "");
}

#[cfg(unix)]
#[test]
fn empty_root_land_checks_post_commit_staging() {
    use std::os::unix::fs::PermissionsExt;
    let repo = support::init_root_kite_repo();
    git(&repo.path, &["rm", "tracked.txt"]);
    assert_success(&kt(&repo.path, &[]).output().unwrap());
    let saved = git(&repo.path, &["rev-parse", "HEAD"]);
    let hook = repo.path.join(".git/hooks/post-commit");
    std::fs::write(
        &hook,
        "#!/bin/sh\nprintf 'too late\\n' > unrelated.txt\ngit add unrelated.txt\n",
    )
    .unwrap();
    std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();

    let output = kt(&repo.path, &["land", "--yes"]).output().unwrap();
    assert!(
        !output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(String::from_utf8_lossy(&output.stderr).contains("left staged changes"));
    assert_eq!(git(&repo.path, &["rev-parse", "HEAD"]), saved);
    assert_eq!(
        std::fs::read_to_string(repo.path.join("unrelated.txt")).unwrap(),
        "too late\n"
    );
    assert_eq!(git(&repo.path, &["diff", "--cached", "--name-only"]), "");
}

#[cfg(unix)]
#[test]
fn rejected_hook_returns_to_original_branch_and_can_retry_without_recreating_worktree() {
    use std::os::unix::fs::PermissionsExt;
    let repo = init_repo();
    save(&repo.path);
    let saved = git(&repo.path, &["rev-parse", "HEAD"]);
    let branch = git(&repo.path, &["symbolic-ref", "HEAD"]);
    write_file(&repo.path, ".git/info/exclude", "node_modules/\n");
    write_file(
        &repo.path,
        "node_modules/installed.txt",
        "installed dependencies\n",
    );
    let hook = repo.path.join(".git/hooks/pre-commit");
    std::fs::write(&hook, "#!/bin/sh\necho 'lint failed' >&2\nexit 1\n").unwrap();
    std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();

    let output = land(&repo.path, false, || {});
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("lint failed"));
    assert_eq!(git(&repo.path, &["symbolic-ref", "HEAD"]), branch);
    assert_eq!(git(&repo.path, &["rev-parse", "HEAD"]), saved);
    assert_eq!(git(&repo.path, &["status", "--porcelain"]), "");
    assert_eq!(git(&repo.path, &["branch", "--list", "kite-landing-*"]), "");
    assert_eq!(
        std::fs::read_to_string(repo.path.join("node_modules/installed.txt")).unwrap(),
        "installed dependencies\n"
    );

    std::fs::remove_file(hook).unwrap();
    assert_success(&land(&repo.path, false, || {}));
    assert_eq!(git(&repo.path, &["symbolic-ref", "HEAD"]), branch);
    assert_eq!(git(&repo.path, &["branch", "--list", "kite-landing-*"]), "");
    assert!(repo.path.join("node_modules/installed.txt").is_file());
}
