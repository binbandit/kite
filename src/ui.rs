use anyhow::Result;
use colored::*;
use std::io::{self, IsTerminal, Write};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

const SPINNER_FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const SPINNER_TICK: Duration = Duration::from_millis(80);

/// Terminal-only activity indicator. Dropping it stops the animation and
/// clears its line, including on early returns through `?`.
pub(crate) struct Spinner {
    message: Arc<Mutex<String>>,
    animation: Option<(Sender<()>, JoinHandle<()>)>,
}

impl Spinner {
    pub(crate) fn start(message: impl Into<String>) -> Self {
        let message = Arc::new(Mutex::new(message.into()));
        if !io::stdout().is_terminal() {
            return Self {
                message,
                animation: None,
            };
        }

        let shown = Arc::clone(&message);
        // Dropping the sender wakes the animation thread immediately.
        let (stop, stopped) = mpsc::channel();
        let animation = std::thread::spawn(move || {
            for frame in SPINNER_FRAMES.iter().cycle() {
                let text = shown.lock().map(|text| text.clone()).unwrap_or_default();
                // Clear the rest of the line in case the text got shorter.
                print!("\r{} {}... \x1b[K", frame.cyan(), text);
                let _ = io::stdout().flush();
                match stopped.recv_timeout(SPINNER_TICK) {
                    Err(RecvTimeoutError::Timeout) => continue,
                    _ => break,
                }
            }
        });

        Self {
            message,
            animation: Some((stop, animation)),
        }
    }

    pub(crate) fn set_message(&self, message: impl Into<String>) {
        if let Ok(mut text) = self.message.lock() {
            *text = message.into();
        }
    }

    pub(crate) fn stop(self) {
        drop(self);
    }
}

/// Counts the finished parts of a long task. A terminal shows the count on
/// the spinner line; other output gets a line for each tenth of the work, so
/// logs show it moving without one line per part.
pub(crate) struct Progress {
    spinner: Spinner,
    label: String,
    total: usize,
    done: usize,
    interactive: bool,
}

impl Progress {
    pub(crate) fn start(label: impl Into<String>, total: usize) -> Self {
        let label = label.into();
        Self {
            spinner: Spinner::start(format!("{label} 0/{total}")),
            label,
            total,
            done: 0,
            interactive: io::stdout().is_terminal(),
        }
    }

    pub(crate) fn advance(&mut self, by: usize) {
        let before = self.tenths();
        self.done += by;
        let progress = format!("{} {}/{}", self.label, self.done, self.total);
        if self.interactive {
            self.spinner.set_message(progress);
        } else if self.tenths() > before {
            println!("{} {}", "·".dimmed(), progress.dimmed());
        }
    }

    fn tenths(&self) -> usize {
        (self.done * 10).checked_div(self.total).unwrap_or(10)
    }
}

impl Drop for Spinner {
    fn drop(&mut self) {
        let Some((stop, animation)) = self.animation.take() else {
            return;
        };

        drop(stop);
        let _ = animation.join();
        print!("\r\x1b[2K");
        let _ = io::stdout().flush();
    }
}

/// Asks a yes/no question and defaults to "no", so destructive steps always
/// require an explicit yes.
pub(crate) fn confirm(question: &str) -> Result<bool> {
    print!("{} {} [y/N]: ", "·".cyan(), question);
    io::stdout().flush()?;

    let mut response = String::new();
    io::stdin().read_line(&mut response)?;

    Ok(matches!(
        response.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

/// Asks for one line of input; returns `None` when the answer is blank.
pub(crate) fn prompt_line(question: &str) -> Result<Option<String>> {
    print!("{} {}: ", "·".cyan(), question);
    io::stdout().flush()?;

    let mut answer = String::new();
    io::stdin().read_line(&mut answer)?;

    let answer = answer.trim();
    Ok((!answer.is_empty()).then(|| answer.to_string()))
}

pub(crate) fn flatten_error(error: &str) -> String {
    error
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join(" | ")
}

pub(crate) fn print_ai_unavailable(error: &anyhow::Error) {
    println!("{} AI unavailable", "·".yellow());
    println!("  {}", flatten_error(&format!("{error:#}")).dimmed());
}

/// Shared wording for the hidden entries in a truncated list.
pub(crate) fn overflow_note(total: usize, shown: usize) -> Option<String> {
    if total <= shown {
        return None;
    }
    Some(format!("… and {} more", total - shown))
}

pub(crate) fn pluralize(count: usize, singular: &str) -> String {
    if count == 1 {
        format!("1 {singular}")
    } else {
        format!("{count} {singular}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overflow_note_appears_only_once_something_is_hidden() {
        assert_eq!(overflow_note(3, 3), None);
        assert_eq!(overflow_note(2, 3), None);
        assert_eq!(overflow_note(4, 3), Some("… and 1 more".to_string()));
    }

    #[test]
    fn pluralize_handles_singular_and_plural_counts() {
        assert_eq!(pluralize(1, "save"), "1 save");
        assert_eq!(pluralize(3, "file"), "3 files");
        assert_eq!(pluralize(0, "commit"), "0 commits");
    }
}
