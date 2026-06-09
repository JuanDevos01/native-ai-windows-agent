//! `metis model` — view or change the agent's LLM model without re-running the
//! full `onboard` flow.
//!
//! - `metis model`                      → show current model + provider status
//! - `metis model <provider/model>`     → set the main model and save config

use anyhow::Result;
use colored::Colorize;

use metis_core::config::{get_config_path, load_config, save_config};
use metis_providers::registry::match_provider;
use metis_providers::PROVIDERS;

/// Dispatch `metis model [TARGET]`.
pub fn run(target: Option<String>) -> Result<()> {
    match target {
        Some(m) => set_model(&m),
        None => show_model(),
    }
}

/// Show the currently configured model and whether its provider is usable.
pub fn show_model() -> Result<()> {
    let config = load_config(None);
    let model = &config.agents.defaults.model;
    let sub = &config.agents.defaults.subagent_model;
    let providers = config.providers.to_map();

    println!();
    println!("{}", "  Model configuration".cyan().bold());
    println!("  Current model : {}", model.green());
    if sub.trim().is_empty() {
        println!("  Subagent model: {}", "(same as main)".dimmed());
    } else {
        println!("  Subagent model: {}", sub.green());
    }

    match match_provider(model, &providers) {
        Some((_cfg, spec)) => {
            println!(
                "  Provider      : {} {}",
                spec.display_name,
                "✓ ready".green()
            );
        }
        None => {
            println!(
                "  Provider      : {}",
                "✗ no configured/usable provider for this model".red()
            );
            println!(
                "  {}",
                "Add the API key with `metis onboard`, or use a local model.".dimmed()
            );
        }
    }

    print_usage();
    Ok(())
}

/// Set the main model in config and persist it.
pub fn set_model(model: &str) -> Result<()> {
    let model = model.trim();
    if model.is_empty() {
        return show_model();
    }

    let mut config = load_config(None);
    let providers = config.providers.to_map();
    let resolves = match_provider(model, &providers).is_some();

    config.agents.defaults.model = model.to_string();
    save_config(&config, None)?;

    println!();
    println!("  {} Model set to {}", "✓".green(), model.green());
    println!("  Config: {}", get_config_path().display());
    if !resolves {
        println!(
            "  {}",
            "⚠ No configured provider matches this model yet.".yellow()
        );
        println!(
            "  {}",
            "  Add the API key with `metis onboard`, or use a local model (ollama/..., lmstudio/...)."
                .dimmed()
        );
    }
    println!(
        "  {}",
        "Restart the gateway/agent for the change to take effect.".dimmed()
    );
    println!();
    Ok(())
}

fn print_usage() {
    println!();
    println!("  Change with: {}", "metis model <provider/model>".bold());
    println!("  Examples:");
    println!("    metis model anthropic/claude-sonnet-4-20250514");
    println!("    metis model openai/gpt-4o");
    println!("    metis model ollama/llama3.1                  (local, no key)");
    println!("    metis model lmstudio/qwen2.5-7b-instruct     (local, no key)");
    println!("    metis model moonshot/kimi-k2-0905-preview");
    println!();
    println!(
        "  Providers: {}",
        PROVIDERS
            .iter()
            .map(|s| s.name)
            .collect::<Vec<_>>()
            .join(", ")
    );
    println!();
}
