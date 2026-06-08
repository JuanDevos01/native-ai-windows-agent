//! Shared CLI helpers — path expansion, response printing, version banner.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use colored::Colorize;

use metis_core::config::schema::ProviderConfig;
use metis_providers::http_provider::create_provider;
use metis_providers::traits::LlmProvider;

/// Build a dedicated LLM provider for subagents, when `subagent_model` is set.
///
/// Returns `None` to mean "reuse the main agent's provider" — that is the case
/// when no subagent model is configured. When a model is configured but its
/// provider can't be built (missing key, unknown provider), we log a warning
/// and fall back to the main provider rather than failing startup.
pub fn build_subagent_provider(
    subagent_model: &str,
    providers_map: &HashMap<String, ProviderConfig>,
) -> Option<Arc<dyn LlmProvider>> {
    let model = subagent_model.trim();
    if model.is_empty() {
        return None;
    }
    match create_provider(model, providers_map) {
        Ok(provider) => Some(Arc::new(provider) as Arc<dyn LlmProvider>),
        Err(e) => {
            tracing::warn!(
                model = %model,
                error = %e,
                "subagent model provider unavailable; subagents will use the main agent's provider"
            );
            None
        }
    }
}

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
