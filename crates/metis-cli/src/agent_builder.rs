//! Shared agent loop construction for CLI, serve, gateway, and desktop.

use std::sync::Arc;

use anyhow::{Context, Result};

use metis_agent::{AgentLoop, ExecToolConfig, OutboundFormatting};
use metis_core::bus::queue::MessageBus;
use metis_core::config::Config;
use metis_core::session::SessionManager;
use metis_providers::http_provider::create_provider;
use metis_providers::LlmProvider;

use crate::helpers;

/// Build an `AgentLoop` from the loaded configuration.
///
/// Pass a shared [`SessionManager`] when the caller also reads/writes sessions (e.g. desktop UI).
pub fn build_agent_loop(
    config: &Config,
    sessions: Option<Arc<SessionManager>>,
) -> Result<AgentLoop> {
    let defaults = &config.agents.defaults;

    let workspace = helpers::expand_tilde(&defaults.workspace);
    std::fs::create_dir_all(&workspace)
        .with_context(|| format!("failed to create workspace: {}", workspace.display()))?;
    helpers::ensure_guide_in_workspace(&workspace);

    let model = &defaults.model;
    let providers_map = config.providers.to_map();
    let provider = create_provider(model, &providers_map).map_err(|e| anyhow::anyhow!(e))?;
    let subagent_provider =
        helpers::build_subagent_provider(&defaults.subagent_model, &providers_map);

    let brave_key = if config.tools.web.search.api_key.is_empty() {
        None
    } else {
        Some(config.tools.web.search.api_key.clone())
    };

    let bus = Arc::new(MessageBus::new(100));
    let session_manager = sessions.unwrap_or_else(|| {
        Arc::new(SessionManager::new(None).expect("failed to create session manager"))
    });

    let agent_name = helpers::load_agent_name(&workspace);
    let exec_cfg = ExecToolConfig {
        timeout: config.tools.exec.timeout,
        shell: config.tools.exec.shell.clone(),
        permission_mode: config.tools.exec.permission_mode.clone(),
    };
    let outbound = OutboundFormatting {
        log_thinking_json: defaults.log_thinking_json,
        include_fenced_code_in_chat_apps: defaults.include_fenced_code_in_chat_apps,
        include_exec_output_in_chat_apps: defaults.include_exec_output_in_chat_apps,
    };

    Ok(AgentLoop::new(
        bus,
        Arc::new(provider),
        workspace,
        Some(model.to_string()),
        Some(defaults.subagent_model.clone()),
        subagent_provider,
        Some(defaults.max_tool_iterations as usize),
        None,
        brave_key,
        Some(exec_cfg),
        config.tools.restrict_to_workspace,
        Some(session_manager),
        agent_name,
        Some(outbound),
    ))
}

/// Create an LLM provider for the given model id using config credentials.
pub fn provider_for_model(config: &Config, model: &str) -> Result<Arc<dyn LlmProvider>> {
    let providers_map = config.providers.to_map();
    create_provider(model, &providers_map)
        .map(|provider| Arc::new(provider) as Arc<dyn LlmProvider>)
        .map_err(|e| anyhow::anyhow!(e))
}

/// Initialize tracing/logging.
pub fn init_logging(verbose: bool) {
    use tracing_subscriber::EnvFilter;

    let filter = if verbose {
        EnvFilter::new("Metis=debug,metis_thinking=debug,info")
    } else {
        EnvFilter::new("warn")
    };

    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .compact()
        .try_init();
}
