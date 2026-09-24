//! Changed paths and their diff, fitted to the AI prompt budget for commit
//! planning and PR drafts.
//!
//! Paths come from Git's separate, unambiguous path list. Parsing paths from
//! diff headers would mishandle Git's quoting of unusual filenames.

use std::ops::Range;

/// About 40k tokens. Even compacted, a large land needs most of this: at 60 KB
/// the telling lines of its fixes were still cut, and the model named them
/// refactors.
pub(crate) const MAX_DIFF_BYTES: usize = 160_000;

// The rendered diff explains its own omissions, so prompts don't have to
// describe how this module trims.
const COMPACTED: &str = "(compacted: unchanged context and index lines omitted)\n";
const TRIMMED: &str = "(trimmed: remaining changed lines omitted)\n";

/// The files a set of saves changed, and the diff describing them.
#[derive(Clone, Debug)]
pub(crate) struct ChangedFiles {
    paths: Vec<String>,
    diff: String,
}

impl ChangedFiles {
    pub(crate) fn new(paths: Vec<String>, diff: String) -> Self {
        Self { paths, diff }
    }

    /// Every changed path, in diff order.
    pub(crate) fn paths(&self) -> &[String] {
        &self.paths
    }

    pub(crate) fn render_diff(&self, budget: usize) -> String {
        render_diff(&self.diff, budget)
    }
}

/// Fits a diff into `budget` bytes while keeping evidence of every change.
///
/// A diff that fits is returned whole. Otherwise each file is compacted to
/// what describes the change: unchanged context and `index`/`---`/`+++`
/// lines go first, hunk headers (which name the enclosing function) stay.
/// Files that still fit take what they need and larger ones share the rest,
/// and a file over its share shows the start of every hunk rather than
/// only its first ones. Cutting whole lines from the top would leave a
/// large change represented by its import block.
pub(crate) fn render_diff(diff: &str, budget: usize) -> String {
    if diff.len() <= budget {
        return diff.to_string();
    }

    let mut ranges = section_ranges(diff);
    // Text without file headers is still cut at line boundaries, as one section.
    if ranges.is_empty() {
        ranges.push(0..diff.len());
    }
    let sections: Vec<Section> = ranges
        .into_iter()
        .map(|range| Section::parse(&diff[range]))
        .collect();

    let mut rendered = String::with_capacity(budget);
    let budget = match budget.checked_sub(COMPACTED.len()) {
        Some(rest) => {
            rendered.push_str(COMPACTED);
            rest
        }
        None => budget,
    };
    let sizes: Vec<usize> = sections.iter().map(Section::compact_len).collect();
    for ((section, &size), cap) in sections.iter().zip(&sizes).zip(fair_shares(&sizes, budget)) {
        section.render_into(&mut rendered, cap, size);
    }
    rendered
}

/// One file's part of a diff, reduced to the lines that describe its change.
struct Section<'a> {
    header: Vec<&'a str>,
    hunks: Vec<Hunk<'a>>,
}

struct Hunk<'a> {
    header: &'a str,
    changes: Vec<&'a str>,
}

impl<'a> Section<'a> {
    fn parse(text: &'a str) -> Self {
        let mut header = Vec::new();
        let mut hunks: Vec<Hunk> = Vec::new();
        for line in text.split_inclusive('\n') {
            if line.starts_with("@@") {
                hunks.push(Hunk {
                    header: line,
                    changes: Vec::new(),
                });
            } else if let Some(hunk) = hunks.last_mut() {
                // Unchanged context and "\ No newline" markers carry no change.
                if line.starts_with('+') || line.starts_with('-') {
                    hunk.changes.push(line);
                }
            } else if !(line.starts_with("index ")
                || line.starts_with("--- ")
                || line.starts_with("+++ "))
            {
                // The `diff --git` line names the file; mode, new/deleted and
                // binary notes stay because they are the whole change for some files.
                header.push(line);
            }
        }
        Self { header, hunks }
    }

    fn compact_len(&self) -> usize {
        let header: usize = self.header.iter().map(|line| line.len()).sum();
        let hunks: usize = self
            .hunks
            .iter()
            .map(|hunk| {
                hunk.header.len() + hunk.changes.iter().map(|line| line.len()).sum::<usize>()
            })
            .sum();
        header + hunks
    }

