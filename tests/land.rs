#[path = "../src/test_support.rs"]
#[allow(dead_code)]
mod support;

use std::io::Write;
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::process::{Command, Output};
use std::time::{Duration, Instant};
use support::{
    TempDir, accept_ai_request, git, init_repo, read_ai_request, respond_with_model_output,
    write_file,
};

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

fn send_plan(stream: TcpStream) {
    respond_with_model_output(
        stream,
        &serde_json::json!({"groups": [{"message": "feat: landed", "files": ["tracked.txt"]}]}),
    );
}

// Mutate the repository while the actual CLI is waiting for its AI response.
fn land(repo: &Path, dirty: bool, during_request: impl FnOnce() + Send + 'static) -> Output {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let server = std::thread::spawn(move || {
        let stream = accept_ai_request(&listener);
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

/// Uncommitted work `kt land --allow-dirty` has to carry: a staged edit with
/// an unstaged edit on top of it, and an untracked file. Returns its status.
fn make_dirty_work(repo: &Path) -> String {
    write_file(repo, "other.txt", "staged work\n");
    git(repo, &["add", "other.txt"]);
    write_file(repo, "other.txt", "unstaged work\n");
    write_file(repo, "pending.txt", "untracked work\n");
    git(repo, &["status", "--porcelain"])
}

fn assert_dirty_work_intact(repo: &Path, status: &str) {
    assert_eq!(git(repo, &["status", "--porcelain"]), status);
    assert_eq!(git(repo, &["show", ":other.txt"]), "staged work\n");
    assert_eq!(
        std::fs::read_to_string(repo.join("other.txt")).unwrap(),
        "unstaged work\n"
    );
    assert_eq!(
        std::fs::read_to_string(repo.join("pending.txt")).unwrap(),
        "untracked work\n"
    );
}

/// Kills `kt land --allow-dirty --yes` while it waits for the AI, or once it
/// is inside a pre-commit hook after the plan was accepted.
#[cfg(unix)]
fn kill_dirty_land(repo: &Path, in_hook: bool) {
    use std::os::unix::{fs::PermissionsExt, process::CommandExt};
    use std::process::Stdio;

    let hook = repo.join(".git/hooks/pre-commit");
    let hook_ready = repo.join(".git/kite-test-hook-ready");
    if in_hook {
        std::fs::write(
            &hook,
            "#!/bin/sh\n: > .git/kite-test-hook-ready\nwhile :; do sleep 1; done\n",
        )
        .unwrap();
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let mut command = kt(repo, &["land", "--allow-dirty", "--yes"]);
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
        let stream = accept_ai_request(&listener);
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
}

#[cfg(unix)]
#[test]
fn dirty_work_stays_in_place_when_planning_is_interrupted() {
    let repo = init_repo();
    save(&repo.path);
    let saved = git(&repo.path, &["rev-parse", "HEAD"]);
    let status = make_dirty_work(&repo.path);

    kill_dirty_land(&repo.path, false);

    // Nothing is stashed until the plan is accepted, so there is nothing to
    // recover and nothing blocks the next command.
    assert_eq!(git(&repo.path, &["rev-parse", "HEAD"]), saved);
    assert_dirty_work_intact(&repo.path, &status);
    assert_eq!(git(&repo.path, &["stash", "list"]), "");
    assert_success(&kt(&repo.path, &[]).output().unwrap());
}

#[cfg(unix)]
#[test]
fn dirty_work_recovers_after_interruption_during_a_hook() {
    let repo = init_repo();
    save(&repo.path);
    let saved = git(&repo.path, &["rev-parse", "HEAD"]);
    let branch = git(&repo.path, &["symbolic-ref", "HEAD"]);
    let status = make_dirty_work(&repo.path);

    kill_dirty_land(&repo.path, true);

    let blocked = kt(&repo.path, &[]).output().unwrap();
    assert!(
        !blocked.status.success(),
        "interrupted work must block a new save"
    );
    assert!(String::from_utf8_lossy(&blocked.stderr).contains("kt undo"));
    assert_success(&kt(&repo.path, &["undo"]).output().unwrap());
    assert_eq!(git(&repo.path, &["symbolic-ref", "HEAD"]), branch);
    assert_eq!(git(&repo.path, &["rev-parse", "HEAD"]), saved);
    assert_dirty_work_intact(&repo.path, &status);
    assert!(git(&repo.path, &["stash", "list"]).contains("kt land temporary work"));
}

/// Finding nothing to land, or answering no, must not cycle the work through
/// `git stash` and leave a backup entry behind for a land that never happened.
#[test]
fn a_dirty_land_that_rewrites_nothing_never_stashes() {
    let unsaved = init_repo();
    let status = make_dirty_work(&unsaved.path);
    let nothing_to_land = kt(&unsaved.path, &["land", "--allow-dirty"])
        .output()
        .unwrap();
    assert_success(&nothing_to_land);
    assert!(String::from_utf8_lossy(&nothing_to_land.stdout).contains("nothing to land"));
    assert_dirty_work_intact(&unsaved.path, &status);
    assert_eq!(git(&unsaved.path, &["stash", "list"]), "");

    let repo = init_repo();
    save(&repo.path);
    let saved = git(&repo.path, &["rev-parse", "HEAD"]);
    let status = make_dirty_work(&repo.path);
    let mut command = kt(&repo.path, &["land", "--allow-dirty"]);
    for key in [
        "KITE_OPENAI_API_KEY",
        "OPENAI_API_KEY",
        "KITE_API_KEY",
        "OPENAI_KEY",
        "AI_GATEWAY_API_KEY",
    ] {
        command.env_remove(key);
    }
    let mut declined = command
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    // Without AI: a manual commit message, then "no" at the plan.
    declined
        .stdin
        .take()
        .unwrap()
        .write_all(b"feat: manual\nn\n")
        .unwrap();
    let declined = declined.wait_with_output().unwrap();
    assert_success(&declined);
    assert!(String::from_utf8_lossy(&declined.stdout).contains("Aborted"));
    assert_eq!(git(&repo.path, &["rev-parse", "HEAD"]), saved);
    assert_dirty_work_intact(&repo.path, &status);
    assert_eq!(git(&repo.path, &["stash", "list"]), "");
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
fn dirty_land_restores_staging_and_keeps_a_named_backup() {
    let repo = init_repo();
    save(&repo.path);
    let status = make_dirty_work(&repo.path);
    let output = land(&repo.path, true, || {});
    assert_success(&output);
    assert_dirty_work_intact(&repo.path, &status);
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
    // Push a linked-worktree stash on top of Kite's own each time Kite looks
    // its entry up, so stash@{0} is never Kite's. Restoring must still apply
    // Kite's entry, and must preserve the peer's work whether it remains in
    // the peer checkout or has moved into its stash.
    std::fs::write(&wrapper, r#"#!/bin/sh
if [ "$1" = stash ] && [ "$2" = list ]; then
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
    let server = std::thread::spawn(move || send_plan(accept_ai_request(&listener)));
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

    let stashes = git(&repo.path, &["stash", "list", "--format=%gs"]);
    let subjects: Vec<&str> = stashes.lines().collect();
    assert!(
        matches!(subjects[..], [peer, kite] if peer.ends_with(": concurrent peer backup")
            && kite.contains("kt land temporary work")),
        "{stashes}"
    );
    assert_eq!(
        git(&repo.path, &["show", "stash@{0}^3:precious.txt"]),
        "unrelated work\n",
        "another worktree's work disappeared"
    );
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
    let undo = kt(&repo.path, &["undo"]).output().unwrap();
    assert_success(&undo);
    assert!(String::from_utf8_lossy(&undo.stdout).contains("left untouched"));
    assert_eq!(git(&repo.path, &["rev-parse", "HEAD"]), saved);
    assert_eq!(git(&remote.path, &["rev-parse", &branch]), newer);
}

#[test]
fn undo_reverts_origin_only_when_it_holds_the_land() {
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
    let published = git(&remote.path, &["rev-parse", &branch]);
    save(&repo.path);
    let saved = git(&repo.path, &["rev-parse", "HEAD"]);

    // A local land never reached origin, so undoing it has nothing to say
    // about the remote and nothing to push.
    assert_success(&land(&repo.path, false, || {}));
    let undo = kt(&repo.path, &["undo"]).output().unwrap();
    assert_success(&undo);
    let stdout = String::from_utf8_lossy(&undo.stdout);
    assert!(
        stdout.lines().count() == 1 && stdout.contains("Restored pre-land saves"),
        "{stdout}"
    );
    assert_eq!(git(&remote.path, &["rev-parse", &branch]), published);

    // A published land is reverted on origin too, and says so.
    assert_success(&land(&repo.path, false, || {}));
    assert_success(&kt(&repo.path, &["publish"]).output().unwrap());
    let undo = kt(&repo.path, &["undo"]).output().unwrap();
    assert_success(&undo);
    assert!(
        String::from_utf8_lossy(&undo.stdout).contains("Restored the saves on `origin/"),
        "{}",
        String::from_utf8_lossy(&undo.stdout)
    );
    assert_eq!(git(&repo.path, &["rev-parse", "HEAD"]), saved);
    assert_eq!(git(&remote.path, &["rev-parse", &branch]), saved);
}

#[test]
fn undo_reverts_origin_without_a_tracking_ref() {
    let repo = init_repo();
    let remote = TempDir::new("kite-undo-remote");
    git(&remote.path, &["init", "--bare"]);
    let base = git(&repo.path, &["branch", "--show-current"])
        .trim()
        .to_string();
    // Fetching only the base branch means publishing `feature` never creates
    // `origin/feature` locally.
    git(
        &repo.path,
        &[
            "remote",
            "add",
            "-t",
            &base,
            "origin",
            remote.path.to_str().unwrap(),
        ],
    );
    git(&repo.path, &["push", "origin", &base]);
    git(&repo.path, &["switch", "-c", "feature"]);
    save(&repo.path);
    let saved = git(&repo.path, &["rev-parse", "HEAD"]);

    // Asking origin finds no `feature`, so an unpublished land stays quiet.
    assert_success(&land(&repo.path, false, || {}));
    let undo = kt(&repo.path, &["undo"]).output().unwrap();
    assert_success(&undo);
    let stdout = String::from_utf8_lossy(&undo.stdout);
    assert!(
        stdout.lines().count() == 1 && stdout.contains("Restored pre-land saves"),
        "{stdout}"
    );

    assert_success(&land(&repo.path, false, || {}));
    assert_success(&kt(&repo.path, &["publish"]).output().unwrap());
    assert_eq!(
        git(&repo.path, &["for-each-ref", "refs/remotes/origin/feature"]),
        ""
    );
    let undo = kt(&repo.path, &["undo"]).output().unwrap();
    assert_success(&undo);
    assert!(
        String::from_utf8_lossy(&undo.stdout).contains("Restored the saves on `origin/feature`"),
        "{}",
        String::from_utf8_lossy(&undo.stdout)
    );
    assert_eq!(git(&remote.path, &["rev-parse", "feature"]), saved);
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
    let status = make_dirty_work(&repo.path);

    assert_success(&land(&repo.path, true, || {}));
    assert_eq!(
        git(&repo.path, &["show", "HEAD:tracked.txt"]),
        "formatted\n"
    );
    assert_dirty_work_intact(&repo.path, &status);
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

/// Answers planning requests the way a model would, several at a time: one
/// commit per window holding all its files, or one message per save.
struct FakeModel {
    address: std::net::SocketAddr,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    server: std::thread::JoinHandle<usize>,
}

impl FakeModel {
    fn start(status: u16) -> Self {
        use std::sync::atomic::{AtomicBool, Ordering};
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let stop = std::sync::Arc::new(AtomicBool::new(false));
        let stopping = std::sync::Arc::clone(&stop);
        let server = std::thread::spawn(move || {
            let mut answering = Vec::new();
            while !stopping.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        answering.push(std::thread::spawn(move || answer(stream, status)))
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(5))
                    }
                    Err(error) => panic!("{error}"),
                }
            }
            let answered = answering.len();
            for thread in answering {
                thread.join().unwrap();
            }
            answered
        });
        Self {
            address,
            stop,
            server,
        }
    }

    fn land(&self, repo: &Path, args: &[&str]) -> Output {
        self.configure(kt(repo, args)).output().unwrap()
    }

    /// Points a `kt` command, however it is launched, at this model.
    fn configure(&self, mut command: Command) -> Command {
        command
            .env("KITE_OPENAI_URL", format!("http://{}", self.address))
            .env("KITE_OPENAI_API_KEY", "local-test-key")
            .env("KITE_OPENAI_TIMEOUT_SECS", "10")
            .stdin(std::process::Stdio::null());
        command
    }

    /// How many requests were made.
    fn finish(self) -> usize {
        self.stop.store(true, std::sync::atomic::Ordering::SeqCst);
        self.server.join().unwrap()
    }
}

fn answer(stream: TcpStream, status: u16) {
    let body = read_ai_request(&stream);
    let request: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let context: serde_json::Value =
        serde_json::from_str(request["input"].as_str().unwrap()).unwrap();

    let reply = if let Some(saves) = context["saves"].as_array() {
        let messages: Vec<serde_json::Value> = saves
            .iter()
            .map(|save| {
                serde_json::json!({
                    "id": save["id"],
                    "message": format!("feat: change {}", save["changed_files"][0].as_str().unwrap()),
                })
            })
            .collect();
        serde_json::json!({ "messages": messages })
    } else {
        let message = match context["history_slice"]["position"].as_u64() {
            Some(position) => format!("feat: window {position}"),
            None => "feat: landed".to_string(),
        };
        serde_json::json!({"groups": [{"message": message, "files": context["changed_files"]}]})
    };
    let body = match status {
        200 => serde_json::json!({ "output_text": reply.to_string() }).to_string(),
        _ => serde_json::json!({ "error": { "message": "Incorrect API key." } }).to_string(),
    };
    let mut stream = stream;
    write!(
        stream,
        "HTTP/1.1 {status} Test\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .unwrap();
}

/// One save per entry, each touching its own file and a shared one, made at
/// the given number of minutes after an arbitrary start.
fn timed_saves(repo: &Path, minutes: &[i64]) -> Vec<String> {
    minutes
        .iter()
        .enumerate()
        .map(|(index, minute)| {
            write_file(repo, &format!("work/step_{index:02}.txt"), "done\n");
            write_file(repo, "shared.txt", &format!("after step {index}\n"));
            git(repo, &["add", "-A"]);
            let date = format!("@{} +0000", 1_790_000_000 + minute * 60);
            let committed = Command::new("git")
                .args(["commit", "-qm", &format!("[kite] save {index}")])
                .env("GIT_COMMITTER_DATE", &date)
                .env("GIT_AUTHOR_DATE", &date)
                .current_dir(repo)
                .output()
                .unwrap();
            assert_success(&committed);
            git(repo, &["rev-parse", "HEAD"]).trim().to_string()
        })
        .collect()
}

fn tree_of(repo: &Path, revision: &str) -> String {
    git(repo, &["rev-parse", &format!("{revision}^{{tree}}")])
}

/// Trees of the commits on top of `base`, oldest first.
fn landed_trees(repo: &Path, base: &str) -> Vec<String> {
    git(
        repo,
        &["log", "--reverse", "--format=%T", &format!("{base}..HEAD")],
    )
    .lines()
    .map(|tree| format!("{tree}\n"))
    .collect()
}

#[test]
fn a_fine_land_follows_the_history_in_windows() {
    let repo = init_repo();
    let base = git(&repo.path, &["rev-parse", "HEAD"]).trim().to_string();
    // Ten saves a minute apart, a two-hour pause, then a busy stretch that
    // outgrows one window.
    let minutes: Vec<i64> = (0..10).chain((0..20).map(|minute| 130 + minute)).collect();
    let saves = timed_saves(&repo.path, &minutes);
    let branch = git(&repo.path, &["symbolic-ref", "HEAD"]);

    let model = FakeModel::start(200);
    let output = model.land(&repo.path, &["land", "--yes", "--granularity", "fine"]);
    let requests = model.finish();
    assert_success(&output);

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("Plan: 30 saves → 3 commits in 3 windows"),
        "{stdout}"
    );
    assert!(stdout.contains("Saves 11-22"), "{stdout}");
    assert_eq!(requests, 3);
    assert_eq!(
        git(
            &repo.path,
            &["log", "--reverse", "--format=%s", &format!("{base}..HEAD")]
        ),
        "feat: window 1\nfeat: window 2\nfeat: window 3\n"
    );
    // Each commit is exactly the save its window ends on.
    assert_eq!(
        landed_trees(&repo.path, &base),
        [&saves[9], &saves[21], &saves[29]].map(|save| tree_of(&repo.path, save))
    );
    assert_eq!(git(&repo.path, &["symbolic-ref", "HEAD"]), branch);
    assert_eq!(git(&repo.path, &["status", "--porcelain"]), "");
    assert_eq!(git(&repo.path, &["worktree", "list"]).lines().count(), 1);

    assert_success(&kt(&repo.path, &["undo"]).output().unwrap());
    assert_eq!(git(&repo.path, &["rev-parse", "HEAD"]).trim(), saves[29]);
}

#[test]
fn a_per_save_land_keeps_every_save_and_rewrites_its_message() {
    let repo = init_repo();
    let base = git(&repo.path, &["rev-parse", "HEAD"]).trim().to_string();
    let minutes: Vec<i64> = (0..10).collect();
    let saves = timed_saves(&repo.path, &minutes);
    write_file(&repo.path, "pending.txt", "uncommitted work\n");

    let model = FakeModel::start(200);
    let output = model.land(
        &repo.path,
        &[
            "land",
            "-y",
            "-g",
            "per-save",
            "--allow-dirty",
            "--tag",
            "PROJ-7",
        ],
    );
    let requests = model.finish();
    assert_success(&output);

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("Plan: 10 saves → 10 commits, one per save"),
        "{stdout}"
    );
    // Eight saves fit one request; the other two take a second.
    assert_eq!(requests, 2);
    assert_eq!(
        landed_trees(&repo.path, &base),
        saves
            .iter()
            .map(|save| tree_of(&repo.path, save))
            .collect::<Vec<_>>()
    );
    let subjects = git(
        &repo.path,
        &["log", "--reverse", "--format=%s", &format!("{base}..HEAD")],
    );
    assert_eq!(
        subjects.lines().next(),
        Some("feat: change shared.txt [PROJ-7]"),
        "{subjects}"
    );
    assert!(!subjects.contains("[kite] save"), "{subjects}");
    assert_eq!(
        std::fs::read_to_string(repo.path.join("pending.txt")).unwrap(),
        "uncommitted work\n"
    );
}

