//! `Metis onboard` — initialize configuration and workspace.
//!
//! Replaces nanobot's `onboard` command:
//! - Creates `~/.metis/config.json` with defaults
//! - Creates workspace directory with template files
//! - Optionally walks through LLM provider + default model and chat channels (TTY only)

use std::io::{self, IsTerminal, Write};

use anyhow::Result;
use colored::Colorize;

use metis_core::config::{load_config, save_config, Config};
use metis_core::utils::{get_data_path, get_default_workspace_path};

/// Run the onboard command.
pub fn run(non_interactive: bool, profile_only: bool) -> Result<()> {
    println!();
    println!("{}", "🦀 Metis — Setup".cyan().bold());
    println!();

    let data_dir = get_data_path();
    let config_path = data_dir.join("config.json");

    // 1. Create config if it doesn't exist
    if config_path.exists() {
        println!(
            "  {} config already exists at {}",
            "✓".green(),
            config_path.display()
        );
    } else {
        let config = load_config(None); // defaults
        save_config(&config, Some(&config_path))?;
        println!(
            "  {} created config at {}",
            "✓".green(),
            config_path.display()
        );
    }

    // 2. Ensure workspace directory
    let workspace = get_default_workspace_path();
    std::fs::create_dir_all(&workspace)?;
    println!(
        "  {} workspace at {}",
        "✓".green(),
        workspace.display()
    );

    // 3. Create memory directory
    let memory_dir = workspace.join("memory");
    std::fs::create_dir_all(&memory_dir)?;
    println!("  {} memory dir at {}", "✓".green(), memory_dir.display());

    // 4. Create template files if they don't exist
    create_template(&workspace.join("AGENTS.md"), AGENTS_TEMPLATE)?;
    create_template(&workspace.join("SOUL.md"), SOUL_TEMPLATE)?;
    create_template(&workspace.join("USER.md"), USER_TEMPLATE)?;
    create_template(&workspace.join("HEARTBEAT.md"), HEARTBEAT_TEMPLATE)?;
    create_template(&memory_dir.join("MEMORY.md"), MEMORY_TEMPLATE)?;

    // 5. Create skills directory with skill-creator
    let skills_dir = workspace.join("skills");
    std::fs::create_dir_all(&skills_dir)?;
    let sc_dir = skills_dir.join("skill-creator");
    if !sc_dir.exists() {
        std::fs::create_dir_all(&sc_dir)?;
        std::fs::write(sc_dir.join("SKILL.md"), SKILL_CREATOR_TEMPLATE)?;
        println!("  {} created skill: skill-creator", "✓".green());
    } else {
        println!("  {} skill-creator already exists", "✓".green());
    }

    // 6. Create sessions + history directories
    let sessions_dir = data_dir.join("sessions");
    std::fs::create_dir_all(&sessions_dir)?;
    let history_dir = data_dir.join("history");
    std::fs::create_dir_all(&history_dir)?;

    let interactive = !non_interactive && io::stdin().is_terminal();
    if interactive {
        println!();
        if profile_only {
            println!("{}", "  Re-onboard profile".cyan().bold());
        } else {
            println!("{}", "  Guided setup (optional)".cyan().bold());
        }
        println!();
        let mut config = load_config(Some(&config_path));
        let mut changed = false;
        let user_profile_path = workspace.join("USER.md");
        let agent_profile_path = workspace.join("AGENTS.md");
        let mut profile_changed = false;
        if prompt_yn(
            "  Set assistant name (what should the agent call itself)?",
            true,
        )? {
            profile_changed |= configure_agent_profile(&agent_profile_path)?;
        }
        if prompt_yn(
            "  Tell Metis about you (name, role, communication style)?",
            true,
        )? {
            profile_changed = configure_user_profile(&user_profile_path)?;
        }
        if !profile_only {
            if prompt_yn(
                "  Configure LLM provider and default model?",
                true,
            )? {
                changed |= configure_provider_and_model(&mut config)?;
            }
            if prompt_yn(
                "  Configure chat channels (Telegram, Discord, WhatsApp, Slack, Email)?",
                false,
            )? {
                changed |= configure_channels(&mut config)?;
            }
        }
        if changed {
            save_config(&config, Some(&config_path))?;
            println!();
            println!("  {} saved {}", "✓".green(), config_path.display());
        }
        if !changed && !profile_changed {
            println!();
            println!(
                "  {}",
                "No config changes (edit ~/.metis/config.json anytime).".dimmed()
            );
        }
    } else if !non_interactive {
        println!();
        if profile_only {
            println!(
                "  {}",
                "Tip: run `Metis onboard --profile-only` in a real terminal (not piped) for profile prompts."
                    .dimmed()
            );
        } else {
            println!(
                "  {}",
                "Tip: run `Metis onboard` in a real terminal (not piped) for guided provider, model, and channel setup."
                    .dimmed()
            );
        }
    }

    println!();
    println!(
        "{}",
        "  Setup complete! Try `Metis status`, then `Metis agent` or `Metis gateway`."
            .green()
    );
    println!();

    Ok(())
}

