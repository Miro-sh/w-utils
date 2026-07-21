//! Helpers : détection de terminal, messages colorés, formatage de durées.

use std::io::IsTerminal;
use std::time::Duration;

use colored::Colorize;

/// La barre de progression et les résumés vont sur stderr/stdout :
/// on se base sur stderr pour savoir si on tourne dans un terminal.
pub fn is_interactive() -> bool {
    std::io::stderr().is_terminal()
}

pub fn print_success(msg: &str) {
    println!("{} {}", "✓".green().bold(), msg);
}

pub fn print_warn(msg: &str) {
    eprintln!("{} {}", "!".yellow().bold(), msg);
}

pub fn print_error(msg: &str) {
    eprintln!("{} {}", "✗".red().bold(), msg);
}

/// Formate une durée de façon compacte : "4.2s", "1m 15s", "1:01:01".
pub fn format_duration(d: Duration) -> String {
    let secs = d.as_secs();
    if secs >= 3600 {
        format!("{}:{:02}:{:02}", secs / 3600, (secs % 3600) / 60, secs % 60)
    } else if secs >= 60 {
        format!("{}m {:02}s", secs / 60, secs % 60)
    } else {
        format!("{:.1}s", d.as_secs_f64())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duration_under_a_minute() {
        assert_eq!(format_duration(Duration::from_millis(4200)), "4.2s");
    }

    #[test]
    fn duration_minutes() {
        assert_eq!(format_duration(Duration::from_secs(75)), "1m 15s");
    }

    #[test]
    fn duration_hours() {
        assert_eq!(format_duration(Duration::from_secs(3661)), "1:01:01");
    }
}
