//! Splits a git diff into addressable hunks so synthesis can route changes
//! from one file into different commits, and landing can stage exactly what
//! each commit needs.
//!
//! Each unit is a single hunk where possible; binary content, mode changes,
//! and header-only diffs become one whole-file unit. Raw patch text is kept
//! verbatim, so assembling a commit's patch is pure concatenation — never
//! reconstruction.

use std::collections::HashSet;

/// Floor for each unit's share of the prompt body budget.
const MIN_UNIT_BODY_BYTES: usize = 400;

/// How much of a file a commit takes, for plan rendering.
#[derive(Clone, Debug)]
pub(crate) struct FileStat {
    pub(crate) path: String,
    pub(crate) selected: usize,
    pub(crate) total: usize,
    /// Headings of the selected hunks, filled only when the file is split
    /// across commits, so the plan shows which parts land where.
    pub(crate) headings: Vec<String>,
}

impl FileStat {
    pub(crate) fn whole(path: String) -> Self {
        Self {
            path,
            selected: 1,
            total: 1,
            headings: Vec::new(),
        }
    }
}

/// One `diff --git` section: the raw header block (including binary
/// payloads, which never contain hunk markers) and the raw hunks beneath it.
#[derive(Clone, Debug)]
struct FileSection {
    path: String,
    header: String,
    hunks: Vec<String>,
    /// Whole-file unit: binary content, mode changes, or header-only diffs.
    atomic: bool,
    /// Index of this file's first unit in the flat h1..hN sequence.
    first_unit: usize,
}

impl FileSection {
    fn unit_count(&self) -> usize {
        if self.atomic { 1 } else { self.hunks.len() }
    }

    fn is_binary(&self) -> bool {
        self.header.contains("GIT binary patch") || self.header.contains("Binary files ")
    }

    fn label(&self) -> &'static str {
        if self.is_binary() {
            "(whole file: binary)"
        } else if self.has_header_line("new file mode") {
            "(whole file: added)"
        } else if self.has_header_line("deleted file mode") {
            "(whole file: deleted)"
        } else {
            "(whole file)"
        }
    }

    fn has_header_line(&self, prefix: &str) -> bool {
        self.header.lines().any(|line| line.starts_with(prefix))
    }
}

#[derive(Clone, Debug)]
pub(crate) struct DiffUnits {
    files: Vec<FileSection>,
}

fn unit_id(index: usize) -> String {
    format!("h{}", index + 1)
}

/// Appends `body`, cut at a line boundary once it exceeds `cap` bytes.
fn push_capped(out: &mut String, body: &str, cap: usize) {
    if body.len() <= cap {
        out.push_str(body);
        return;
    }
    let mut used = 0;
    for line in body.lines() {
        if used + line.len() + 1 > cap {
            break;
        }
        out.push_str(line);
        out.push('\n');
        used += line.len() + 1;
    }
    out.push_str("(trimmed)\n");
}

pub(crate) fn parse_diff(diff: &str) -> DiffUnits {
    let mut raw_files: Vec<(String, Vec<String>)> = Vec::new();

    for line in diff.lines() {
        if line.starts_with("diff --git ") {
            raw_files.push((format!("{line}\n"), Vec::new()));
            continue;
        }
        let Some((header, hunks)) = raw_files.last_mut() else {
            continue;
        };
        // Hunk body lines only start with ' ', '+', '-', or '\', so these
        // two prefixes are unambiguous section markers.
        if line.starts_with("@@ -") {
            hunks.push(format!("{line}\n"));
        } else if let Some(hunk) = hunks.last_mut() {
            hunk.push_str(line);
            hunk.push('\n');
        } else {
            header.push_str(line);
            header.push('\n');
        }
    }

    let mut files = Vec::new();
    let mut next_unit = 0;
    for (header, hunks) in raw_files {
        let file = FileSection {
            path: extract_path(&header),
            atomic: hunks.is_empty() || has_unsplittable_marker(&header),
            header,
            hunks,
            first_unit: next_unit,
        };
        next_unit += file.unit_count();
        files.push(file);
    }

    DiffUnits { files }
}

/// Mode changes and renames must stay whole: replaying their header once per
/// split hunk group would not apply cleanly.
fn has_unsplittable_marker(header: &str) -> bool {
    header.lines().any(|line| {
        line.starts_with("old mode ")
            || line.starts_with("new mode ")
            || line.starts_with("rename from ")
            || line.starts_with("copy from ")
    })
}