fn read_line(prompt: &str) -> Result<String> {
    print!("{}", prompt);
    io::stdout().flush()?;
    let mut line = String::new();
    io::stdin().read_line(&mut line)?;
    Ok(line.trim().to_string())
}

fn prompt_yn(prompt: &str, default_yes: bool) -> Result<bool> {
    let hint = if default_yes { "Y/n" } else { "y/N" };
    loop {
        let line = read_line(&format!("{prompt} [{hint}]: "))?;
        if line.is_empty() {
            return Ok(default_yes);
        }
        match line.to_lowercase().as_str() {
            "y" | "yes" | "1" | "true" => return Ok(true),
            "n" | "no" | "0" | "false" => return Ok(false),
            _ => println!("  Please enter y or n (or press Enter for default)."),
        }
    }
}

fn prompt_nonempty(prompt: &str) -> Result<String> {
    loop {
        let line = read_line(prompt)?;
        if !line.trim().is_empty() {
            return Ok(line.trim().to_string());
        }
        println!("  This cannot be empty.");
    }
}

fn prompt_choice(prompt: &str, options: &[&str], default: usize) -> Result<usize> {
    println!("{prompt}");
    for (i, option) in options.iter().enumerate() {
        println!("    {}) {}", i + 1, option);
    }

    loop {
        let line = read_line(&format!(
            "  Enter choice [1-{}, default {}]: ",
            options.len(),
            default + 1
        ))?;
        if line.trim().is_empty() {
            return Ok(default);
        }
        if let Ok(n) = line.trim().parse::<usize>() {
            if (1..=options.len()).contains(&n) {
                return Ok(n - 1);
            }
        }
        println!("  Please enter a valid number.");
    }
}

/// Ask user identity + style questions and write USER.md.
///
/// Returns `true` if file content changed.
fn configure_user_profile(path: &std::path::Path) -> Result<bool> {
    let current = std::fs::read_to_string(path).unwrap_or_default();

    let name = prompt_nonempty("  What should I call you? ")?;

    let role_options = [
        "Founder / Product Builder",
        "Software Engineer",
        "Student / Learner",
        "Other (type your own)",
    ];
    let role_choice = prompt_choice("  What best describes your role?", &role_options, 1)?;
    let role = if role_choice == 3 {
        prompt_nonempty("  Enter your role: ")?
    } else {
        role_options[role_choice].to_string()
    };

    let style_options = [
        "Concise - short, direct answers",
        "Balanced - practical detail with examples",
        "Detailed - thorough step-by-step explanations",
        "Other (type your own)",
    ];
    let style_choice = prompt_choice(
        "  How should I communicate with you?",
        &style_options,
        1,
    )?;
    let preferences = if style_choice == 3 {
        prompt_nonempty("  Enter your communication style: ")?
    } else {
        style_options[style_choice].to_string()
    };

    println!();
    println!("  Profile summary:");
    println!("    Name: {}", name);
    println!("    Role: {}", role);
    println!("    Communication Style: {}", preferences);

    if !prompt_yn("  Save this profile?", true)? {
        println!("  {}", "Skipped profile save — keeping current USER.md.".dimmed());
        return Ok(false);
    }

    let new_content = user_profile_template(&name, &role, &preferences);
    if current == new_content {
        println!("  {}", "USER.md already matches your answers.".dimmed());
        return Ok(false);
    }

    std::fs::write(path, new_content)?;
    println!("  {} updated {}", "✓".green(), path.display());
    Ok(true)
}

