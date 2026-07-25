use anyhow::Result;
use colored::*;
use std::io::{self, IsTerminal, Write};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::thread::JoinHandle;
use std::time::Duration;

use crate::ai::flatten_error;

const SPINNER_FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const SPINNER_TICK: Duration = Duration::from_millis(80);

/// Transient activity indicator for slow steps (AI calls, pushes). It animates
/// `message` on the current line and erases itself when it goes away, so the
/// lines printed afterwards are the only record of what happened. When stdout
/// is not a terminal it prints nothing at all.
///
/// Cleanup lives in `Drop`, not only in `stop`, so an early return through `?`
/// cannot leave a thread scribbling over the error message that follows it.
pub(crate) struct Spinner {
    animation: Option<(Sender<()>, JoinHandle<()>)>,
}

impl Spinner {
    pub(crate) fn start(message: impl Into<String>) -> Self {
        if !io::stdout().is_terminal() {
            return Self { animation: None };
        }

        let message = message.into();
        // A channel rather than a polled flag: dropping the sender wakes the
        // thread at once instead of up to one tick later.
        let (stop, stopped) = mpsc::channel();
        let animation = std::thread::spawn(move || {
            for frame in SPINNER_FRAMES.iter().cycle() {
                print!("\r{} {}... ", frame.cyan(), message);
                let _ = io::stdout().flush();
                match stopped.recv_timeout(SPINNER_TICK) {
                    Err(RecvTimeoutError::Timeout) => continue,
                    _ => break,
                }
            }
        });

        Self {
            animation: Some((stop, animation)),
        }
    }

    pub(crate) fn stop(self) {
        drop(self);
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

pub(crate) fn print_ai_unavailable(error: &anyhow::Error) {
    println!("{} AI unavailable", "·".yellow());
    println!("  {}", flatten_error(&format!("{error:#}")).dimmed());
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
    fn pluralize_handles_singular_and_plural_counts() {
        assert_eq!(pluralize(1, "save"), "1 save");
        assert_eq!(pluralize(3, "file"), "3 files");
        assert_eq!(pluralize(0, "commit"), "0 commits");
    }
}
