//! Plan a land that follows history. The saves are cut into windows of
//! consecutive saves; each window is planned on its own and lands as one or
//! more commits that end exactly on the window's last save.
//!
//! Windows keep every AI request bounded however long the history is, and
//! they are planned concurrently.

use std::future::Future;
use std::ops::Range;
use std::sync::Arc;

use anyhow::{Context, Result};
use chrono::{DateTime, Local};
use clap::ValueEnum;
use colored::*;

use crate::ai;
use crate::diff::{ChangedFiles, MAX_DIFF_BYTES};
use crate::git::{KiteBase, Save, changes_between, kite_saves};
use crate::synth::{
    CommitGroup, HistorySlice, commit_style_examples, describe_saves, normalize_groups,
    synthesize_slice,
};
use crate::ui::{Progress, Spinner, pluralize};

/// How finely `kt land` turns saves into commits.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, ValueEnum)]
pub(crate) enum Granularity {
    /// Plan all saved changes together: the fewest commits
    #[default]
    Coarse,
    /// Plan each window of consecutive saves, landing the windows in order
    Fine,
    /// Keep every save as its own commit, with a written message
    PerSave,
}

/// A pause this long usually separates one piece of work from the next.
const WINDOW_GAP_SECS: i64 = 30 * 60;
/// Without a pause, a window still closes at these sizes, counted over its
/// saves. They keep each request small enough to describe well: about the
/// most a diff can hold before compaction starts dropping changed lines.
const MAX_WINDOW_SAVES: usize = 12;
const MAX_WINDOW_LINES: usize = 5_000;
const MAX_WINDOW_FILES: usize = 300;
/// Saves whose messages are written in one request when every save keeps its
/// own commit. Their diffs share one request's budget.
const MAX_SAVES_PER_REQUEST: usize = 8;

/// The commits that take history from the previous window to this one's
/// last save.
#[derive(Clone, Debug)]
pub(super) struct WindowPlan {
    /// Positions of the window's saves among all the saves being landed.
    pub(super) saves: Range<usize>,
    /// The window's last save. The window's commits end on its exact tree.
    pub(super) save: String,
    pub(super) commits: Vec<CommitGroup>,
}

/// A land that follows history, window by window.
#[derive(Clone, Debug)]
pub(super) struct HistoryPlan {
    pub(super) granularity: Granularity,
    pub(super) saves: Vec<Save>,
    pub(super) windows: Vec<WindowPlan>,
}

impl HistoryPlan {
    pub(super) fn commits_mut(&mut self) -> impl Iterator<Item = &mut CommitGroup> {
        self.windows
            .iter_mut()
            .flat_map(|window| window.commits.iter_mut())
    }

    pub(super) fn commit_count(&self) -> usize {
        self.windows.iter().map(|window| window.commits.len()).sum()
    }
}

/// One window, read from history and ready to plan.
struct WindowChanges {
    saves: Range<usize>,
    save: String,
    files: ChangedFiles,
}

pub(super) async fn plan_history(
    base: &KiteBase,
    save_count: usize,
    granularity: Granularity,
) -> Result<HistoryPlan> {
    let saves = kite_saves(save_count)?;
    let windows = read_windows(base, &saves, granularity)?;
    let examples = Arc::new(commit_style_examples());
    let commits = match granularity {
        Granularity::Coarse | Granularity::Fine => group_each_window(&windows, &examples).await?,
        Granularity::PerSave => describe_each_save(&windows, &examples).await?,
    };

    let windows = windows
        .into_iter()
        .zip(commits)
        .map(|(window, commits)| WindowPlan {
            saves: window.saves,
            save: window.save,
            commits,
        })
        .collect();
    Ok(HistoryPlan {
        granularity,
        saves,
        windows,
    })
}

fn read_windows(
    base: &KiteBase,
    saves: &[Save],
    granularity: Granularity,
) -> Result<Vec<WindowChanges>> {
    let ranges = match granularity {
        Granularity::Coarse => std::iter::once(0..saves.len()).collect(),
        Granularity::Fine => split_into_windows(saves),
        Granularity::PerSave => (0..saves.len()).map(|index| index..index + 1).collect(),
    };
    let spinner = Spinner::start(format!("Reading {}", pluralize(saves.len(), "save")));
    let mut from = base.tree_ish()?;
    let mut windows = Vec::with_capacity(ranges.len());
    for range in ranges {
        let save = saves[range.end - 1].commit.clone();
        let (paths, diff) = changes_between(&from, &save)?;
        spinner.set_message(format!("Reading saves {}", describe_range(&range)));
        windows.push(WindowChanges {
            saves: range,
            save: save.clone(),
            files: ChangedFiles::new(paths, diff),
        });
        from = save;
    }
    Ok(windows)
}

