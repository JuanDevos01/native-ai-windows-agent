//! Shared CLI helpers — path expansion, response printing, version banner.

use std::path::PathBuf;

use colored::Colorize;

/// Expand `~` at the start of a path to the user's home directory.
pub fn expand_tilde(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = dirs_next::home_dir() {
            return home.join(rest);
        }
    }
    if path == "~" {
        if let Some(home) = dirs_next::home_dir() {
            return home;
        }
    }
    PathBuf::from(path)
}

/// Print an agent response to stdout.
pub fn print_response(response: &str, _render_markdown: bool) {
    // TODO: add termimad or similar markdown renderer when render_markdown=true
    println!();
    println!("{}", "🦀 Metis".cyan().bold());
    if response.is_empty() {
        println!("{}", "(no response)".dimmed());
    } else {
        println!("{response}");
    }
    println!();
}

/// Print the banner shown at REPL start.
pub fn print_banner() {
    println!();
    println!(
        "{}  {}",
        "🦀 Metis".cyan().bold(),
        metis_core::build::version_line().dimmed()
    );
    println!(
        "{}",
        "Type a message, or \"exit\" to quit.".dimmed()
    );
    println!();
}

/// Print a "thinking" spinner placeholder (for non-log mode).
pub fn print_thinking() {
    eprint!("{}", "⠿ thinking...".dimmed());
}

/// Clear the "thinking" placeholder.
pub fn clear_thinking() {
    eprint!("\r{}\r", " ".repeat(40));
}

/// Read assistant name from `AGENTS.md` in workspace.
///
/// Expected line format: `- **Name**: <value>`.
/// Returns None if file/field missing or value is empty.
pub fn load_agent_name(workspace: &std::path::Path) -> Option<String> {
    let path = workspace.join("AGENTS.md");
    let content = std::fs::read_to_string(path).ok()?;
    for line in content.lines() {
        let line = line.trim();
        if let Some(name) = line.strip_prefix("- **Name**:") {
            let name = name.trim();
            if !name.is_empty() {
                return Some(name.to_string());
            }
        }
    }
    None
}

// ─────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expand_tilde_home() {
        let result = expand_tilde("~/foo/bar");
        assert!(result.ends_with("foo/bar"));
        assert!(!result.starts_with("~"));
    }

    #[test]
    fn expand_tilde_no_tilde() {
        let result = expand_tilde("/absolute/path");
        assert_eq!(result, PathBuf::from("/absolute/path"));
    }

    #[test]
    fn expand_tilde_bare() {
        let result = expand_tilde("~");
        assert!(!result.to_string_lossy().contains('~'));
    }

    #[test]
    fn expand_tilde_relative() {
        let result = expand_tilde("relative/path");
        assert_eq!(result, PathBuf::from("relative/path"));
    }

    #[test]
    fn load_agent_name_from_agents_md() {
        let dir = tempfile::tempdir().unwrap();
        let agents = dir.path().join("AGENTS.md");
        std::fs::write(
            &agents,
            "# Agents\n\n- **Name**: Nova\n- **Role**: Personal AI assistant\n",
        )
        .unwrap();

        let name = load_agent_name(dir.path());
        assert_eq!(name.as_deref(), Some("Nova"));
    }

    #[test]
    fn load_agent_name_none_when_missing() {
        let dir = tempfile::tempdir().unwrap();
        assert!(load_agent_name(dir.path()).is_none());
    }
}
