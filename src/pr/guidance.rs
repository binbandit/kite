//! Bounded discovery of repository PR templates and dedicated writing skills.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::ai::truncate_for_prompt;

const MAX_SKILLS: usize = 3;
const MAX_SKILL_BYTES: usize = 4_000;
const MAX_TEMPLATE_BYTES: usize = 6_000;

/// A named piece of guidance (a PR template or an agent skill) fed to the AI.
pub(super) struct Guidance {
    pub(super) label: String,
    pub(super) content: String,
}

/// Finds the repository's pull request template in the places GitHub looks:
/// the root, `.github/`, and `docs/`, plus the `.github/PULL_REQUEST_TEMPLATE/`
/// multi-template directory. Matching is case-insensitive.
pub(super) fn find_pr_template(root: &Path) -> Option<Guidance> {
    for dir in [root.to_path_buf(), root.join(".github"), root.join("docs")] {
        if let Some(path) = find_entry_case_insensitive(&dir, "pull_request_template.md")
            && path.is_file()
        {
            return read_guidance(root, &path, MAX_TEMPLATE_BYTES);
        }
    }

    let template_dir = find_entry_case_insensitive(&root.join(".github"), "PULL_REQUEST_TEMPLATE")
        .filter(|path| path.is_dir())?;
    let mut templates: Vec<PathBuf> = std::fs::read_dir(template_dir)
        .ok()?
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("md"))
        })
        .collect();
    templates.sort();
    read_guidance(root, templates.first()?, MAX_TEMPLATE_BYTES)
}

/// Project guidance precedes user guidance; the first skill of each name wins.
/// Only immediate SKILL.md files are read; referenced documents and nested
/// skill directories are not followed.
pub(super) fn find_pr_skills(root: &Path) -> Vec<Guidance> {
    let project_dirs: Vec<PathBuf> = [
        ".claude/skills",
        ".codex/skills",
        ".agents/skills",
        "skills",
    ]
    .iter()
    .map(|directory| root.join(directory))
    .collect();
    let user_dirs: Vec<PathBuf> = std::env::home_dir()
        .map(|home| {
            [".claude/skills", ".codex/skills", ".agents/skills"]
                .iter()
                .map(|directory| home.join(directory))
                .collect()
        })
        .unwrap_or_default();
    find_pr_skills_in(&project_dirs, &user_dirs)
}

fn find_pr_skills_in(project_dirs: &[PathBuf], user_dirs: &[PathBuf]) -> Vec<Guidance> {
    let mut seen = HashSet::new();
    let mut skills = Vec::new();

    for (scope, directories) in [("project", project_dirs), ("user", user_dirs)] {
        for dir in directories {
            let Ok(entries) = std::fs::read_dir(dir) else {
                continue;
            };
            let mut skill_homes: Vec<PathBuf> =
                entries.flatten().map(|entry| entry.path()).collect();
            skill_homes.sort();

            for skill_home in skill_homes {
                let Some(name) = skill_home.file_name().and_then(|name| name.to_str()) else {
                    continue;
                };
                let Some(manifest) = find_entry_case_insensitive(&skill_home, "SKILL.md") else {
                    continue;
                };
                let Ok(content) = std::fs::read_to_string(&manifest) else {
                    continue;
                };
                if !seen.insert(name.to_ascii_lowercase()) || !is_pr_writing_skill(name, &content) {
                    continue;
                }
                skills.push(Guidance {
                    label: format!("{scope}: {name}"),
                    content: truncate_for_prompt(&content, MAX_SKILL_BYTES).to_string(),
                });
                if skills.len() == MAX_SKILLS {
                    return skills;
                }
            }
        }
    }
    skills
}

/// Match a dedicated writing purpose, rather than incidental references to PRs
/// in operational skills such as use-kite, babysit-pr, or release automation.
fn is_pr_writing_skill(name: &str, content: &str) -> bool {
    const ACTIONS: &[&str] = &[
        "write",
        "writing",
        "draft",
        "drafting",
        "create",
        "creating",
        "open",
        "opening",
        "file",
        "filing",
        "compose",
        "composing",
        "format",
        "formatting",
    ];
    const SUBJECTS: &[&str] = &[
        "title",
        "titles",
        "body",
        "bodies",
        "description",
        "descriptions",
        "template",
        "templates",
        "guidance",
        "style",
        "etiquette",
    ];
    let words = |text: &str| -> Vec<String> {
        text.split(|c: char| !c.is_ascii_alphanumeric())
            .filter(|word| !word.is_empty())
            .map(str::to_ascii_lowercase)
            .collect()
    };
    let pr_position = |words: &[String]| {
        words.iter().enumerate().position(|(index, word)| {
            word == "pr"
                || word == "prs"
                || (word == "pull"
                    && words
                        .get(index + 1)
                        .is_some_and(|next| next == "request" || next == "requests"))
        })
    };
    let name = words(name);
    if pr_position(&name).is_some()
        && name
            .iter()
            .any(|word| ACTIONS.contains(&word.as_str()) || SUBJECTS.contains(&word.as_str()))
    {
        return true;
    }

    let description = words(&skill_description(content));
    let Some(pr) = pr_position(&description).filter(|position| *position < 12) else {
        return false;
    };
    description[pr.saturating_sub(6)..pr]
        .iter()
        .any(|word| ACTIONS.contains(&word.as_str()) || SUBJECTS.contains(&word.as_str()))
        || description
            .iter()
            .skip(pr)
            .take(4)
            .any(|word| SUBJECTS.contains(&word.as_str()))
}