/// Cuts the saves, oldest first, into windows: at a pause in the work, or
/// once a window is as large as one request should describe. A single save
/// larger than that is a window of its own.
fn split_into_windows(saves: &[Save]) -> Vec<Range<usize>> {
    let mut windows = Vec::new();
    let mut start = 0;
    let (mut lines, mut files) = (0, 0);
    for (index, save) in saves.iter().enumerate() {
        if index > start {
            let paused = save.time - saves[index - 1].time >= WINDOW_GAP_SECS;
            let full = index - start >= MAX_WINDOW_SAVES
                || lines + save.lines > MAX_WINDOW_LINES
                || files + save.files > MAX_WINDOW_FILES;
            if paused || full {
                windows.push(start..index);
                start = index;
                (lines, files) = (0, 0);
            }
        }
        lines += save.lines;
        files += save.files;
    }
    if start < saves.len() {
        windows.push(start..saves.len());
    }
    windows
}

async fn group_each_window(
    windows: &[WindowChanges],
    examples: &Arc<Vec<String>>,
) -> Result<Vec<Vec<CommitGroup>>> {
    // Saves that cancel out within a window need no commit and no request.
    let planned: Vec<&WindowChanges> = windows
        .iter()
        .filter(|window| !window.files.paths().is_empty())
        .collect();
    let jobs = planned.iter().enumerate().map(|(position, window)| {
        let files = window.files.clone();
        let examples = Arc::clone(examples);
        let slice = HistorySlice {
            position: position + 1,
            count: planned.len(),
            saves: window.saves.len(),
        };
        let label = describe_range(&window.saves);
        let job = async move {
            synthesize_slice(&files, &examples, slice)
                .await
                .map(|groups| normalize_groups(groups, &files))
                .with_context(|| format!("AI planning failed for saves {label}"))
        };
        (window.saves.len(), job)
    });
    let mut planned_groups = run_requests(jobs.collect(), "Planning saves")
        .await?
        .into_iter();

    Ok(windows
        .iter()
        .map(|window| {
            if window.files.paths().is_empty() {
                Vec::new()
            } else {
                planned_groups.next().unwrap_or_default()
            }
        })
        .collect())
}

async fn describe_each_save(
    windows: &[WindowChanges],
    examples: &Arc<Vec<String>>,
) -> Result<Vec<Vec<CommitGroup>>> {
    let described: Vec<&WindowChanges> = windows
        .iter()
        .filter(|window| !window.files.paths().is_empty())
        .collect();
    let jobs = batch_saves(&described).into_iter().map(|batch| {
        let saves: Vec<ChangedFiles> = batch.iter().map(|window| window.files.clone()).collect();
        let examples = Arc::clone(examples);
        let label = describe_range(
            &(batch.first().map_or(0, |window| window.saves.start)
                ..batch.last().map_or(0, |window| window.saves.end)),
        );
        let job = async move {
            describe_saves(&saves, &examples)
                .await
                .with_context(|| format!("AI planning failed for saves {label}"))
        };
        (batch.len(), job)
    });
    let mut messages = run_requests(jobs.collect(), "Describing saves")
        .await?
        .into_iter()
        .flatten();

    Ok(windows
        .iter()
        .map(|window| {
            if window.files.paths().is_empty() {
                return Vec::new();
            }
            let message = messages.next().unwrap_or_default();
            vec![CommitGroup {
                message,
                files: window.files.paths().to_vec(),
            }]
        })
        .collect())
}

/// Consecutive saves that fit one message request.
fn batch_saves<'a>(saves: &[&'a WindowChanges]) -> Vec<Vec<&'a WindowChanges>> {
    let mut batches: Vec<Vec<&WindowChanges>> = Vec::new();
    let mut size = 0;
    for &save in saves {
        let fits = batches.last().is_some_and(|batch| {
            batch.len() < MAX_SAVES_PER_REQUEST && size + save.files.diff_len() <= MAX_DIFF_BYTES
        });
        match batches.last_mut() {
            Some(batch) if fits => batch.push(save),
            _ => {
                batches.push(vec![save]);
                size = 0;
            }
        }
        size += save.files.diff_len();
    }
    batches
}

