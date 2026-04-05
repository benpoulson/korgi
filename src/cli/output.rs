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

/// Interactive multi-select for choosing services.
/// Returns indices of selected items.
/// If `auto_yes` is true, returns all items selected.
pub fn multi_select(prompt: &str, items: &[&str], auto_yes: bool) -> Vec<usize> {
    if auto_yes || items.len() <= 1 {
        return (0..items.len()).collect();
    }

    use console::Term;
    let term = Term::stderr();

    let mut selected: Vec<bool> = vec![false; items.len()]; // none selected by default
    let mut cursor: usize = 0;

    eprintln!(
        "{} {} {}",
        style("?").cyan().bold(),
        prompt,
        style("(space=toggle, a=all, n=none, enter=confirm)").dim()
    );

    loop {
        // Render list
        for (i, item) in items.iter().enumerate() {
            let checkbox = if selected[i] {
                style("[x]").green().to_string()
            } else {
                style("[ ]").dim().to_string()
            };
            let arrow = if i == cursor {
                style(">").cyan().bold().to_string()
            } else {
                " ".to_string()
            };
            eprintln!("{} {} {}", arrow, checkbox, item);
        }

        // Read key
        match term.read_key() {
            Ok(console::Key::ArrowUp | console::Key::Char('k')) if cursor > 0 => {
                cursor -= 1;
            }
            Ok(console::Key::ArrowDown | console::Key::Char('j'))
                if cursor < items.len().saturating_sub(1) =>
            {
                cursor += 1;
            }
            Ok(console::Key::Char(' ')) => {
                selected[cursor] = !selected[cursor];
            }
            Ok(console::Key::Char('a')) => {
                selected.iter_mut().for_each(|s| *s = true);
            }
            Ok(console::Key::Char('n')) => {
                selected.iter_mut().for_each(|s| *s = false);
            }
            Ok(console::Key::Enter) => break,
            Ok(console::Key::Escape) | Ok(console::Key::Char('q')) => {
                return vec![]; // cancel
            }
            _ => {}
        }

        // Clear rendered lines and re-render
        for _ in 0..items.len() {
            term.clear_line().ok();
            term.move_cursor_up(1).ok();
        }
        term.clear_line().ok();
    }

    selected
        .iter()
        .enumerate()
        .filter(|(_, s)| **s)
        .map(|(i, _)| i)
        .collect()
}
