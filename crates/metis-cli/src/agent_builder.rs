//! Shared agent loop construction for CLI, serve, gateway, and desktop.

use std::sync::Arc;

use anyhow::{Context, Result};

use metis_agent::tools::sharepoint::SharePointSettings;
use metis_agent::{AgentLoop, ExecToolConfig, MemorySettings, OutboundFormatting};
use metis_core::bus::queue::MessageBus;
use metis_core::config::Config;
use metis_core::session::SessionManager;
use metis_providers::http_provider::create_provider;
use metis_providers::registry::ProviderConfig;
use metis_providers::traits::LlmProvider;

use crate::helpers;

/// Build semantic memory settings from config, creating the embedding
/// provider when one is configured. Embedding-provider failures degrade to
/// keyword-only search instead of failing startup.
pub fn build_memory_settings(
    config: &Config,
    providers_map: &std::collections::HashMap<String, ProviderConfig>,
) -> MemorySettings {
    let mc = &config.memory;
    let embed_model = mc.embedding_model.trim().to_string();
    let embed_provider: Option<Arc<dyn LlmProvider>> = if mc.enabled && !embed_model.is_empty() {
        match create_provider(&embed_model, providers_map) {
            Ok(p) => Some(Arc::new(p)),
            Err(e) => {
                tracing::warn!(
                    model = %embed_model,
                    error = %e,
                    "no provider for embedding model; memory falls back to keyword search"
                );
                None
            }
        }
    } else {
        None
    };

    MemorySettings {
        enabled: mc.enabled,
        db_path: None,
        embed_provider,
        embed_model,
        compaction_threshold: mc.compaction_threshold as usize,
        keep_recent: mc.keep_recent as usize,
        top_k: mc.top_k as usize,
    }
}

/// Build an `AgentLoop` from the loaded configuration.
pub fn build_agent_loop(config: &Config) -> Result<AgentLoop> {
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
    let session_manager = SessionManager::new(None).context("failed to create session manager")?;

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
        show_token_usage: defaults.show_token_usage,
    };

    let memory_settings = build_memory_settings(config, &providers_map);
    let sharepoint = build_sharepoint_settings(config);
    let sp_workspace = workspace.clone();

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
    )
    .with_memory(memory_settings)
    .with_sharepoint(sharepoint, sp_workspace)
    .with_direct_chat_context(defaults.chat_context_length))
}

/// Build SharePoint settings, falling back to the Graph app configured for
/// email so the client secret is stored in exactly one place.
pub fn build_sharepoint_settings(config: &Config) -> SharePointSettings {
    let sp = &config.tools.sharepoint;
    let mail = &config.channels.email;
    let pick = |specific: &str, shared: &str| {
        let s = specific.trim();
        if s.is_empty() { shared.trim().to_string() } else { s.to_string() }
    };
    SharePointSettings {
        enabled: sp.enabled,
        sites: sp
            .sites
            .iter()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect(),
        tenant_id: pick(&sp.tenant_id, &mail.graph_tenant_id),
        client_id: pick(&sp.client_id, &mail.graph_client_id),
        client_secret: pick(&sp.client_secret, &mail.graph_client_secret),
    }
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

#[cfg(test)]
mod sharepoint_settings_tests {
    use super::*;

    fn base() -> Config {
        let mut c = Config::default();
        c.channels.email.graph_tenant_id = "mail-tenant".into();
        c.channels.email.graph_client_id = "mail-client".into();
        c.channels.email.graph_client_secret = "mail-secret".into();
        c
    }

    #[test]
    fn credentials_fall_back_to_the_email_graph_app() {
        // One Azure app, one secret, stored once.
        let mut c = base();
        c.tools.sharepoint.enabled = true;
        c.tools.sharepoint.sites = vec!["host:/sites/Finance".into()];

        let s = build_sharepoint_settings(&c);
        assert_eq!(s.tenant_id, "mail-tenant");
        assert_eq!(s.client_id, "mail-client");
        assert_eq!(s.client_secret, "mail-secret");
        assert!(s.is_usable());
    }

    #[test]
    fn explicit_credentials_win_over_the_email_app() {
        let mut c = base();
        c.tools.sharepoint.enabled = true;
        c.tools.sharepoint.sites = vec!["host:/sites/Finance".into()];
        c.tools.sharepoint.tenant_id = "own-tenant".into();
        c.tools.sharepoint.client_id = "own-client".into();
        c.tools.sharepoint.client_secret = "own-secret".into();

        let s = build_sharepoint_settings(&c);
        assert_eq!(s.tenant_id, "own-tenant");
        assert_eq!(s.client_secret, "own-secret");
    }

    #[test]
    fn disabled_by_default_even_when_email_graph_is_configured() {
        // Configuring Graph for mail must never silently hand the agent
        // SharePoint as well.
        let s = build_sharepoint_settings(&base());
        assert!(!s.enabled);
        assert!(!s.is_usable(), "must not be usable without opting in");
    }

    #[test]
    fn enabled_without_sites_is_still_not_usable() {
        let mut c = base();
        c.tools.sharepoint.enabled = true;
        let s = build_sharepoint_settings(&c);
        assert!(!s.is_usable(), "no site means nothing was authorized");
    }

    #[test]
    fn blank_site_entries_are_dropped() {
        let mut c = base();
        c.tools.sharepoint.enabled = true;
        c.tools.sharepoint.sites =
            vec!["  ".into(), "host:/sites/Finance".into(), "".into()];
        let s = build_sharepoint_settings(&c);
        assert_eq!(s.sites, vec!["host:/sites/Finance".to_string()]);
        assert!(s.is_usable());
    }
}