/// Runs the requests with a bounded number in flight, returning results in
/// request order. Each request is paired with how many saves it plans, which
/// is what the progress counts. The first failure cancels the requests still
/// running: dropping the task set aborts them.
async fn run_requests<T, F>(jobs: Vec<(usize, F)>, label: &str) -> Result<Vec<T>>
where
    T: Send + 'static,
    F: Future<Output = Result<T>> + Send + 'static,
{
    let limit = ai::concurrency()?;
    let count = jobs.len();
    let mut progress = Progress::start(label, jobs.iter().map(|(saves, _)| saves).sum());
    let mut results: Vec<Option<T>> = std::iter::repeat_with(|| None).take(count).collect();
    let mut pending = jobs.into_iter().enumerate();
    let mut running = tokio::task::JoinSet::new();
    loop {
        while running.len() < limit
            && let Some((index, (saves, job))) = pending.next()
        {
            running.spawn(async move { (index, saves, job.await) });
        }
        let Some(finished) = running.join_next().await else {
            break;
        };
        let (index, saves, result) = finished.context("A planning request stopped unexpectedly")?;
        results[index] = Some(result?);
        progress.advance(saves);
    }
    results
        .into_iter()
        .collect::<Option<Vec<T>>>()
        .context("A planning request never finished")
}

/// One-based, as saves are counted in the plan: `3` or `3-9`.
pub(super) fn describe_range(saves: &Range<usize>) -> String {
    if saves.len() <= 1 {
        format!("{}", saves.start + 1)
    } else {
        format!("{}-{}", saves.start + 1, saves.end)
    }
}

/// Renders the plan with one heading per window, so the preview shows where
/// in history each commit comes from. Only subjects are shown: a long history
/// is already a long list.
pub(super) fn render_history_plan(plan: &HistoryPlan) -> String {
    let commits = plan.commit_count();
    let summary = match plan.granularity {
        Granularity::PerSave => format!(
            "{} {} {}, one per save",
            pluralize(plan.saves.len(), "save"),
            "→".dimmed(),
            pluralize(commits, "commit"),
        ),
        Granularity::Coarse | Granularity::Fine => format!(
            "{} {} {} in {}",
            pluralize(plan.saves.len(), "save"),
            "→".dimmed(),
            pluralize(commits, "commit"),
            pluralize(plan.windows.len(), "window"),
        ),
    };
    let mut rendered = format!("{} Plan: {summary}\n", "·".cyan());

    let mut number = 0;
    for window in &plan.windows {
        let times = describe_times(&plan.saves[window.saves.clone()]);
        let heading = match window.saves.len() {
            1 => format!("Save {}", describe_range(&window.saves)),
            _ => format!("Saves {}", describe_range(&window.saves)),
        };
        rendered.push_str(&format!("\n  {} {}\n", heading, times.dimmed()));
        if window.commits.is_empty() {
            rendered.push_str(&format!(
                "     {}\n",
                "changes cancel out; no commit".dimmed()
            ));
        }
        for commit in &window.commits {
            number += 1;
            let subject = commit.message.lines().next().unwrap_or("").trim();
            rendered.push_str(&format!(
                "  {number:>3}. {} {}\n",
                subject.bold(),
                format!("({})", pluralize(commit.files.len(), "file")).dimmed()
            ));
        }
    }
    rendered.push('\n');
    rendered
}