    /// Writes the compacted section (`size` bytes) within `cap`, spreading
    /// the budget across hunks so each one shows its opening changed lines.
    fn render_into(&self, out: &mut String, cap: usize, size: usize) {
        let trimmed = size > cap;
        let Some(mut remaining) = cap.checked_sub(if trimmed { TRIMMED.len() } else { 0 }) else {
            return;
        };

        // Headers before any changed line: hunk headers say where every
        // change is, even when there is no room to show what it is.
        let kept = self
            .header
            .iter()
            .copied()
            .chain(self.hunks.iter().map(|hunk| hunk.header))
            .take_while(|line| {
                let fits = line.len() <= remaining;
                if fits {
                    remaining -= line.len();
                }
                fits
            })
            .count();
        let kept_header = kept.min(self.header.len());
        let hunks = &self.hunks[..kept - kept_header];

        // One more line per hunk each round, until a hunk's next line doesn't fit.
        let mut shown = vec![0; hunks.len()];
        let mut open: Vec<usize> = (0..hunks.len()).collect();
        while !open.is_empty() {
            open.retain(|&index| match hunks[index].changes.get(shown[index]) {
                Some(line) if line.len() <= remaining => {
                    remaining -= line.len();
                    shown[index] += 1;
                    true
                }
                _ => false,
            });
        }

        for line in &self.header[..kept_header] {
            out.push_str(line);
        }
        for (hunk, count) in hunks.iter().zip(shown) {
            out.push_str(hunk.header);
            for line in &hunk.changes[..count] {
                out.push_str(line);
            }
        }
        if trimmed {
            out.push_str(TRIMMED);
        }
    }
}

/// Splits `budget` so no section gets more than it needs: small sections
/// are satisfied first and the remainder is shared evenly by larger ones.
fn fair_shares(sizes: &[usize], budget: usize) -> Vec<usize> {
    let mut order: Vec<usize> = (0..sizes.len()).collect();
    order.sort_by_key(|&index| sizes[index]);
    let mut shares = vec![0; sizes.len()];
    let mut remaining = budget;
    for (position, index) in order.into_iter().enumerate() {
        let share = sizes[index].min(remaining / (sizes.len() - position));
        shares[index] = share;
        remaining -= share;
    }
    shares
}

