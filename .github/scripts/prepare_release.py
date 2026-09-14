"""Prepare a daily release from a pinned Git commit, without publishing it."""

from collections import defaultdict
from datetime import datetime, timezone
import json
import os
from pathlib import Path
import re
import subprocess


BOT_NAME = "github-actions[bot]"
BOT_EMAIL = "41898282+github-actions[bot]@users.noreply.github.com"
CALENDAR_TAG = "[0-9][0-9][0-9][0-9].[0-1][0-9].[0-3][0-9]"
SECTIONS = {
    "breaking": "Breaking Changes",
    "feat": "Features",
    "fix": "Bug Fixes",
    "perf": "Performance",
    "refactor": "Refactors",
    "docs": "Documentation",
    "build": "Build System",
    "ci": "CI",
    "test": "Tests",
    "style": "Style",
    "chore": "Chores",
    "revert": "Reverts",
    "other": "Other Changes",
}
CONVENTIONAL = re.compile(r"^([a-z]+)(?:\(([^)]+)\))?(!)?: (.+)$")


def git(*args):
    return subprocess.check_output(["git", *args], text=True).strip("\r\n")


def release_notes(range_spec, repo_url):
    raw = git("log", range_spec, "--reverse", "--format=%H%x1f%an%x1f%ae%x1f%s%x1f%b%x1e")
    grouped = defaultdict(list)
    for record in raw.split("\x1e"):
        if not record.strip():
            continue
        sha, author, email, subject, body = record.strip("\r\n").split("\x1f", 4)
        if author == BOT_NAME or email == BOT_EMAIL:
            continue
        if subject.startswith("chore(release): ") or "[skip-ci]" in subject:
            continue
        section, description = "other", subject
        if match := CONVENTIONAL.match(subject):
            section, scope, breaking, description = match.groups()
            if breaking or "BREAKING CHANGE:" in body or "BREAKING-CHANGE:" in body:
                section = "breaking"
            if scope:
                description = f"**{scope}:** {description}"
        if section not in SECTIONS:
            section = "other"
        grouped[section].append(f"- {description} ([`{sha[:7]}`]({repo_url}/commit/{sha}))")
    return "\n\n".join(
        f"### {title}\n\n" + "\n".join(grouped[key])
        for key, title in SECTIONS.items()
        if grouped[key]
    )


def main():
    today = datetime.now(timezone.utc)
    tag = today.strftime("%Y.%m.%d")
    version = f"{today.year}.{today.month}.{today.day}"
    repository = os.environ["GITHUB_REPOSITORY"]
    repo_url = f"{os.environ['GITHUB_SERVER_URL']}/{repository}"
    output = Path(os.environ["GITHUB_OUTPUT"])

    # Only a confirmed 404 means absent. Authentication and network errors stop
    # preparation, instead of being mistaken for permission to create a release.
    release = subprocess.run(
        ["gh", "api", f"repos/{repository}/releases/tags/{tag}"],
        text=True, capture_output=True,
    )
    if release.returncode == 0:
        if not json.loads(release.stdout)["draft"]:
            output.write_text("should_release=false\n")
            print(f"Release {tag} is already published.")
            return
    elif "HTTP 404" not in release.stderr:
        raise SystemExit(release.stderr)

    tag_exists = tag in git("tag", "--list", tag).splitlines()
    if tag_exists:
        git("checkout", "--detach", f"refs/tags/{tag}")
    source_sha = git("rev-parse", "HEAD")
    previous_tags = git("tag", "--merged", source_sha, "--list", CALENDAR_TAG, "--sort=-version:refname")
    previous = next((value for value in previous_tags.splitlines() if value < tag), None)
    range_spec = f"{previous}..{source_sha}" if previous else source_sha
    notes = release_notes(range_spec, repo_url)
    if not notes:
        output.write_text("should_release=false\n")
        print("No releasable commits.")
        return
    Path("release_notes.md").write_text(f"## Changes\n\n{notes}\n")

    if not tag_exists:
        manifest = Path("Cargo.toml")
        updated, count = re.subn(
            r'(\[package\][\s\S]*?\nversion\s*=\s*")([^"]+)(")',
            rf"\g<1>{version}\g<3>", manifest.read_text(), count=1,
        )
        if count != 1:
            raise SystemExit("Failed to update [package] version in Cargo.toml")
        manifest.write_text(updated)
        changelog = Path("CHANGELOG.md")
        existing = changelog.read_text() if changelog.exists() else "# Changelog\n"
        remainder = existing.removeprefix("# Changelog").lstrip("\n")
        changelog.write_text(f"# Changelog\n\n## {tag}\n\n{notes}\n\n{remainder}".rstrip() + "\n")
        # Cargo updates this package's version while retaining locked dependency
        # versions. generate-lockfile would unnecessarily upgrade dependencies.
        subprocess.run(["cargo", "check"], check=True)

    output.write_text(
        f"should_release=true\nrelease_tag={tag}\ncargo_version={version}\n"
        f"source_sha={source_sha}\ntag_exists={str(tag_exists).lower()}\n"
    )


if __name__ == "__main__":
    main()
