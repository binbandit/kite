//! Changed paths and their diff, used to group whole files into commits.
//!
//! Paths come from Git's separate, unambiguous path list. Parsing paths from
//! diff headers would mishandle Git's quoting of unusual filenames.

use std::ops::Range;

pub(crate) const MAX_DIFF_BYTES: usize = 60_000;

const TRIMMED: &str = "(trimmed)\n";

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

    /// Shares the byte budget across files so later changes get context too.
    pub(crate) fn render_diff(&self, budget: usize) -> String {
        if self.diff.len() <= budget {
            return self.diff.clone();
        }

        let sections = section_ranges(&self.diff);
        let mut rendered = String::with_capacity(budget);
        if sections.is_empty() {
            push_capped(&mut rendered, &self.diff, budget);
            return rendered;
        }

        let cap = budget / sections.len();
        for section in sections {
            push_capped(&mut rendered, &self.diff[section], cap);
        }
        rendered
    }
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

/// Appends `text`, cut at a line boundary once it exceeds `cap` bytes.
fn push_capped(out: &mut String, text: &str, cap: usize) {
    if text.len() <= cap {
        out.push_str(text);
        return;
    }
    let Some(body_cap) = cap.checked_sub(TRIMMED.len()) else {
        return;
    };
    let mut used = 0;
    for line in text.split_inclusive('\n') {
        if used + line.len() > body_cap {
            break;
        }
        out.push_str(line);
        used += line.len();
    }
    out.push_str(TRIMMED);
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

    #[test]
    fn render_diff_shares_the_budget_across_files() {
        let long_change: String = (0..60)
            .map(|line| format!("+line {line} padded to take up space\n"))
            .collect();
        let diff = format!(
            "diff --git a/big.txt b/big.txt\n\
             index 1111111..2222222 100644\n\
             --- a/big.txt\n\
             +++ b/big.txt\n\
             @@ -0,0 +1,60 @@\n{long_change}\
             diff --git a/small.txt b/small.txt\n\
             index 3333333..4444444 100644\n\
             --- a/small.txt\n\
             +++ b/small.txt\n\
             @@ -1,1 +1,2 @@\n one\n+two\n"
        );
        let files = ChangedFiles::new(
            vec!["big.txt".to_string(), "small.txt".to_string()],
            diff.clone(),
        );

        let trimmed = files.render_diff(1_000);
        assert!(trimmed.len() <= 1_000);
        assert!(trimmed.contains("diff --git a/big.txt b/big.txt"));
        assert!(trimmed.contains("(trimmed)"));
        // The tail file keeps its content rather than being cut off entirely.
        assert!(trimmed.contains("diff --git a/small.txt b/small.txt"));
        assert!(trimmed.contains("+two"));
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

    #[test]
    fn an_empty_diff_renders_nothing() {
        let files = ChangedFiles::new(Vec::new(), String::new());

        assert!(files.paths().is_empty());
        assert_eq!(files.render_diff(usize::MAX), "");
    }
}
