//! Shared CLI helpers — path expansion, response printing, version banner.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use colored::Colorize;
use metis_core::config::schema::ProviderConfig;
use metis_providers::http_provider::create_provider;
use metis_providers::traits::LlmProvider;

/// The user/agent guide, bundled into the binary at build time.
const GUIDE_MD: &str = include_str!("../../../GUIDE.md");

/// Write the bundled `GUIDE.md` into the workspace so the agent can always read
/// it via `read_file` at a stable path (`<workspace>/GUIDE.md`). Refreshes it
/// when the bundled copy changes. Also updates legacy OxiBot branding in bootstrap
/// files. Best-effort: logs a warning on failure.
pub fn ensure_guide_in_workspace(workspace: &Path) {
    migrate_legacy_branding_in_workspace(workspace);

    let dest = workspace.join("GUIDE.md");
    let needs_write = match std::fs::read_to_string(&dest) {
        Ok(existing) => existing != GUIDE_MD,
        Err(_) => true,
    };
    if needs_write {
        if let Err(e) = std::fs::write(&dest, GUIDE_MD) {
            tracing::warn!(path = %dest.display(), error = %e, "failed to write GUIDE.md to workspace");
        }
    }
}

/// Bootstrap files that may still contain legacy OxiBot naming from older installs.
const BOOTSTRAP_BRANDING_FILES: &[&str] = &[
    "AGENTS.md",
    "USER.md",
    "SOUL.md",
    "IDENTITY.md",
    "HEARTBEAT.md",
    "memory/MEMORY.md",
];

/// Replace legacy OxiBot/Oxibot strings with Metis in workspace bootstrap files.
pub fn migrate_legacy_branding_in_workspace(workspace: &Path) {
    for rel in BOOTSTRAP_BRANDING_FILES {
        let path = workspace.join(rel);
        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };
        let updated = rebrand_legacy_text(&content);
        if updated == content {
            continue;
        }
        match std::fs::write(&path, &updated) {
            Ok(()) => tracing::info!(
                path = %path.display(),
                "updated legacy branding to Metis"
            ),
            Err(e) => tracing::warn!(
                path = %path.display(),
                error = %e,
                "failed to update legacy branding"
            ),
        }
    }
}

/// Targeted replacements — avoids touching `.oxibot` path segments.
fn rebrand_legacy_text(content: &str) -> String {
    let mut out = content.to_string();
    const REPLACEMENTS: &[(&str, &str)] = &[
        ("Tell Oxibot about", "Tell Metis about"),
        ("Tell OxiBot about", "Tell Metis about"),
        ("I am Oxibot,", "I am Metis,"),
        ("I am OxiBot,", "I am Metis,"),
        ("Oxibot persists", "Metis persists"),
        ("OxiBot persists", "Metis persists"),
        ("# Metis (formerly OxiBot)", "# Metis"),
        ("# Metis (formerly Oxibot)", "# Metis"),
        ("- **Name**: Oxibot", "- **Name**: Metis"),
        ("- **Name**: OxiBot", "- **Name**: Metis"),
        ("your OxiBot agent", "your Metis agent"),
        ("your Oxibot agent", "your Metis agent"),
    ];
    for (from, to) in REPLACEMENTS {
        if out.contains(from) {
            out = out.replace(from, to);
        }
    }
    out
}

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
    fn rebrand_legacy_text_updates_oxibot_phrases() {
        let input = "Tell Oxibot about yourself.\nOxibot persists important information here.";
        let out = rebrand_legacy_text(input);
        assert!(out.contains("Tell Metis about"));
        assert!(out.contains("Metis persists"));
        assert!(!out.contains("Oxibot"));
    }

    #[test]
    fn load_agent_name_none_when_missing() {
        let dir = tempfile::tempdir().unwrap();
        assert!(load_agent_name(dir.path()).is_none());
    }
}