/// Ask assistant identity and write AGENTS.md.
///
/// Returns `true` if file content changed.
fn configure_agent_profile(path: &std::path::Path) -> Result<bool> {
    let current = std::fs::read_to_string(path).unwrap_or_default();
    let name = read_line("  Assistant name (Enter = Metis): ")?;
    let name = if name.trim().is_empty() {
        "Metis".to_string()
    } else {
        name.trim().to_string()
    };

    let new_content = agent_profile_template(&name);
    if current == new_content {
        println!("  {}", "AGENTS.md already matches your answers.".dimmed());
        return Ok(false);
    }

    std::fs::write(path, new_content)?;
    println!("  {} updated {}", "✓".green(), path.display());
    Ok(true)
}

fn agent_profile_template(name: &str) -> String {
    let name = if name.trim().is_empty() {
        "Metis"
    } else {
        name.trim()
    };

    format!(
        "# Agents\n\n\
Configuration and personality for your AI agents.\n\n\
## Default Agent\n\n\
- **Name**: {name}\n\
- **Role**: Personal AI assistant\n\
- **Style**: Concise, helpful, technical when needed\n"
    )
}

fn user_profile_template(name: &str, role: &str, preferences: &str) -> String {
    let name = if name.trim().is_empty() {
        "(your name)"
    } else {
        name.trim()
    };
    let role = if role.trim().is_empty() {
        "(your role/profession)"
    } else {
        role.trim()
    };
    let preferences = if preferences.trim().is_empty() {
        "(communication preferences)"
    } else {
        preferences.trim()
    };

    format!(
        "# User Profile\n\n\
Tell Metis about yourself so it can personalize its responses.\n\n\
## About Me\n\n\
- **Name**: {name}\n\
- **Role**: {role}\n\
- **Communication Style**: {preferences}\n"
    )
}

/// Returns `true` if config was modified.
fn configure_provider_and_model(config: &mut Config) -> Result<bool> {
    println!("  LLM provider (API key input is echoed in this terminal):");
    println!("    1) OpenRouter (multi-model gateway)");
    println!("    2) Anthropic (Claude direct)");
    println!("    3) OpenAI");
    println!("    4) DeepSeek");
    println!("    5) Groq");
    println!("    6) Google Gemini");
    println!("    7) MiniMax (direct API, platform.minimax.io)");
    println!("    8) Skip");
    let choice = loop {
        let line = read_line("  Enter choice [1-8, default 8]: ")?;
        let c = if line.is_empty() { "8".to_string() } else { line };
        match c.as_str() {
            "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" => break c,
            _ => println!("  Enter a number from 1 to 8."),
        }
    };

    if choice == "8" {
        return Ok(false);
    }

    let default_model = match choice.as_str() {
        "1" | "2" => "anthropic/claude-sonnet-4-20250514",
        "3" => "openai/gpt-4o",
        "4" => "deepseek/deepseek-chat",
        "5" => "groq/llama-3.3-70b-versatile",
        "6" => "gemini/gemini-2.0-flash",
        "7" => "MiniMax-M2",
        _ => return Ok(false),
    };

    let key = read_line("  Paste API key (Enter to skip): ")?;
    if key.is_empty() {
        println!("  {}", "Skipped — no API key stored.".dimmed());
        return Ok(false);
    }
    match choice.as_str() {
        "1" => config.providers.openrouter.api_key = key,
        "2" => config.providers.anthropic.api_key = key,
        "3" => config.providers.openai.api_key = key,
        "4" => config.providers.deepseek.api_key = key,
        "5" => config.providers.groq.api_key = key,
        "6" => config.providers.gemini.api_key = key,
        "7" => config.providers.minimax.api_key = key,
        _ => {}
    }

    println!("  Suggested default model: {}", default_model.dimmed());
    let model_line = read_line("  Model id (Enter = use suggestion above): ")?;
    if model_line.is_empty() {
        config.agents.defaults.model = default_model.to_string();
    } else {
        config.agents.defaults.model = model_line;
    }

    println!(
        "  {} model set to {}",
        "✓".green(),
        config.agents.defaults.model
    );
    Ok(true)
}