/// Byte range of each `diff --git` section. Content lines always carry a
/// ' ', '+', '-', or '\' prefix, so a line starting at column zero with
/// `diff --git ` is a section header and never file content.
fn section_ranges(diff: &str) -> Vec<Range<usize>> {
    let mut sections: Vec<Range<usize>> = Vec::new();
    let mut offset = 0;

    for line in diff.split_inclusive('\n') {
        if line.starts_with("diff --git ") {
            if let Some(previous) = sections.last_mut() {
                previous.end = offset;
            }
            sections.push(offset..diff.len());
        }
        offset += line.len();
    }

    sections
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
@@ -10,3 +11,3 @@ fn ten()
 ten
-eleven
+ELEVEN
diff --git a/docs/b.md b/docs/b.md
index 3333333..4444444 100644
--- a/docs/b.md
+++ b/docs/b.md
@@ -1,1 +1,2 @@
 title
+more
";

    fn sample() -> ChangedFiles {
        ChangedFiles::new(
            vec!["src/a.rs".to_string(), "docs/b.md".to_string()],
            SAMPLE.to_string(),
        )
    }

    #[test]
    fn changed_files_expose_their_paths_and_diff() {
        let files = sample();

        assert_eq!(files.paths(), ["src/a.rs", "docs/b.md"]);
        assert_eq!(files.render_diff(usize::MAX), SAMPLE);
    }

    #[test]
    fn sections_cover_the_diff_exactly_once() {
        let sections = section_ranges(SAMPLE);

        assert_eq!(sections.len(), 2);
        let stitched: String = sections
            .iter()
            .map(|section| &SAMPLE[section.clone()])
            .collect();
        assert_eq!(stitched, SAMPLE);
        // Both of src/a.rs's hunks belong to its one section.
        assert_eq!(SAMPLE[sections[0].clone()].matches("@@ -").count(), 2);
    }

    /// The parser's one real assumption: content lines always carry a ' ',
    /// '+', '-', or '\\' prefix, so only a header sits at column zero.
    #[test]
    fn a_content_line_that_looks_like_a_header_does_not_split_a_section() {
        let diff = "\
diff --git a/notes.txt b/notes.txt
index 1111111..2222222 100644
--- a/notes.txt
+++ b/notes.txt
@@ -1 +1,3 @@
 keep
+diff --git a/fake b/fake
+ diff --git a/also-fake b/also-fake
";
        let sections = section_ranges(diff);

        assert_eq!(sections.len(), 1);
        assert_eq!(&diff[sections[0].clone()], diff);
    }

    #[test]
    fn crlf_sections_keep_their_bytes() {
        let diff = "diff --git a/win.txt b/win.txt\r\nindex 1111111..2222222 100644\r\n--- a/win.txt\r\n+++ b/win.txt\r\n@@ -1,2 +1,2 @@\r\n-one\r\n+ONE\r\n two\r\n";
        let files = ChangedFiles::new(vec!["win.txt".to_string()], diff.to_string());

        assert_eq!(section_ranges(diff).len(), 1);
        assert_eq!(files.render_diff(usize::MAX), diff);
        assert!(files.render_diff(8).len() <= 8);
    }

    /// A mode change produces a section with no hunks at all.
    #[test]
    fn a_section_without_hunks_still_renders_whole() {
        let diff = "\
diff --git a/run.sh b/run.sh
old mode 100644
new mode 100755
";
        let files = ChangedFiles::new(vec!["run.sh".to_string()], diff.to_string());

        assert_eq!(section_ranges(diff).len(), 1);
        assert_eq!(files.render_diff(usize::MAX), diff);
    }

    #[test]
    fn render_diff_respects_even_tiny_budgets() {
        let body: String = (0..40)
            .map(|line| format!("+padding line {line} with a good deal of text\n"))
            .collect();
        let diff = format!(
            "diff --git a/one.txt b/one.txt\n@@ -0,0 +1,40 @@\n{body}\
             diff --git a/two.txt b/two.txt\n@@ -0,0 +1,40 @@\n{body}"
        );
        let files = ChangedFiles::new(
            vec!["one.txt".to_string(), "two.txt".to_string()],
            diff.clone(),
        );

        for budget in [0, 1, 8, 10, 100, 400, 1_000] {
            assert!(files.render_diff(budget).len() <= budget);
        }
    }

    #[test]
    fn quoted_and_binary_headers_need_no_path_parsing() {
        // git C-quotes these paths in the header; the path list carries the
        // real names, so the diff text is only ever shown, never parsed.
        let diff = "\
diff --git \"a/new\\nline.txt\" \"b/new\\nline.txt\"
index 1111111..2222222 100644
--- \"a/new\\nline.txt\"
+++ \"b/new\\nline.txt\"
@@ -1 +1 @@
-old
+new
diff --git a/logo.png b/logo.png
index 3333333..4444444 100644
Binary files a/logo.png and b/logo.png differ
";
        let files = ChangedFiles::new(
            vec!["new\nline.txt".to_string(), "logo.png".to_string()],
            diff.to_string(),
        );

        assert_eq!(files.paths(), ["new\nline.txt", "logo.png"]);
        assert_eq!(section_ranges(diff).len(), 2);
    }

    /// A large change used to be represented by its first lines, which in
    /// most source files are imports. Every hunk must show up instead.
    #[test]
    fn an_over_budget_file_shows_the_start_of_every_hunk() {
        let new_tests: String = (0..80)
            .map(|line| format!("+    assert_eq!(check({line}), Ok(()));\n"))
            .collect();
        let diff = format!(
            "diff --git a/src/done.rs b/src/done.rs\n\
             index 1111111..2222222 100644\n\
             --- a/src/done.rs\n\
             +++ b/src/done.rs\n\
             @@ -1,4 +1,4 @@\n use std::fs;\n-use crate::model::Record;\n+use crate::model::{{Record, Target}};\n use crate::ui;\n\
             @@ -40,5 +40,5 @@ pub fn run() {{\n     let found = state\n-        .find(|w| w.branch == target.branch)\n+        .workspace_for_target(&target)\n     .map(path);\n\
             @@ -90,2 +90,82 @@ mod tests {{\n+fn done_never_returns_another_pull_request() {{\n{new_tests}"
        );
        let files = ChangedFiles::new(vec!["src/done.rs".to_string()], diff);

        let rendered = files.render_diff(700);
        assert!(rendered.len() <= 700, "{}", rendered.len());
        assert!(rendered.contains("@@ -40,5 +40,5 @@ pub fn run() {"));
        assert!(rendered.contains("+        .workspace_for_target(&target)"));
        assert!(rendered.contains("+fn done_never_returns_another_pull_request() {"));
        assert!(rendered.starts_with(COMPACTED));
        assert!(rendered.ends_with(TRIMMED));
        // Context and index lines are the first to go.
        assert!(!rendered.contains("index 1111111"));
        assert!(!rendered.contains(" use std::fs;"));
    }

    #[test]
    fn small_files_leave_their_unused_budget_to_large_ones() {
        let body: String = (0..100).map(|line| format!("+line {line}\n")).collect();
        let diff = format!(
            "diff --git a/big.txt b/big.txt\n@@ -0,0 +1,100 @@\n{body}\
             diff --git a/a.txt b/a.txt\n@@ -1 +1 @@\n-a\n+b\n\
             diff --git a/c.txt b/c.txt\n@@ -1 +1 @@\n-c\n+d\n"
        );
        let files = ChangedFiles::new(
            vec![
                "big.txt".to_string(),
                "a.txt".to_string(),
                "c.txt".to_string(),
            ],
            diff,
        );

        let rendered = files.render_diff(600);
        assert!(rendered.len() <= 600);
        assert!(rendered.contains("-a\n+b\n") && rendered.contains("-c\n+d\n"));
        // An even three-way split would give big.txt 200 bytes; it gets the rest.
        let big = &rendered[..rendered.find("diff --git a/a.txt").unwrap()];
        assert!(big.len() > 450, "{}", big.len());
    }

    #[test]
    fn fair_shares_never_exceed_need_or_budget() {
        assert_eq!(fair_shares(&[10, 500, 20], 300), vec![10, 270, 20]);
        assert_eq!(fair_shares(&[500, 500], 300), vec![150, 150]);
        assert_eq!(fair_shares(&[5, 5], 300), vec![5, 5]);
        assert_eq!(fair_shares(&[], 300), Vec::<usize>::new());
    }

    #[test]
    fn an_empty_diff_renders_nothing() {
        let files = ChangedFiles::new(Vec::new(), String::new());

        assert!(files.paths().is_empty());
        assert_eq!(files.render_diff(usize::MAX), "");
    }
}
