//! Metis CLI — entry point.
//!
//! Replaces nanobot's `cli/commands.py` (Typer app).
//!
//! # Commands
//!
//! - `Metis agent [-m MESSAGE] [-s SESSION]` — main chat (single-shot or REPL)
//! - `Metis onboard` — initialize config + workspace
//! - `Metis status` — show configuration and provider status
//! - `Metis serve` — local HTTP API for the agent (Axum; loopback by default)

mod helpers;
mod onboard;
mod repl;
mod status;
mod gateway;
mod cron_cmd;
mod channels_cmd;
mod serve;

use std::sync::Arc;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use tracing::info;

use metis_agent::{AgentLoop, ExecToolConfig, OutboundFormatting};
use metis_core::bus::queue::MessageBus;
use metis_core::config::{load_config, Config};
use metis_core::session::SessionManager;
use metis_providers::http_provider::create_provider;

// ─────────────────────────────────────────────
// CLI definition
// ─────────────────────────────────────────────

/// 🦀 Metis — Ultra-lightweight AI assistant in Rust
#[derive(Parser)]
#[command(name = "metis", version, about, long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Chat with the AI agent (single-shot or interactive REPL)
    Agent {
        /// Single message (non-interactive). Omit for REPL mode.
        #[arg(short, long)]
        message: Option<String>,

        /// Session identifier (format: "channel:id")
        #[arg(short, long, default_value = "cli:default")]
        session: String,

        /// Disable Markdown rendering in output
        #[arg(long, default_value_t = false)]
        no_markdown: bool,

        /// Enable debug logging
        #[arg(long, default_value_t = false)]
        logs: bool,
    },

    /// Initialize configuration and workspace (guided provider, model, and channels when run in a TTY)
    Onboard {
        /// Skip interactive prompts (files + default config only; use in scripts / CI)
        #[arg(long, alias = "defaults-only")]
        non_interactive: bool,
        /// Re-onboard profile only (ask who you are and communication style)
        #[arg(long)]
        profile_only: bool,
    },

    /// Show configuration and provider status
    Status,

    /// Start the gateway (all channels + agent loop)
    Gateway {
        /// Enable debug logging
        #[arg(long, default_value_t = false)]
        logs: bool,
        /// Restart existing gateway instance before starting
        #[arg(long, default_value_t = false)]
        restart: bool,
    },

    /// Manage scheduled tasks
    Cron {
        #[command(subcommand)]
        action: cron_cmd::CronCommands,
    },

    /// Manage chat channels
    Channels {
        #[command(subcommand)]
        action: channels_cmd::ChannelsCommands,
    },

    /// Start a local HTTP API for the agent (Axum; loopback by default)
    Serve {
        /// Bind address (overrides `httpServer.host` in config)
        #[arg(long)]
        host: Option<String>,

        /// TCP port (overrides `httpServer.port` in config)
        #[arg(long)]
        port: Option<u16>,

        /// API bearer token (overrides `httpServer.apiKey`; empty disables auth)
        #[arg(long)]
        api_key: Option<String>,

        /// Enable debug logging
        #[arg(long, default_value_t = false)]
        logs: bool,
    },
}

// ─────────────────────────────────────────────
// Entrypoint
// ─────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Agent {
            message,
            session,
            no_markdown,
            logs,
        } => {
            init_logging(logs);
            run_agent(message, session, !no_markdown, logs).await
        }
        Commands::Onboard {
            non_interactive,
            profile_only,
        } => onboard::run(non_interactive, profile_only),
        Commands::Status => status::run(),
        Commands::Gateway { logs, restart } => {
            init_logging(logs);
            if restart {
                gateway::restart().await
            } else {
                gateway::run().await
            }
        }
        Commands::Cron { action } => {
            init_logging(false);
            cron_cmd::dispatch(action).await
        }
        Commands::Channels { action } => channels_cmd::dispatch(action),
        Commands::Serve {
            host,
            port,
            api_key,
            logs,
        } => serve::run(host, port, api_key, logs).await,
    }
}

// ─────────────────────────────────────────────
// Agent command
// ─────────────────────────────────────────────

async fn run_agent(
    message: Option<String>,
    session_id: String,
    render_markdown: bool,
    show_logs: bool,
) -> Result<()> {
    let config = load_config(None);
    let agent_loop = build_agent_loop(&config)?;

    match message {
        Some(msg) => {
            // Single-shot mode
            info!(session = %session_id, "processing single message");
            let response = agent_loop
                .process_direct(&msg)
                .await
                .context("agent processing failed")?;
            helpers::print_response(&response, render_markdown);
        }
        None => {
            // Interactive REPL mode
            repl::run(agent_loop, &session_id, render_markdown, show_logs).await?;
        }
    }

    Ok(())
}

/// Build an `AgentLoop` from the loaded configuration.
pub fn build_agent_loop(config: &Config) -> Result<AgentLoop> {
    let defaults = &config.agents.defaults;

    // Resolve workspace path (expand ~)
    let workspace = helpers::expand_tilde(&defaults.workspace);
    std::fs::create_dir_all(&workspace)
        .with_context(|| format!("failed to create workspace: {}", workspace.display()))?;

    // Resolve model
    let model = &defaults.model;

    // Create provider
    let providers_map = config.providers.to_map();
    let provider = create_provider(model, &providers_map)
        .map_err(|e| anyhow::anyhow!(e))?;

    // Brave API key
    let brave_key = if config.tools.web.search.api_key.is_empty() {
        None
    } else {
        Some(config.tools.web.search.api_key.clone())
    };

    // Build agent loop
    let bus = Arc::new(MessageBus::new(100));
    let session_manager = SessionManager::new(None)
        .context("failed to create session manager")?;

    let agent_name = helpers::load_agent_name(&workspace);
    let exec_cfg = ExecToolConfig {
        timeout: config.tools.exec.timeout,
        shell: config.tools.exec.shell.clone(),
        permission_mode: config.tools.exec.permission_mode.clone(),
    };
    let outbound = OutboundFormatting {
        log_thinking_json: defaults.log_thinking_json,
        include_fenced_code_in_chat_apps: defaults.include_fenced_code_in_chat_apps,
    };
    let agent_loop = AgentLoop::new(
        bus,
        Arc::new(provider),
        workspace,
        Some(model.to_string()),
        Some(defaults.max_tool_iterations as usize),
        None, // uses defaults for temperature/max_tokens
        brave_key,
        Some(exec_cfg),
        config.tools.restrict_to_workspace,
        Some(session_manager),
        agent_name,
        Some(outbound),
    );

    Ok(agent_loop)
}

/// Initialize tracing/logging.
fn init_logging(verbose: bool) {
    use tracing_subscriber::EnvFilter;

    let filter = if verbose {
        EnvFilter::new("Metis=debug,metis_thinking=debug,info")
    } else {
        EnvFilter::new("warn")
    };

    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .compact()
        .init();
}
