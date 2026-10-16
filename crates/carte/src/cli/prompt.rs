//! Line-based questions on the terminal.

use std::io::{self, BufRead, IsTerminal, Write};

/// Whether questions can be asked: stdin is a terminal and `--yes` wasn't given.
pub fn interactive(yes: bool) -> bool {
    !yes && io::stdin().is_terminal()
}

/// Asks for a line of text; an empty answer gives `default`.
pub fn ask(question: &str, default: Option<&str>) -> anyhow::Result<String> {
    match default {
        Some(default) if !default.is_empty() => print!("{question} [{default}]: "),
        _ => print!("{question}: "),
    }
    io::stdout().flush()?;
    let mut line = String::new();
    if io::stdin().lock().read_line(&mut line)? == 0 {
        anyhow::bail!("no answer: input closed");
    }
    let answer = line.trim();
    Ok(if answer.is_empty() {
        default.unwrap_or_default().to_string()
    } else {
        answer.to_string()
    })
}

pub fn confirm(question: &str, default: bool) -> anyhow::Result<bool> {
    let hint = if default { "Y/n" } else { "y/N" };
    loop {
        let answer = ask(&format!("{question} [{hint}]"), None)?;
        match answer.to_lowercase().as_str() {
            "" => return Ok(default),
            "y" | "yes" => return Ok(true),
            "n" | "no" => return Ok(false),
            _ => println!("Please answer y or n."),
        }
    }
}

/// `path` with `$HOME` written as `~`.
pub fn tilde(path: &std::path::Path) -> String {
    if let Some(home) = std::env::var_os("HOME").filter(|h| !h.is_empty())
        && let Ok(rest) = path.strip_prefix(&home)
    {
        if rest.as_os_str().is_empty() {
            return "~".into();
        }
        return format!("~/{}", rest.display());
    }
    path.display().to_string()
}
