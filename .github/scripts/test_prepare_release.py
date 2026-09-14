"""Exercise release preparation against local Git repositories and a fake gh."""

from datetime import datetime, timezone
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest


SCRIPT = Path(__file__).with_name("prepare_release.py").resolve()


class PrepareReleaseTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="kite-release-test-")
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.repo = self.root / "repo"
        self.repo.mkdir()
        self.git("init", "-b", "main")
        self.git("config", "user.name", "Release Test")
        self.git("config", "user.email", "test@example.invalid")
        self.git("config", "commit.gpgsign", "false")
        self.git("config", "core.hooksPath", str(self.root / "no-hooks"))
        (self.repo / "Cargo.toml").write_text(
            '[package]\nname = "release-test"\nversion = "0.1.0"\nedition = "2024"\n'
        )
        (self.repo / "src").mkdir()
        (self.repo / "src/main.rs").write_text('fn main() { println!("{}", env!("CARGO_PKG_VERSION")); }')
        self.run_command("cargo", "generate-lockfile")
        self.git("add", "Cargo.toml", "Cargo.lock", "src")
        self.git("commit", "-m", "feat: first change")
        self.git("commit", "--allow-empty", "-m", "fix: second change")
        binary = self.root / "bin"
        binary.mkdir()
        fake_gh = binary / "gh"
        fake_gh.write_text(
            '#!/bin/sh\nif [ "$GH_RESPONSE" = "404" ]; then echo "gh: Not Found (HTTP 404)" >&2; exit 1; fi\n'
            'if [ "$GH_RESPONSE" = "error" ]; then echo "network unavailable" >&2; exit 1; fi\n'
            'printf "%s\\n" "$GH_RESPONSE"\n'
        )
        fake_gh.chmod(0o755)
        self.env = dict(
            os.environ, PATH=f"{binary}:{os.environ['PATH']}", GH_RESPONSE="404",
            GITHUB_REPOSITORY="example/repo", GITHUB_SERVER_URL="https://example.invalid",
            GITHUB_OUTPUT=str(self.root / "outputs"),
        )
        today = datetime.now(timezone.utc)
        self.tag = today.strftime("%Y.%m.%d")
        self.version = f"{today.year}.{today.month}.{today.day}"

    def run_command(self, *args):
        return subprocess.check_output(args, cwd=self.repo, text=True, stderr=subprocess.DEVNULL).strip()

    def git(self, *args):
        return self.run_command("git", *args)

    def prepare(self, response="404"):
        self.env["GH_RESPONSE"] = response
        result = subprocess.run(["python3", str(SCRIPT)], cwd=self.repo, env=self.env, text=True, capture_output=True)
        return result

    def outputs(self):
        return dict(line.split("=", 1) for line in (self.root / "outputs").read_text().splitlines())

    def test_prepared_source_builds_the_release_version_and_links_every_commit(self):
        before = self.git("rev-parse", "HEAD")
        shas = self.git("log", "--format=%H").splitlines()
        result = self.prepare()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.outputs()["source_sha"], before)
        self.assertEqual(self.git("rev-parse", "HEAD"), before)
        self.assertEqual(self.run_command("cargo", "run", "--quiet", "--locked"), self.version)
        notes = (self.repo / "release_notes.md").read_text()
        for sha in shas:
            self.assertIn(f"/commit/{sha})", notes)
        self.assertEqual(self.git("tag", "--list"), "")

    def test_published_release_and_api_failure_leave_source_untouched(self):
        for response, expected in [(json.dumps({"draft": False}), 0), ("error", 1)]:
            with self.subTest(response=response):
                result = self.prepare(response)
                self.assertEqual(result.returncode, expected, result.stderr)
                self.assertEqual(self.git("status", "--porcelain"), "")
                self.assertFalse((self.repo / "release_notes.md").exists())

    def test_retry_uses_tagged_source_without_duplicating_changelog(self):
        result = self.prepare()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.git("add", "Cargo.toml", "Cargo.lock", "CHANGELOG.md")
        self.git("commit", "-m", f"chore(release): {self.tag} [skip-ci]")
        self.git("tag", self.tag)
        tagged_sha = self.git("rev-parse", "HEAD")
        changelog = (self.repo / "CHANGELOG.md").read_text()
        self.git("commit", "--allow-empty", "-m", "feat: tomorrow's change")
        result = self.prepare(json.dumps({"draft": True}))
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.outputs()["source_sha"], tagged_sha)
        self.assertEqual(self.outputs()["tag_exists"], "true")
        self.assertEqual((self.repo / "CHANGELOG.md").read_text(), changelog)
        self.assertNotIn("tomorrow", (self.repo / "release_notes.md").read_text())

    def test_previous_release_excludes_old_changes(self):
        self.git("tag", "2000.01.01")
        self.git("commit", "--allow-empty", "-m", "docs: fresh change")
        result = self.prepare()
        self.assertEqual(result.returncode, 0, result.stderr)
        notes = (self.repo / "release_notes.md").read_text()
        self.assertIn("fresh change", notes)
        self.assertNotIn("first change", notes)

    def test_no_new_changes_skips_release(self):
        self.git("tag", "2000.01.01")
        result = self.prepare()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.outputs(), {"should_release": "false"})
        self.assertEqual(self.git("status", "--porcelain"), "")


if __name__ == "__main__":
    unittest.main()