/// Path for display and grouping. Staging uses the raw patch text, so a
/// best-effort answer here can never corrupt a landing.
fn extract_path(header: &str) -> String {
    for line in header.lines() {
        if let Some(path) = line.strip_prefix("+++ b/") {
            return path.trim_end_matches('\t').to_string();
        }
    }
    for line in header.lines() {
        if let Some(path) = line.strip_prefix("--- a/") {
            return path.trim_end_matches('\t').to_string();
        }
    }

    // Binary diffs carry no ---/+++ lines; recover the path from the
    // symmetric `diff --git a/<path> b/<path>` line.
    let first = header.lines().next().unwrap_or("");
    let Some(rest) = first.strip_prefix("diff --git a/") else {
        return first.to_string();
    };
    let bytes = rest.as_bytes();
    if bytes.len() >= 3 {
        let mid = (bytes.len() - 3) / 2;
        if &bytes[mid..mid + 3] == b" b/" && bytes[..mid] == bytes[mid + 3..] {
            return rest[..mid].to_string();
        }
    }
    rest.rfind(" b/")
        .map(|pos| rest[pos + 3..].to_string())
        .unwrap_or_else(|| rest.to_string())
}

impl DiffUnits {
    pub(crate) fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    /// Every assignable hunk id, in diff order.
    pub(crate) fn unit_ids(&self) -> Vec<String> {
        self.files
            .iter()
            .flat_map(|file| (0..file.unit_count()).map(|offset| unit_id(file.first_unit + offset)))
            .collect()
    }

    pub(crate) fn all_files(&self) -> Vec<String> {
        self.files.iter().map(|file| file.path.clone()).collect()
    }

    /// Compact `- <id> <path> <summary>` list. Always included whole in the
    /// prompt, so the model sees every id even when contents get truncated.
    pub(crate) fn render_unit_index(&self) -> String {
        let mut index = String::new();
        for file in &self.files {
            if file.atomic {
                index.push_str(&format!(
                    "- {} {} {}\n",
                    unit_id(file.first_unit),
                    file.path,
                    file.label()
                ));
            } else {
                for (offset, hunk) in file.hunks.iter().enumerate() {
                    let heading = hunk.lines().next().unwrap_or("").trim();
                    index.push_str(&format!(
                        "- {} {} {}\n",
                        unit_id(file.first_unit + offset),
                        file.path,
                        heading
                    ));
                }
            }
        }
        index
    }

    /// Hunk bodies for the prompt, binary payloads elided. When the full
    /// render exceeds `budget`, every unit gets a fair share of it instead of
    /// the tail units losing their bodies entirely — a hunk the model never
    /// saw is a hunk it forgets to assign.
    pub(crate) fn render_unit_bodies(&self, budget: usize) -> String {
        let full = self.render_bodies_capped(usize::MAX);
        if full.len() <= budget {
            return full;
        }
        let per_unit = (budget / self.unit_ids().len().max(1)).max(MIN_UNIT_BODY_BYTES);
        self.render_bodies_capped(per_unit)
    }

    fn render_bodies_capped(&self, cap: usize) -> String {
        let mut bodies = String::new();
        for file in &self.files {
            if file.atomic {
                bodies.push_str(&format!(
                    "[{}] {} {}\n",
                    unit_id(file.first_unit),
                    file.path,
                    file.label()
                ));
                if !file.is_binary() {
                    push_capped(&mut bodies, &file.hunks.concat(), cap);
                }
            } else {
                for (offset, hunk) in file.hunks.iter().enumerate() {
                    bodies.push_str(&format!(
                        "[{}] {}\n",
                        unit_id(file.first_unit + offset),
                        file.path
                    ));
                    push_capped(&mut bodies, hunk, cap);
                }
            }
            bodies.push('\n');
        }
        bodies
    }