/// Returns `true` if config was modified.
fn configure_channels(config: &mut Config) -> Result<bool> {
    let mut changed = false;

    let tg = read_line("  Telegram bot token (Enter to skip): ")?;
    if !tg.is_empty() {
        config.channels.telegram.token = tg;
        changed = true;
        println!("  {} Telegram token saved", "✓".green());
    }

    let dc = read_line("  Discord bot token (Enter to skip): ")?;
    if !dc.is_empty() {
        config.channels.discord.token = dc;
        changed = true;
        println!("  {} Discord token saved", "✓".green());
    }

    let wa = read_line(
        "  WhatsApp bridge URL (Enter to skip, or type default for ws://localhost:3001): ",
    )?;
    if wa.eq_ignore_ascii_case("default") || wa.eq_ignore_ascii_case("d") {
        config.channels.whatsapp.bridge_url = "ws://localhost:3001".to_string();
        changed = true;
        println!("  {} WhatsApp bridge URL set to ws://localhost:3001", "✓".green());
    } else if !wa.is_empty() {
        config.channels.whatsapp.bridge_url = wa;
        changed = true;
        println!("  {} WhatsApp bridge URL saved", "✓".green());
    }

    let slack_b = read_line("  Slack bot token xoxb-... (Enter to skip): ")?;
    if !slack_b.is_empty() {
        config.channels.slack.bot_token = slack_b;
        let slack_a = read_line("  Slack app token xapp-... (required for Socket Mode): ")?;
        if slack_a.is_empty() {
            println!(
                "  {}",
                "Slack app token missing — bot token not saved.".yellow()
            );
            config.channels.slack.bot_token.clear();
        } else {
            config.channels.slack.app_token = slack_a;
            changed = true;
            println!("  {} Slack tokens saved", "✓".green());
        }
    }

    if prompt_yn("  Configure Email (IMAP + SMTP)?", false)? {
        let imap_host = read_line("  IMAP host (e.g. imap.gmail.com): ")?;
        if !imap_host.is_empty() {
            config.channels.email.imap_host = imap_host.clone();
            config.channels.email.imap_username =
                read_line("  IMAP username (often your email): ")?;
            config.channels.email.imap_password = read_line("  IMAP password / app password: ")?;
            let smtp_default = if imap_host.to_lowercase().contains("gmail") {
                "smtp.gmail.com"
            } else {
                ""
            };
            let smtp_host = read_line(&format!(
                "  SMTP host (Enter for `{smtp_default}` or type host): "
            ))?;
            config.channels.email.smtp_host = if smtp_host.is_empty() {
                smtp_default.to_string()
            } else {
                smtp_host
            };
            let same = read_line("  SMTP username (Enter = same as IMAP username): ")?;
            config.channels.email.smtp_username = if same.is_empty() {
                config.channels.email.imap_username.clone()
            } else {
                same
            };
            let sp = read_line("  SMTP password (Enter = same as IMAP password): ")?;
            config.channels.email.smtp_password = if sp.is_empty() {
                config.channels.email.imap_password.clone()
            } else {
                sp
            };
            let from = read_line("  From address (Enter = IMAP username): ")?;
            config.channels.email.from_address = if from.is_empty() {
                config.channels.email.imap_username.clone()
            } else {
                from
            };
            changed = true;
            println!("  {} Email channel saved (review ports/TLS in config if needed)", "✓".green());
        }
    }

    if changed {
        println!();
        println!(
            "  {}",
            "Channel entries saved; build Metis with matching --features to use them."
                .dimmed()
        );
    }

    Ok(changed)
}

