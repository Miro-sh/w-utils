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

/// Analyse un débit façon rsync : "10m", "512k", "1.5g", "1048576" (octets/s).
/// Suffixes k/m/g en base 1024, insensibles à la casse.
pub fn parse_rate(s: &str) -> Result<u64, String> {
    let s = s.trim();
    let (num_part, mult) = match s.chars().last() {
        Some(c) if c.is_ascii_alphabetic() => {
            let m = match c.to_ascii_lowercase() {
                'k' => 1024.0,
                'm' => 1024.0 * 1024.0,
                'g' => 1024.0 * 1024.0 * 1024.0,
                'b' => 1.0,
                _ => return Err(format!("suffixe inconnu : '{c}'")),
            };
            (&s[..s.len() - 1], m)
        }
        _ => (s, 1.0),
    };
    let value: f64 = num_part.parse().map_err(|_| format!("débit invalide : '{s}'"))?;
    if value.is_nan() || value <= 0.0 {
        return Err(format!("le débit doit être positif : '{s}'"));
    }
    Ok((value * mult) as u64)
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

    #[test]
    fn rate_with_suffixes() {
        assert_eq!(parse_rate("1024").unwrap(), 1024);
        assert_eq!(parse_rate("512k").unwrap(), 512 * 1024);
        assert_eq!(parse_rate("10M").unwrap(), 10 * 1024 * 1024);
        assert_eq!(parse_rate("1.5g").unwrap(), 1_610_612_736);
        assert_eq!(parse_rate("100b").unwrap(), 100);
    }

    #[test]
    fn rate_rejects_garbage() {
        assert!(parse_rate("0").is_err());
        assert!(parse_rate("-5").is_err());
        assert!(parse_rate("abc").is_err());
        assert!(parse_rate("10t").is_err());
        assert!(parse_rate("").is_err());
    }
}