    /// Patch containing exactly the selected units, in original diff order.
    pub(crate) fn assemble_patch(&self, ids: &HashSet<String>) -> String {
        let mut patch = String::new();
        for file in &self.files {
            let hunks: Vec<&String> = if file.atomic {
                if !ids.contains(&unit_id(file.first_unit)) {
                    continue;
                }
                file.hunks.iter().collect()
            } else {
                let selected: Vec<&String> = file
                    .hunks
                    .iter()
                    .enumerate()
                    .filter(|(offset, _)| ids.contains(&unit_id(file.first_unit + offset)))
                    .map(|(_, hunk)| hunk)
                    .collect();
                if selected.is_empty() {
                    continue;
                }
                selected
            };

            patch.push_str(&file.header);
            for hunk in hunks {
                patch.push_str(hunk);
            }
        }
        patch
    }

    /// Per-file selection counts for the selected ids, in diff order.
    pub(crate) fn file_stats(&self, ids: &HashSet<String>) -> Vec<FileStat> {
        let mut stats = Vec::new();
        for file in &self.files {
            let total = file.unit_count();
            let selected: Vec<usize> = (0..total)
                .filter(|offset| ids.contains(&unit_id(file.first_unit + offset)))
                .collect();
            if selected.is_empty() {
                continue;
            }
            let headings = if selected.len() < total {
                selected
                    .iter()
                    .map(|&offset| hunk_heading(&file.hunks[offset]))
                    .collect()
            } else {
                Vec::new()
            };
            stats.push(FileStat {
                path: file.path.clone(),
                selected: selected.len(),
                total,
                headings,
            });
        }
        stats
    }

    pub(crate) fn files_for(&self, ids: &HashSet<String>) -> Vec<String> {
        self.file_stats(ids)
            .into_iter()
            .map(|stat| stat.path)
            .collect()
    }

    /// File a unit id belongs to; `None` for ids that don't match `h<n>`.
    pub(crate) fn path_for(&self, id: &str) -> Option<&str> {
        let index = id
            .strip_prefix('h')?
            .parse::<usize>()
            .ok()?
            .checked_sub(1)?;
        self.files
            .iter()
            .find(|file| index >= file.first_unit && index < file.first_unit + file.unit_count())
            .map(|file| file.path.as_str())
    }
}

