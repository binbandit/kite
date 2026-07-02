use anyhow::Result;
use colored::*;
use std::io::{self, IsTerminal, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use crate::ai::{ProviderFailure, flatten_error};

const SPINNER_FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const SPINNER_TICK: Duration = Duration::from_millis(80);

/// Transient activity indicator for slow steps (AI calls, pushes). It animates
/// `message` on the current line and erases itself on `stop`, so the lines
/// printed afterwards are the only record of what happened. When stdout is not
/// a terminal it prints nothing at all.
pub(crate) struct Spinner {
    animation: Option<(Arc<AtomicBool>, JoinHandle<()>)>,
}

impl Spinner {
    pub(crate) fn start(message: impl Into<String>) -> Self {
        if !io::stdout().is_terminal() {
            return Self { animation: None };
        }

        let message = message.into();
        let stop = Arc::new(AtomicBool::new(false));
        let animation = {
            let stop = stop.clone();
            std::thread::spawn(move || {
                for frame in SPINNER_FRAMES.iter().cycle() {
                    if stop.load(Ordering::Relaxed) {
                        break;
                    }
                    print!("\r{} {}... ", frame.cyan(), message);
                    let _ = io::stdout().flush();
                    std::thread::sleep(SPINNER_TICK);
                }
            })
        };

        Self {
            animation: Some((stop, animation)),
        }
    }

    pub(crate) fn stop(self) {
        if let Some((stop, animation)) = self.animation {
            stop.store(true, Ordering::Relaxed);
            let _ = animation.join();
            print!("\r\x1b[2K");
            let _ = io::stdout().flush();
        }
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

pub(crate) fn print_provider_failures(failures: &[ProviderFailure]) {
    println!("{} AI unavailable", "·".yellow());
    for failure in failures {
        println!(
            "  {} {}: {}",
            "-".dimmed(),
            failure.provider,
            flatten_error(&failure.error).dimmed()
        );
    }
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