/// Create a template file if it doesn't exist.
fn create_template(path: &std::path::Path, content: &str) -> Result<()> {
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    if path.exists() {
        println!("  {} {} already exists", "✓".green(), name);
    } else {
        std::fs::write(path, content)?;
        println!("  {} created {}", "✓".green(), name);
    }
    Ok(())
}

// ─────────────────────────────────────────────
// Templates
// ─────────────────────────────────────────────

const AGENTS_TEMPLATE: &str = r#"# Agents

Configuration and personality for your AI agents.

## Default Agent

- **Name**: Metis
- **Role**: Personal AI assistant
- **Style**: Concise, helpful, technical when needed
"#;

const USER_TEMPLATE: &str = r#"# User Profile

Tell Metis about yourself so it can personalize its responses.

## About Me

- **Name**: (your name)
- **Role**: (your role/profession)
- **Preferences**: (communication preferences)
"#;

const SOUL_TEMPLATE: &str = r#"# Soul

I am Metis, a lightweight AI assistant built in Rust.

## Personality

- Helpful and friendly
- Concise and to the point
- Curious and eager to learn

## Values

- Accuracy over speed
- User privacy and safety
- Transparency in actions
"#;

const HEARTBEAT_TEMPLATE: &str = r#"# Heartbeat Tasks

This file is checked every 30 minutes by your Metis agent.
Add tasks below that you want the agent to work on periodically.

If this file has no tasks (only headers and comments), the agent will skip the heartbeat.

## Active Tasks

<!-- Add your periodic tasks below this line -->


## Completed

<!-- Move completed tasks here or delete them -->
"#;

const MEMORY_TEMPLATE: &str = r#"# Long-term Memory

Metis persists important information here automatically.
You can also edit this file directly.
"#;

const SKILL_CREATOR_TEMPLATE: &str = include_str!("../../metis-agent/skills/skill-creator/SKILL.md");

// ─────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_template_new_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("TEST.md");
        create_template(&path, "hello").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "hello");
    }

    #[test]
    fn create_template_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("TEST.md");
        std::fs::write(&path, "original").unwrap();
        create_template(&path, "new content").unwrap();
        // Should NOT overwrite
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "original");
    }

    #[test]
    fn templates_not_empty() {
        assert!(!AGENTS_TEMPLATE.is_empty());
        assert!(!USER_TEMPLATE.is_empty());
        assert!(!MEMORY_TEMPLATE.is_empty());
    }

    #[test]
    fn user_profile_template_uses_values() {
        let rendered = user_profile_template("Alex", "Engineer", "Concise and direct");
        assert!(rendered.contains("**Name**: Alex"));
        assert!(rendered.contains("**Role**: Engineer"));
        assert!(rendered.contains("**Communication Style**: Concise and direct"));
    }

    #[test]
    fn user_profile_template_uses_placeholders_when_blank() {
        let rendered = user_profile_template("", "", "");
        assert!(rendered.contains("**Name**: (your name)"));
        assert!(rendered.contains("**Role**: (your role/profession)"));
        assert!(rendered.contains("**Communication Style**: (communication preferences)"));
    }

    #[test]
    fn agent_profile_template_uses_name() {
        let rendered = agent_profile_template("Ava");
        assert!(rendered.contains("**Name**: Ava"));
        assert!(rendered.contains("## Default Agent"));
    }
}