/// The function context git puts after the `@@` markers, or the raw `@@`
/// line when there is none.
fn hunk_heading(hunk: &str) -> String {
    let header = hunk.lines().next().unwrap_or("");
    match header.rsplit_once("@@") {
        Some((_, context)) if !context.trim().is_empty() => context.trim().to_string(),
        _ => header.trim().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\
diff --git a/src/a.rs b/src/a.rs
index 1111111..2222222 100644
--- a/src/a.rs
+++ b/src/a.rs
@@ -1,3 +1,4 @@
 one
+two
 three
 four
@@ -10,3 +11,3 @@ fn ten()
 ten
-eleven
+ELEVEN
 twelve
diff --git a/docs/b.md b/docs/b.md
index 3333333..4444444 100644
--- a/docs/b.md
+++ b/docs/b.md
@@ -1,1 +1,2 @@
 title
+more
";

    fn ids(list: &[&str]) -> HashSet<String> {
        list.iter().map(|id| id.to_string()).collect()
    }

    #[test]
    fn parse_diff_assigns_sequential_ids_across_files() {
        let units = parse_diff(SAMPLE);

        assert_eq!(units.unit_ids(), vec!["h1", "h2", "h3"]);
        assert_eq!(units.all_files(), vec!["src/a.rs", "docs/b.md"]);

        let index = units.render_unit_index();
        assert!(index.contains("- h1 src/a.rs @@ -1,3 +1,4 @@"));
        assert!(index.contains("- h2 src/a.rs @@ -10,3 +11,3 @@ fn ten()"));
        assert!(index.contains("- h3 docs/b.md @@ -1,1 +1,2 @@"));
    }

    #[test]
    fn assemble_patch_keeps_headers_and_only_selected_hunks() {
        let units = parse_diff(SAMPLE);

        let patch = units.assemble_patch(&ids(&["h2"]));
        assert!(patch.starts_with("diff --git a/src/a.rs b/src/a.rs\n"));
        assert!(patch.contains("+ELEVEN"));
        assert!(!patch.contains("+two"));
        assert!(!patch.contains("docs/b.md"));

        let full = units.assemble_patch(&ids(&["h1", "h2", "h3"]));
        assert_eq!(full, SAMPLE);
    }

    #[test]
    fn file_stats_report_partial_selection() {
        let units = parse_diff(SAMPLE);

        let stats = units.file_stats(&ids(&["h1", "h3"]));
        assert_eq!(stats.len(), 2);
        assert_eq!(stats[0].path, "src/a.rs");
        assert_eq!((stats[0].selected, stats[0].total), (1, 2));
        assert_eq!(stats[1].path, "docs/b.md");
        assert_eq!((stats[1].selected, stats[1].total), (1, 1));

        assert_eq!(units.files_for(&ids(&["h2"])), vec!["src/a.rs"]);
    }

    #[test]
    fn binary_files_become_one_whole_file_unit() {
        let diff = "\
diff --git a/logo.png b/logo.png
index 0000000000000000000000000000000000000000..1111111111111111111111111111111111111111 100644
GIT binary patch
literal 5
Mcmd0

literal 0
Hc$@<O00001
";
        let units = parse_diff(diff);

        assert_eq!(units.unit_ids(), vec!["h1"]);
        assert!(
            units
                .render_unit_index()
                .contains("- h1 logo.png (whole file: binary)")
        );
        assert!(!units.render_unit_bodies(usize::MAX).contains("literal 5"));

        let patch = units.assemble_patch(&ids(&["h1"]));
        assert!(patch.contains("GIT binary patch"));
        assert!(patch.contains("literal 5"));
    }

    #[test]
    fn mode_changes_stay_whole_even_with_hunks() {
        let diff = "\
diff --git a/run.sh b/run.sh
old mode 100644
new mode 100755
index 1111111..2222222
--- a/run.sh
+++ b/run.sh
@@ -1,1 +1,2 @@
 echo hi
+set -e
";
        let units = parse_diff(diff);

        assert_eq!(units.unit_ids(), vec!["h1"]);
        assert!(
            units
                .render_unit_index()
                .contains("- h1 run.sh (whole file)")
        );

        let patch = units.assemble_patch(&ids(&["h1"]));
        assert!(patch.contains("new mode 100755"));
        assert!(patch.contains("+set -e"));
    }

    #[test]
    fn render_unit_bodies_shares_the_budget_across_units() {
        // One oversized hunk and one small one: with a tight budget the big
        // body gets trimmed while every unit keeps its header and a share.
        let long_hunk: String = (0..60)
            .map(|line| format!("+line {line} padded to take up space\n"))
            .collect();
        let diff = format!(
            "diff --git a/big.txt b/big.txt\n\
             index 1111111..2222222 100644\n\
             --- a/big.txt\n\
             +++ b/big.txt\n\
             @@ -0,0 +1,60 @@\n{long_hunk}\
             diff --git a/small.txt b/small.txt\n\
             index 3333333..4444444 100644\n\
             --- a/small.txt\n\
             +++ b/small.txt\n\
             @@ -1,1 +1,2 @@\n one\n+two\n"
        );
        let units = parse_diff(&diff);

        let full = units.render_unit_bodies(usize::MAX);
        assert!(!full.contains("(trimmed)"));

        let trimmed = units.render_unit_bodies(1_000);
        assert!(trimmed.contains("[h1] big.txt"));
        assert!(trimmed.contains("(trimmed)"));
        assert!(trimmed.contains("[h2] small.txt"));
        assert!(trimmed.contains("+two"));
    }

    #[test]
    fn extract_path_handles_deletions_and_spaces() {
        let deletion = "\
diff --git a/gone.txt b/gone.txt
deleted file mode 100644
index 1111111..0000000
--- a/gone.txt
+++ /dev/null
@@ -1,1 +0,0 @@
-bye
";
        let units = parse_diff(deletion);
        assert_eq!(units.all_files(), vec!["gone.txt"]);
        assert!(!units.is_empty());

        let spaced = "\
diff --git a/my file.txt b/my file.txt
index 1111111..2222222 100644
--- a/my file.txt\t
+++ b/my file.txt\t
@@ -1,1 +1,2 @@
 one
+two
";
        let units = parse_diff(spaced);
        assert_eq!(units.all_files(), vec!["my file.txt"]);

        let binary_spaced = "\
diff --git a/img dir/pic.png b/img dir/pic.png
index 1111111..2222222 100644
GIT binary patch
literal 3
Kcmd
";
        let units = parse_diff(binary_spaced);
        assert_eq!(units.all_files(), vec!["img dir/pic.png"]);
    }
}
