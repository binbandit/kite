use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub(crate) fn acquire_cwd_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

pub(crate) struct TempDir {
    pub(crate) path: PathBuf,
}

impl TempDir {
    pub(crate) fn new(prefix: &str) -> Self {
        static NEXT_ID: AtomicU64 = AtomicU64::new(0);
        let unique = format!(
            "{prefix}-{}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("system time should be after unix epoch")
                .as_nanos(),
            NEXT_ID.fetch_add(1, Ordering::Relaxed)
        );
        let path = std::env::temp_dir().join(unique);
        fs::create_dir_all(&path).expect("temp directory should be created");
        Self { path }
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

pub(crate) fn git(repo: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(repo)
        .output()
        .expect("git command should run");

    assert!(
        output.status.success(),
        "git {} failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr)
    );

    String::from_utf8_lossy(&output.stdout).to_string()
}

pub(crate) fn write_file(root: &Path, path: &str, contents: &str) {
    let path = root.join(path);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("parent directories should be created");
    }
    fs::write(path, contents).expect("test file should be written");
}

pub(crate) fn with_repo_cwd<T>(repo: &Path, f: impl FnOnce() -> T) -> T {
    struct RestoreCwd(PathBuf);
    impl Drop for RestoreCwd {
        fn drop(&mut self) {
            std::env::set_current_dir(&self.0).expect("should restore original cwd");
        }
    }

    let _restore = RestoreCwd(std::env::current_dir().expect("current dir should resolve"));
    std::env::set_current_dir(repo).expect("should enter temp repo");
    f()
}

/// Waits up to ten seconds for one request to a fake AI endpoint and reads
/// all of it, returning the connection so the test decides when, or whether,
/// to answer. The deadline turns a CLI that never calls into a failure
/// rather than a hung test.
pub(crate) fn accept_ai_request(listener: &TcpListener) -> TcpStream {
    listener.set_nonblocking(true).unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    let stream = loop {
        match listener.accept() {
            Ok((stream, _)) => break stream,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                assert!(Instant::now() < deadline, "the CLI never called the AI");
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(error) => panic!("{error}"),
        }
    };
    read_ai_request(&stream);
    stream
}

/// Reads one whole request from an accepted fake AI connection and returns
/// its body.
pub(crate) fn read_ai_request(stream: &TcpStream) -> Vec<u8> {
    // BSD-derived systems, macOS included, hand the listener's non-blocking
    // mode to accepted sockets. Linux does not, so CI alone cannot catch this.
    stream.set_nonblocking(false).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();

    let mut reader = BufReader::new(stream);
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
    let mut body = vec![0; length];
    reader.read_exact(&mut body).unwrap();
    body
}

/// Answers an accepted AI request with `body` as the whole JSON response.
pub(crate) fn respond_json(mut stream: TcpStream, body: &serde_json::Value) {
    let body = body.to_string();
    write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .unwrap();
}

/// Answers an accepted AI request with `reply` as the model's output text.
pub(crate) fn respond_with_model_output(stream: TcpStream, reply: &serde_json::Value) {
    respond_json(
        stream,
        &serde_json::json!({ "output_text": reply.to_string() }),
    );
}

pub(crate) fn init_repo() -> TempDir {
    let repo = TempDir::new("kite-test");
    git(&repo.path, &["init"]);
    git(&repo.path, &["config", "user.name", "Kite Test"]);
    git(&repo.path, &["config", "user.email", "kite@example.com"]);

    write_file(&repo.path, "tracked.txt", "base\n");
    write_file(&repo.path, "other.txt", "base\n");
    git(&repo.path, &["add", "tracked.txt", "other.txt"]);
    git(&repo.path, &["commit", "-m", "chore: initial"]);

    repo
}

/// A repo wired to a bare `origin`, plus a branch that exists only on that
/// remote — the shape that used to make `kt go` fork a divergent branch.
pub(crate) fn init_repo_with_remote_branch(remote_branch: &str) -> (TempDir, TempDir) {
    let remote = TempDir::new("kite-test-remote");
    git(&remote.path, &["init", "--bare", "-q"]);

    let repo = init_repo();
    let remote_url = remote.path.display().to_string();
    git(&repo.path, &["remote", "add", "origin", &remote_url]);
    git(&repo.path, &["push", "-q", "origin", "HEAD"]);

    git(&repo.path, &["checkout", "-q", "-b", remote_branch]);
    write_file(&repo.path, "teammate.txt", "their work\n");
    git(&repo.path, &["add", "teammate.txt"]);
    git(&repo.path, &["commit", "-m", "feat: teammate work"]);
    git(&repo.path, &["push", "-q", "origin", remote_branch]);

    // Leave the branch on the remote only, as it would be for a colleague.
    let default_branch = git(&repo.path, &["rev-parse", "--abbrev-ref", "@{-1}"]);
    git(&repo.path, &["checkout", "-q", default_branch.trim()]);
    git(&repo.path, &["branch", "-q", "-D", remote_branch]);
    git(
        &repo.path,
        &[
            "update-ref",
            "-d",
            &format!("refs/remotes/origin/{remote_branch}"),
        ],
    );

    (repo, remote)
}

pub(crate) fn init_root_kite_repo() -> TempDir {
    let repo = TempDir::new("kite-test");
    git(&repo.path, &["init"]);
    git(&repo.path, &["config", "user.name", "Kite Test"]);
    git(&repo.path, &["config", "user.email", "kite@example.com"]);

    write_file(&repo.path, "tracked.txt", "base\n");
    git(&repo.path, &["add", "tracked.txt"]);
    git(&repo.path, &["commit", "-m", "[kite] save 12:00:00"]);

    repo
}