/// When a window's saves were made, in local time: `Oct 17 22:22-23:05`.
fn describe_times(saves: &[Save]) -> String {
    let local =
        |save: &Save| DateTime::from_timestamp(save.time, 0).map(|time| time.with_timezone(&Local));
    let (Some(first), Some(last)) = (saves.first().and_then(local), saves.last().and_then(local))
    else {
        return String::new();
    };
    if saves.len() == 1 || first == last {
        first.format("%b %-d %H:%M").to_string()
    } else if first.date_naive() == last.date_naive() {
        format!("{}-{}", first.format("%b %-d %H:%M"), last.format("%H:%M"))
    } else {
        format!(
            "{} - {}",
            first.format("%b %-d %H:%M"),
            last.format("%b %-d %H:%M")
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn save(time: i64, lines: usize, files: usize) -> Save {
        Save {
            commit: format!("{time:040x}"),
            time,
            files,
            lines,
        }
    }

    fn sized(files: usize, diff_bytes: usize) -> WindowChanges {
        WindowChanges {
            saves: 0..1,
            save: String::new(),
            files: ChangedFiles::new(
                (0..files).map(|index| format!("f{index}")).collect(),
                "x".repeat(diff_bytes),
            ),
        }
    }

    #[test]
    fn a_pause_in_the_work_starts_a_new_window() {
        let saves = [
            save(0, 10, 1),
            save(60, 10, 1),
            save(60 + WINDOW_GAP_SECS, 10, 1),
            save(120 + WINDOW_GAP_SECS, 10, 1),
        ];

        assert_eq!(split_into_windows(&saves), [0..2, 2..4]);
    }

    #[test]
    fn busy_stretches_close_windows_by_save_count_and_size() {
        let steady: Vec<Save> = (0..30).map(|index| save(index * 60, 1, 1)).collect();
        assert_eq!(split_into_windows(&steady), [0..12, 12..24, 24..30]);

        let large = [
            save(0, 3_000, 10),
            save(60, 3_000, 10),
            save(120, 10, 280),
            save(180, 10, 20),
        ];
        // The second save would pass the line cap; the fourth the file cap.
        assert_eq!(split_into_windows(&large), [0..1, 1..3, 3..4]);
    }

    #[test]
    fn an_oversized_save_still_lands_in_a_window_of_its_own() {
        let saves = [save(0, 10, 1), save(60, 90_000, 900), save(120, 10, 1)];

        assert_eq!(split_into_windows(&saves), [0..1, 1..2, 2..3]);
        assert!(split_into_windows(&[]).is_empty());
    }

    #[test]
    fn message_requests_batch_small_saves_and_isolate_large_ones() {
        let small: Vec<WindowChanges> = (0..10).map(|_| sized(1, 100)).collect();
        let small: Vec<&WindowChanges> = small.iter().collect();
        let sizes: Vec<usize> = batch_saves(&small).iter().map(Vec::len).collect();
        assert_eq!(sizes, [MAX_SAVES_PER_REQUEST, 2]);

        let mixed = [
            sized(1, 100),
            sized(1, MAX_DIFF_BYTES),
            sized(1, 100),
            sized(1, 100),
        ];
        let mixed: Vec<&WindowChanges> = mixed.iter().collect();
        let sizes: Vec<usize> = batch_saves(&mixed).iter().map(Vec::len).collect();
        assert_eq!(sizes, [1, 1, 2]);
    }

    #[test]
    fn ranges_are_described_one_based() {
        assert_eq!(describe_range(&(0..1)), "1");
        assert_eq!(describe_range(&(12..24)), "13-24");
    }

    #[test]
    fn the_preview_heads_each_window_and_numbers_commits_across_them() {
        colored::control::set_override(false);
        let commit = |message: &str, files: &[&str]| CommitGroup {
            message: message.to_string(),
            files: files.iter().map(|file| file.to_string()).collect(),
        };
        let plan = HistoryPlan {
            granularity: Granularity::Fine,
            saves: (0..5).map(|index| save(index * 60, 1, 1)).collect(),
            windows: vec![
                WindowPlan {
                    saves: 0..3,
                    save: String::new(),
                    commits: vec![
                        commit("feat(api): add webhooks\n\nWith a body.", &["a", "b"]),
                        commit("docs: describe webhooks", &["c"]),
                    ],
                },
                WindowPlan {
                    saves: 3..4,
                    save: String::new(),
                    commits: Vec::new(),
                },
                WindowPlan {
                    saves: 4..5,
                    save: String::new(),
                    commits: vec![commit("fix(api): retry webhooks", &["a"])],
                },
            ],
        };

        let rendered = render_history_plan(&plan);

        assert!(
            rendered.contains("Plan: 5 saves → 3 commits in 3 windows"),
            "{rendered}"
        );
        assert!(rendered.contains("  Saves 1-3 "), "{rendered}");
        assert!(
            rendered.contains("    1. feat(api): add webhooks (2 files)\n"),
            "{rendered}"
        );
        assert!(
            rendered.contains("    2. docs: describe webhooks (1 file)\n"),
            "{rendered}"
        );
        assert!(rendered.contains("  Save 4 "), "{rendered}");
        assert!(
            rendered.contains("changes cancel out; no commit"),
            "{rendered}"
        );
        assert!(
            rendered.contains("    3. fix(api): retry webhooks (1 file)\n"),
            "{rendered}"
        );
        assert!(!rendered.contains("With a body."), "{rendered}");
    }
}
