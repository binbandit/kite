use anyhow::Result;
use colored::*;
use std::io::{self, IsTerminal, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

const SPINNER_FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const SPINNER_TICK: Duration = Duration::from_millis(80);

/// Animates `message` on the current line while a slow step (usually an AI
/// call) runs, then replaces itself with the final outcome. Falls back to a
/// single plain line when stdout is not a terminal.
pub(crate) struct Spinner {
    message: String,
    animation: Option<(Arc<AtomicBool>, JoinHandle<()>)>,
}

impl Spinner {
    pub(crate) fn start(message: impl Into<String>) -> Self {
        let message = message.into();

        if !io::stdout().is_terminal() {
            return Self {
                message,
                animation: None,
            };
        }

        let stop = Arc::new(AtomicBool::new(false));
        let animation = {
            let stop = stop.clone();
            let message = message.clone();
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
            message,
            animation: Some((stop, animation)),
        }
    }

    pub(crate) fn finish(self, outcome: &str) {
        if let Some((stop, animation)) = self.animation {
            stop.store(true, Ordering::Relaxed);
            let _ = animation.join();
            print!("\r\x1b[2K");
        }
        println!("{} {}... {}", "·".cyan(), self.message, outcome);
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
