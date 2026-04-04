use console::style;
use indicatif::{ProgressBar, ProgressStyle};
use std::time::Duration;

/// Create a spinner with a message for long-running operations.
pub fn spinner(msg: &str) -> ProgressBar {
    let pb = ProgressBar::new_spinner();
    pb.set_style(
        ProgressStyle::default_spinner()
            .template("{spinner:.cyan} {msg}")
            .expect("invalid spinner template"),
    );
    pb.set_message(msg.to_string());
    pb.enable_steady_tick(Duration::from_millis(100));
    pb
}

/// Create a progress bar for operations with known count.
pub fn progress_bar(len: u64, msg: &str) -> ProgressBar {
    let pb = ProgressBar::new(len);
    pb.set_style(
        ProgressStyle::default_bar()
            .template("{spinner:.cyan} {msg} [{bar:30.cyan/dim}] {pos}/{len}")
            .expect("invalid progress bar template")
            .progress_chars("=> "),
    );
    pb.set_message(msg.to_string());
    pb
}

pub fn success(msg: &str) {
    eprintln!("{} {}", style("✓").green().bold(), msg);
}

pub fn error(msg: &str) {
    eprintln!("{} {}", style("✗").red().bold(), msg);
}

pub fn warn(msg: &str) {
    eprintln!("{} {}", style("!").yellow().bold(), msg);
}

pub fn info(msg: &str) {
    eprintln!("{} {}", style("→").cyan(), msg);
}

pub fn header(msg: &str) {
    eprintln!("\n{}", style(msg).bold().underlined());
}

/// Prompt the user for confirmation. Returns true if they accept.
/// If `auto_yes` is true, skips the prompt and returns true.
pub fn confirm(msg: &str, auto_yes: bool) -> bool {
    if auto_yes {
        return true;
    }

    eprint!("{} {} ", style("?").cyan().bold(), msg);
    eprint!("{} ", style("[y/N]").dim());

    use std::io::{self, BufRead};
    let mut input = String::new();
    if io::stdin().lock().read_line(&mut input).is_err() {
        return false;
    }

    matches!(input.trim().to_lowercase().as_str(), "y" | "yes")
}
