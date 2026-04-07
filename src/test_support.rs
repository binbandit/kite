use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

pub(crate) fn cwd_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

pub(crate) fn acquire_cwd_lock() -> std::sync::MutexGuard<'static, ()> {
    cwd_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

pub(crate) struct TempRepo {
    pub(crate) path: PathBuf,
}

impl TempRepo {
    fn new() -> Self {
        let unique = format!(
            "kite-test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("system time should be after unix epoch")
                .as_nanos()
        );
        let path = std::env::temp_dir().join(unique);
        fs::create_dir_all(&path).expect("temp repo directory should be created");
        Self { path }
    }
}

impl Drop for TempRepo {
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

pub(crate) fn write_file(repo: &Path, path: &str, contents: &str) {
    fs::write(repo.join(path), contents).expect("test file should be written");
}

pub(crate) fn with_repo_cwd<T>(repo: &Path, f: impl FnOnce() -> T) -> T {
    let original_dir = std::env::current_dir().expect("current dir should resolve");
    std::env::set_current_dir(repo).expect("should enter temp repo");
    let result = f();
    std::env::set_current_dir(&original_dir).expect("should restore original cwd");
    result
}

pub(crate) fn init_repo() -> TempRepo {
    let repo = TempRepo::new();
    git(&repo.path, &["init"]);
    git(&repo.path, &["config", "user.name", "Kite Test"]);
    git(&repo.path, &["config", "user.email", "kite@example.com"]);

    write_file(&repo.path, "tracked.txt", "base\n");
    write_file(&repo.path, "other.txt", "base\n");
    git(&repo.path, &["add", "tracked.txt", "other.txt"]);
    git(&repo.path, &["commit", "-m", "chore: initial"]);

    repo
}

pub(crate) fn init_root_kite_repo() -> TempRepo {
    let repo = TempRepo::new();
    git(&repo.path, &["init"]);
    git(&repo.path, &["config", "user.name", "Kite Test"]);
    git(&repo.path, &["config", "user.email", "kite@example.com"]);

    write_file(&repo.path, "tracked.txt", "base\n");
    git(&repo.path, &["add", "tracked.txt"]);
    git(&repo.path, &["commit", "-m", "[kite] save 12:00:00"]);

    repo
}
