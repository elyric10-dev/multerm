//! Cross-platform paths and shell helpers.

use std::path::PathBuf;
use std::process::Command;

/// User home directory (`HOME` on Unix, `USERPROFILE` on Windows).
pub fn home_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        std::env::var("USERPROFILE").ok().map(PathBuf::from)
    }
    #[cfg(not(windows))]
    {
        std::env::var("HOME").ok().map(PathBuf::from)
    }
}

/// Default interactive shell for new PTY sessions.
pub fn default_shell() -> String {
    #[cfg(windows)]
    {
        std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".into())
    }
    #[cfg(not(windows))]
    {
        std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".into())
    }
}

/// Persistent app data directory (`~/.multerm` on Unix, `%LOCALAPPDATA%\multerm` on Windows).
pub fn data_dir() -> PathBuf {
    #[cfg(windows)]
    {
        std::env::var("LOCALAPPDATA")
            .or_else(|_| std::env::var("APPDATA"))
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("."))
            .join("multerm")
    }
    #[cfg(not(windows))]
    {
        home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".multerm")
    }
}

pub fn expand_tilde(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed == "~" {
        return home_dir()
            .and_then(|p| p.to_str().map(str::to_owned))
            .unwrap_or_else(|| trimmed.to_string());
    }
    if let Some(rest) = trimmed.strip_prefix("~/") {
        if let Some(home) = home_dir() {
            return home.join(rest).to_string_lossy().into_owned();
        }
    }
    trimmed.to_string()
}

/// Whether `command` is on the user's PATH.
pub fn is_command_available(command: &str) -> bool {
    if !command
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return false;
    }

    #[cfg(windows)]
    {
        Command::new("cmd")
            .args(["/C", &format!("where {command} >nul 2>&1")])
            .status()
            .is_ok_and(|s| s.success())
    }

    #[cfg(not(windows))]
    {
        Command::new("sh")
            .arg("-lc")
            .arg(format!("command -v {command} >/dev/null 2>&1"))
            .status()
            .is_ok_and(|s| s.success())
    }
}
