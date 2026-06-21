//! `metis heartbeat` — run one heartbeat tick manually (for testing).

use std::sync::Arc;

use anyhow::{Context, Result};
use clap::Args;

use metis_core::config::load_config;
use metis_core::heartbeat::HeartbeatService;

use crate::helpers;
use crate::agent_builder::build_agent_loop;

/// Run a single heartbeat tick against the configured workspace.
#[derive(Args)]
pub struct HeartbeatArgs {
    /// Run even when HEARTBEAT.md has no actionable tasks
    #[arg(long, default_value_t = false)]
    force: bool,
}

pub async fn run(args: HeartbeatArgs) -> Result<()> {
    let config = load_config(None);
    let workspace = helpers::expand_tilde(&config.agents.defaults.workspace);
    std::fs::create_dir_all(&workspace)
        .with_context(|| format!("failed to create workspace: {}", workspace.display()))?;

    let agent = Arc::new(build_agent_loop(&config, None)?);
    let callback: metis_core::heartbeat::OnHeartbeatFn = Arc::new(move |prompt| {
        let agent = agent.clone();
        Box::pin(async move {
            agent
                .process_direct_session("heartbeat", "manual", &prompt)
                .await
        })
    });

    let service = HeartbeatService::new(
        workspace.clone(),
        None,
        Some(callback),
        None,
        true,
    );

    let path = service.heartbeat_file();
    println!("  Workspace: {}", workspace.display());
    println!("  HEARTBEAT.md: {}", path.display());

    if service.is_file_empty() && !args.force {
        println!();
        println!("  No actionable tasks in HEARTBEAT.md (headers/comments only).");
        println!("  Add tasks under ## Active Tasks, or run: metis heartbeat --force");
        return Ok(());
    }

    println!();
    println!("  Running heartbeat tick…");
    let Some(result) = service.trigger_now(args.force).await else {
        anyhow::bail!("heartbeat callback not configured");
    };
    let response = result.context("heartbeat agent run failed")?;
    helpers::print_response(&response, false);
    Ok(())
}