fn skill_description(content: &str) -> String {
    let mut lines = content.lines();
    if lines.next().is_some_and(|line| line.trim() == "---") {
        while let Some(line) = lines.next() {
            if line.trim() == "---" {
                break;
            }
            if let Some(description) = line.strip_prefix("description:") {
                let description = description.trim();
                if matches!(description, ">" | "|" | ">-" | "|-") {
                    return lines
                        .take_while(|line| line.starts_with(char::is_whitespace))
                        .map(str::trim)
                        .collect::<Vec<_>>()
                        .join(" ");
                }
                return description.trim_matches(['\'', '"']).to_string();
            }
        }
        return String::new();
    }
    content
        .lines()
        .skip_while(|line| line.trim().is_empty() || line.starts_with('#'))
        .take_while(|line| !line.trim().is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

fn find_entry_case_insensitive(dir: &Path, name: &str) -> Option<PathBuf> {
    std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .map(|entry| entry.path())
        .find(|path| {
            path.file_name()
                .and_then(|file_name| file_name.to_str())
                .is_some_and(|file_name| file_name.eq_ignore_ascii_case(name))
        })
}

fn read_guidance(root: &Path, path: &Path, max_bytes: usize) -> Option<Guidance> {
    let content = std::fs::read_to_string(path).ok()?;
    let label = path
        .strip_prefix(root)
        .unwrap_or(path)
        .display()
        .to_string();

    Some(Guidance {
        label,
        content: truncate_for_prompt(&content, max_bytes).to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{TempDir, write_file};

    #[test]
    fn find_pr_template_matches_github_locations_case_insensitively() {
        let dir = TempDir::new("kite-pr-template");
        write_file(
            &dir.path,
            ".github/PULL_REQUEST_TEMPLATE.md",
            "## Summary\n",
        );

        let template = find_pr_template(&dir.path).expect("template should be found");
        assert_eq!(template.label, ".github/PULL_REQUEST_TEMPLATE.md");
        assert_eq!(template.content, "## Summary\n");
    }

    #[test]
    fn find_pr_template_falls_back_to_multi_template_directory() {
        let dir = TempDir::new("kite-pr-template-dir");
        write_file(
            &dir.path,
            ".github/PULL_REQUEST_TEMPLATE/bugfix.md",
            "## Bugfix\n",
        );
        write_file(
            &dir.path,
            ".github/PULL_REQUEST_TEMPLATE/feature.md",
            "## Feature\n",
        );

        let template = find_pr_template(&dir.path).expect("template should be found");
        assert!(template.label.ends_with("bugfix.md"));
        assert_eq!(template.content, "## Bugfix\n");
    }

    #[test]
    fn find_pr_template_returns_none_without_templates() {
        let dir = TempDir::new("kite-pr-no-template");
        assert!(find_pr_template(&dir.path).is_none());
    }

    #[test]
    fn find_pr_skills_picks_pull_request_skills_and_dedupes_by_name() {
        let dir = TempDir::new("kite-pr-skills");
        let project = dir.path.join(".claude/skills");
        let user = dir.path.join("home/.claude/skills");

        write_file(
            &dir.path,
            ".claude/skills/write-prs/SKILL.md",
            "---\nname: write-prs\ndescription: Guidance for pull requests.\n---\nAlways link issues.",
        );
        write_file(
            &dir.path,
            "home/.claude/skills/write-prs/SKILL.md",
            "Stale duplicate that must lose to the project copy.",
        );
        write_file(
            &dir.path,
            "home/.claude/skills/unrelated/SKILL.md",
            "---\nname: unrelated\ndescription: Formats SQL.\n---",
        );

        let skills = find_pr_skills_in(&[project], &[user]);

        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].label, "project: write-prs");
        assert!(skills[0].content.contains("Always link issues."));
    }

    #[test]
    fn is_pr_writing_skill_checks_name_tokens_and_content() {
        assert!(is_pr_writing_skill("write-prs", ""));
        assert!(!is_pr_writing_skill("pr-helper", ""));
        assert!(is_pr_writing_skill("create-pull-request", ""));
        assert!(is_pr_writing_skill("Pull_Request_Titles", ""));
        assert!(is_pr_writing_skill(
            "shipit",
            "Use this when opening a Pull Request."
        ));
        assert!(is_pr_writing_skill("shipit", "pull-request etiquette"));
        assert!(!is_pr_writing_skill("sprint-notes", "Formats SQL."));
        assert!(!is_pr_writing_skill("prettier-config", ""));
    }

    #[test]
    fn is_pr_writing_skill_ignores_body_only_mentions() {
        let body_only = "---\nname: shipit\ndescription: Release automation.\n---\nStep 9: also open a pull request.";
        assert!(!is_pr_writing_skill("shipit", body_only));
    }

    #[test]
    fn operational_skills_do_not_crowd_out_writing_guidance() {
        assert!(!is_pr_writing_skill(
            "use-kite",
            include_str!("../../skills/use-kite/SKILL.md")
        ));
        assert!(!is_pr_writing_skill(
            "babysit-pr",
            "---\ndescription: Monitor a pull request through review and CI.\n---"
        ));
        assert!(is_pr_writing_skill(
            "team-style",
            "---\ndescription: >\n  Write concise pull request descriptions\n  for this repository.\n---"
        ));
    }

    #[test]
    fn project_codex_guidance_precedes_user_skills() {
        let dir = TempDir::new("kite-project-codex-skills");
        write_file(
            &dir.path,
            ".codex/skills/pr-writing/SKILL.md",
            "Use sentence case titles.",
        );
        let skills = find_pr_skills(&dir.path);
        assert_eq!(skills[0].label, "project: pr-writing");
        assert!(skills[0].content.contains("sentence case"));
    }
}