#[test]
fn a_fine_land_without_ai_fails_cleanly_and_asks_for_nothing() {
    let repo = init_repo();
    let saves = timed_saves(&repo.path, &[0, 1, 2]);

    for args in [
        &["land", "-g", "fine"][..],
        &["land", "--yes", "-g", "per-save"],
    ] {
        let model = FakeModel::start(401);
        let output = model.land(&repo.path, args);
        model.finish();

        assert!(!output.status.success(), "{args:?}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("AI planning failed for saves"), "{stderr}");
        assert!(stderr.contains("check the API key"), "{stderr}");
        assert!(
            !String::from_utf8_lossy(&output.stdout).contains("commit message"),
            "a windowed land must not fall back to one manual message"
        );
        assert_eq!(git(&repo.path, &["rev-parse", "HEAD"]).trim(), saves[2]);
        assert_eq!(git(&repo.path, &["status", "--porcelain"]), "");
    }
}

#[cfg(unix)]
#[test]
fn an_interrupted_fine_land_recovers_with_undo() {
    use std::os::unix::{fs::PermissionsExt, process::CommandExt};

    let repo = init_repo();
    let saves = timed_saves(&repo.path, &[0, 1, 2, 3]);
    let branch = git(&repo.path, &["symbolic-ref", "HEAD"]);
    let ready = repo.path.join(".git/kite-test-hook-ready");
    let hook = repo.path.join(".git/hooks/pre-commit");
    std::fs::write(
        &hook,
        "#!/bin/sh\n: > \"$KITE_TEST_READY\"\nwhile :; do sleep 1; done\n",
    )
    .unwrap();
    std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();

    let model = FakeModel::start(200);
    let child = kt(&repo.path, &["land", "--yes", "-g", "fine"])
        .env("KITE_OPENAI_URL", format!("http://{}", model.address))
        .env("KITE_OPENAI_API_KEY", "local-test-key")
        .env("KITE_TEST_READY", &ready)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .process_group(0)
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    while !ready.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    let killed = Command::new("/bin/kill")
        .args(["-KILL", "--", &format!("-{}", child.id())])
        .output()
        .unwrap();
    child.wait_with_output().unwrap();
    model.finish();
    assert!(ready.exists(), "the land never reached its commit hook");
    assert_success(&killed);
    std::fs::remove_file(hook).unwrap();

    // The private checkout outlived the process; recovery removes it.
    assert_eq!(git(&repo.path, &["worktree", "list"]).lines().count(), 2);
    let blocked = kt(&repo.path, &[]).output().unwrap();
    assert!(String::from_utf8_lossy(&blocked.stderr).contains("kt undo"));
    assert_success(&kt(&repo.path, &["undo"]).output().unwrap());

    assert_eq!(git(&repo.path, &["symbolic-ref", "HEAD"]), branch);
    assert_eq!(git(&repo.path, &["rev-parse", "HEAD"]).trim(), saves[3]);
    assert_eq!(git(&repo.path, &["status", "--porcelain"]), "");
    assert_eq!(git(&repo.path, &["worktree", "list"]).lines().count(), 1);
    assert_eq!(git(&repo.path, &["branch", "--list", "kite-landing-*"]), "");
}

/// The private checkout holds every saved file, including ones a later save
/// deleted. A shared temp directory and a permissive umask must not expose it.
#[cfg(unix)]
#[test]
fn a_fine_land_keeps_its_private_checkout_owner_only() {
    use std::os::unix::fs::PermissionsExt;

    let repo = init_repo();
    std::fs::set_permissions(&repo.path, std::fs::Permissions::from_mode(0o700)).unwrap();
    timed_saves(&repo.path, &[0, 1, 2]);
    // A world-readable temp directory, unlike the per-user one macOS uses.
    let shared_temp = TempDir::new("kite-shared-temp");
    std::fs::set_permissions(&shared_temp.path, std::fs::Permissions::from_mode(0o777)).unwrap();
    let modes = repo.path.join(".git/kite-test-modes");
    let hook = repo.path.join(".git/hooks/pre-commit");
    std::fs::write(
        &hook,
        "#!/bin/sh\nls -ld .. \"$PWD\" > \"$KITE_TEST_MODES\"\n",
    )
    .unwrap();
    std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();

    let model = FakeModel::start(200);
    let mut command = Command::new("/bin/sh");
    command
        .args([
            "-c",
            "umask 000 && exec \"$0\" \"$@\"",
            env!("CARGO_BIN_EXE_kt"),
        ])
        .args(["land", "--yes", "-g", "fine"])
        .current_dir(&repo.path)
        .env("TMPDIR", &shared_temp.path)
        .env("KITE_TEST_MODES", &modes);
    let output = model.configure(command).output().unwrap();
    model.finish();
    assert_success(&output);

    let listed = std::fs::read_to_string(&modes).unwrap();
    let mut lines = listed.lines();
    let folder = lines.next().unwrap();
    assert!(folder.starts_with("drwx------"), "{listed}");
    assert!(lines.next().unwrap().contains("kite-land-"), "{listed}");
    // Nothing is left behind in the shared directory.
    assert_eq!(std::fs::read_dir(&shared_temp.path).unwrap().count(), 0);
}

/// Saves a superproject commit at `minute`, after `change` has run in it.
fn timed_save(repo: &Path, minute: i64, label: &str, change: impl FnOnce()) -> String {
    change();
    git(repo, &["add", "-A"]);
    let date = format!("@{} +0000", 1_790_000_000 + minute * 60);
    let committed = Command::new("git")
        .args(["commit", "-qm", &format!("[kite] save {label}")])
        .env("GIT_COMMITTER_DATE", &date)
        .env("GIT_AUTHOR_DATE", &date)
        .current_dir(repo)
        .output()
        .unwrap();
    assert_success(&committed);
    git(repo, &["rev-parse", "HEAD"]).trim().to_string()
}

/// Submodule pointers are saved as commits, not files. The private checkout
/// has no submodule checked out, so they must land from the saves.
#[test]
fn windowed_lands_carry_submodule_pointers() {
    let library = init_repo();
    let commits: Vec<String> = ["second", "third"]
        .iter()
        .map(|label| {
            git(
                &library.path,
                &[
                    "commit",
                    "-q",
                    "--allow-empty",
                    "-m",
                    &format!("feat: {label}"),
                ],
            );
            git(&library.path, &["rev-parse", "HEAD"])
                .trim()
                .to_string()
        })
        .collect();
    let library_url = library.path.to_str().unwrap();

    let repo = init_repo();
    git(&repo.path, &["clone", "-q", library_url, "module"]);
    git(&repo.path.join("module"), &["checkout", "-q", "HEAD~2"]);
    git(&repo.path, &["add", "module"]);
    git(&repo.path, &["commit", "-qm", "chore: add module"]);
    let base = git(&repo.path, &["rev-parse", "HEAD"]).trim().to_string();

    let module = repo.path.join("module");
    let extra = repo.path.join("extra");
    let saves = [
        timed_save(&repo.path, 0, "1", || {
            git(&module, &["checkout", "-q", &commits[0]]);
            write_file(&repo.path, "notes.txt", "moved the module\n");
        }),
        // After a pause, so it is a window of its own: a pointer is added...
        timed_save(&repo.path, 120, "2", || {
            git(&module, &["checkout", "-q", &commits[1]]);
            git(&repo.path, &["clone", "-q", library_url, "extra"]);
        }),
        // ...and removed again.
        timed_save(&repo.path, 121, "3", || {
            git(&repo.path, &["rm", "-q", "--cached", "extra"]);
            std::fs::remove_dir_all(&extra).unwrap();
            write_file(&repo.path, "notes.txt", "dropped the extra module\n");
        }),
    ];
    let save_trees: Vec<String> = saves.iter().map(|save| tree_of(&repo.path, save)).collect();

    for (granularity, expected) in [
        ("fine", vec![save_trees[0].clone(), save_trees[2].clone()]),
        ("per-save", save_trees.clone()),
    ] {
        let model = FakeModel::start(200);
        let output = model.land(&repo.path, &["land", "--yes", "-g", granularity]);
        model.finish();
        assert_success(&output);

        assert_eq!(landed_trees(&repo.path, &base), expected, "{granularity}");
        assert_eq!(
            git(&repo.path, &["status", "--porcelain"]),
            "",
            "{granularity}"
        );
        assert_success(&kt(&repo.path, &["undo"]).output().unwrap());
        assert_eq!(git(&repo.path, &["rev-parse", "HEAD"]).trim(), saves[2]);
    }
}
