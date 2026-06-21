//! Discover LLM models that are actually usable on this machine.

use std::collections::{BTreeSet, HashSet};
use std::process::Command;

use metis_core::config::Config;
use metis_providers::registry::match_provider;
use metis_providers::{ollama_model_supports_tools_sync, ProviderConfig, PROVIDERS};

use super::config::DesktopConfig;

/// Default model id offered when a cloud provider has an API key configured.
fn default_model_for_provider(name: &str) -> Option<&'static str> {
    match name {
        "anthropic" => Some("anthropic/claude-sonnet-4-20250514"),
        "openai" => Some("openai/gpt-4o"),
        "openrouter" => Some("openrouter/auto"),
        "deepseek" => Some("deepseek/deepseek-chat"),
        "groq" => Some("groq/llama-3.3-70b-versatile"),
        "gemini" => Some("gemini/gemini-2.0-flash"),
        "moonshot" => Some("moonshot/kimi-k2-0905-preview"),
        "minimax" => Some("MiniMax-M2"),
        "zhipu" => Some("glm-4-flash"),
        "dashscope" => Some("qwen-turbo"),
        "aihubmix" => Some("openai/gpt-4o"),
        _ => None,
    }
}

/// Bare model names reported by `ollama list` (e.g. `gemma3:4b`).
pub fn list_ollama_installed() -> Vec<String> {
    let output = Command::new("ollama").arg("list").output();
    let Ok(output) = output else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    let text = String::from_utf8_lossy(&output.stdout);
    parse_ollama_list_text(&text)
}

fn parse_ollama_list_text(text: &str) -> Vec<String> {
    let mut names = Vec::new();
    for line in text.lines().skip(1) {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Some(name) = line.split_whitespace().next() {
            if name != "NAME" {
                names.push(name.to_string());
            }
        }
    }
    names
}

fn ollama_bare_name(model: &str) -> Option<&str> {
    model
        .strip_prefix("ollama/")
        .or_else(|| model.strip_prefix("Ollama/"))
}

fn is_model_usable(
    model: &str,
    providers: &std::collections::HashMap<String, ProviderConfig>,
    ollama_installed: &HashSet<String>,
) -> bool {
    let Some((_cfg, spec)) = match_provider(model, providers) else {
        return false;
    };
    if spec.name == "ollama" {
        if let Some(bare) = ollama_bare_name(model) {
            return ollama_installed.contains(bare);
        }
        return ollama_installed.contains(model);
    }
    true
}

/// Build the stable model dropdown: installed Ollama models + configured cloud providers.
pub fn discover_available_models(config: &Config, desktop: &DesktopConfig) -> Vec<String> {
    let providers = config.providers.to_map();
    let ollama_installed: HashSet<String> = list_ollama_installed().into_iter().collect();
    let mut models = BTreeSet::new();

    for bare in &ollama_installed {
        models.insert(format!("ollama/{bare}"));
    }

    for spec in PROVIDERS {
        if spec.is_gateway || spec.is_local {
            continue;
        }
        let Some(cfg) = providers.get(spec.name) else {
            continue;
        };
        if !cfg.is_configured() {
            continue;
        }
        if let Some(default) = default_model_for_provider(spec.name) {
            models.insert(default.to_string());
        }
    }

    if !config.agents.defaults.model.is_empty() {
        models.insert(config.agents.defaults.model.clone());
    }
    if !config.agents.defaults.subagent_model.is_empty() {
        models.insert(config.agents.defaults.subagent_model.clone());
    }

    for m in &desktop.extra_models {
        let m = m.trim();
        if !m.is_empty() {
            models.insert(m.to_string());
        }
    }

    let mut out: Vec<String> = models
        .into_iter()
        .filter(|m| is_model_usable(m, &providers, &ollama_installed))
        .collect();

    out.sort_by(|a, b| {
        let a_ollama = a.starts_with("ollama/");
        let b_ollama = b.starts_with("ollama/");
        match (a_ollama, b_ollama) {
            (true, false) => std::cmp::Ordering::Less,
            (false, true) => std::cmp::Ordering::Greater,
            _ => a.to_lowercase().cmp(&b.to_lowercase()),
        }
    });

    out
}

fn ollama_api_base(config: &Config) -> String {
    config
        .providers
        .ollama
        .api_base
        .clone()
        .or_else(|| PROVIDERS.iter().find(|s| s.name == "ollama")?.default_api_base.map(String::from))
        .unwrap_or_else(|| "http://localhost:11434/v1".to_string())
}

/// Human-readable label for the model dropdown (notes chat-only Ollama models).
pub fn model_menu_label(model: &str, config: &Config) -> String {
    if let Some(bare) = ollama_bare_name(model) {
        if ollama_model_supports_tools_sync(&ollama_api_base(config), bare) == Some(false) {
            return format!("{model} (chat only — no tools)");
        }
    }
    model.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_ollama_list() {
        let text = "NAME         ID              SIZE      MODIFIED\n\
                    gemma3:4b    a2af6cc3eb7f    3.3 GB    11 days ago\n";
        assert_eq!(parse_ollama_list_text(text), vec!["gemma3:4b".to_string()]);
    }
}
