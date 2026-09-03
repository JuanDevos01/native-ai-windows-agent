//! Gateway command — orchestrates channels, agent loop, and message routing.
//!
//! Port of nanobot's gateway command from `cli/commands.py`.
//!
//! Startup sequence:
//! 1. Load config
//! 2. Create message bus
//! 3. Create agent loop (with provider, tools, sessions)
//! 4. Create channel manager, register enabled channels
//! 5. Run: `tokio::select!` of agent loop + channel manager
//! 6. Handle Ctrl+C for graceful shutdown

use std::sync::Arc;
use std::path::PathBuf;

use anyhow::{Context, Result};
use tracing::info;

use metis_agent::{AgentLoop, ExecToolConfig, OutboundFormatting};
use metis_channels::ChannelManager;
use metis_core::bus::queue::MessageBus;
use metis_core::config::load_config;
use metis_core::heartbeat::HeartbeatService;
use metis_core::session::SessionManager;
use metis_cron::CronService;
use metis_providers::http_provider::create_provider;

use crate::helpers;

fn gateway_pid_path() -> PathBuf {
    metis_core::utils::get_data_path().join("gateway.pid")
}

fn write_gateway_pid() -> Result<()> {
    let pid_path = gateway_pid_path();
    if let Some(parent) = pid_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create pid dir: {}", parent.display()))?;
    }
    std::fs::write(&pid_path, std::process::id().to_string())
        .with_context(|| format!("failed to write pid file: {}", pid_path.display()))?;
    Ok(())
}

fn remove_gateway_pid() {
    let pid_path = gateway_pid_path();
    let _ = std::fs::remove_file(pid_path);
}

/// Stop an existing gateway process if a PID file is present.
fn stop_existing_gateway_process() -> Result<bool> {
    let pid_path = gateway_pid_path();
    if !pid_path.exists() {
        return Ok(false);
    }
    let content = std::fs::read_to_string(&pid_path)
        .with_context(|| format!("failed to read pid file: {}", pid_path.display()))?;
    let pid: u32 = match content.trim().parse() {
        Ok(p) => p,
        Err(_) => {
            // stale/corrupted pid file
            let _ = std::fs::remove_file(&pid_path);
            return Ok(false);
        }
    };
    if pid == std::process::id() {
        return Ok(false);
    }

    #[cfg(target_os = "windows")]
    {
        let output = std::process::Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .output()
            .with_context(|| "failed to run taskkill")?;
        // The pid file can outlive the process it names (a previous crash,
        // an external kill, this same pid getting reused by something else
        // entirely) — "process not found" means the goal ("nothing is
        // running under that pid") is already met, so it isn't a failure.
        // Only a real problem stopping a process that IS still there
        // (e.g. permission denied) should block starting the new gateway.
        let already_gone = !output.status.success()
            && String::from_utf8_lossy(&output.stderr).to_lowercase().contains("not found");
        if !output.status.success() && !already_gone {
            anyhow::bail!(
                "failed to stop existing gateway pid {pid} (taskkill exit: {}): {}",
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
    }

    #[cfg(not(target_os = "windows"))]
    {
        let output = std::process::Command::new("kill")
            .args(["-TERM", &pid.to_string()])
            .output()
            .with_context(|| "failed to run kill")?;
        let already_gone = !output.status.success()
            && String::from_utf8_lossy(&output.stderr).to_lowercase().contains("no such process");
        if !output.status.success() && !already_gone {
            anyhow::bail!(
                "failed to stop existing gateway pid {pid} (kill exit: {}): {}",
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
    }

    // Best-effort remove stale pid file; new process will re-create.
    let _ = std::fs::remove_file(&pid_path);
    Ok(true)
}

/// Restart gateway by stopping existing instance (if any) then starting a new one.
pub async fn restart() -> Result<()> {
    let stopped = stop_existing_gateway_process()?;
    if stopped {
        println!("  Previous gateway instance stopped.");
        tokio::time::sleep(tokio::time::Duration::from_millis(700)).await;
    } else {
        println!("  No existing gateway instance found.");
    }
    run().await
}

/// Send replies a human approved in the desktop app.
///
/// The desktop and the gateway are separate processes, so the approval queue
/// file is the only channel between them: the desktop flips an entry to
/// Approved, and this loop notices and puts it on the bus. Polling (rather
/// than watching) keeps it simple and survives either side restarting.

/// Wrap a scheduled job's prompt with the delivery contract.
///
/// The scheduler sends the agent's FINAL REPLY to the user verbatim. Without
/// saying so, the model narrates instead — a real run ended with the user
/// receiving "Done! Sent 5 headlines to Telegram." while the headlines
/// themselves went nowhere.
fn cron_delivery_prompt(message: &str) -> String {
    format!(
        "{message}\n\n[Metis scheduler note — not from the user: your final reply is \
         delivered to the user's chat EXACTLY as you write it. Write ONLY the content \
         itself. Do not send it with the message tool, do not describe what you did, do \
         not announce that something was sent, and do not add a greeting about this being \
         a scheduled task.]"
    )
}

async fn run_approval_sender(bus: Arc<MessageBus>) {
    use metis_core::approvals::{self, ApprovalStatus};
    use metis_core::bus::types::OutboundMessage;

    let path = approvals::default_path();
    loop {
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;

        let store = match approvals::load(&path) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(error = %e, "could not read approval queue");
                continue;
            }
        };
        let approved: Vec<_> = approvals::with_status(&store, ApprovalStatus::Approved)
            .into_iter()
            .cloned()
            .collect();

        for entry in approved {
            // Mark Sent BEFORE publishing. If the process dies mid-send the
            // reply is lost, which is recoverable; marking after could resend
            // the same mail on every restart, which is not.
            if let Err(e) = approvals::set_status(&path, &entry.id, ApprovalStatus::Sent, None) {
                tracing::warn!(id = %entry.id, error = %e, "could not update approval status");
                continue;
            }
            let msg = OutboundMessage::new(entry.channel.clone(), entry.to.clone(), entry.body.clone());
            if let Err(e) = bus.publish_outbound(msg).await {
                tracing::error!(id = %entry.id, error = %e, "failed to send approved reply");
                let _ = approvals::set_status(
                    &path,
                    &entry.id,
                    ApprovalStatus::Failed,
                    Some(e.to_string()),
                );
            } else {
                info!(id = %entry.id, to = %entry.to, "sent approved reply");
            }
        }
    }
}

/// Run the gateway — starts the agent loop + channel manager.
pub async fn run() -> Result<()> {
    println!();
    helpers::print_banner();
    println!("  Mode: Gateway");
    println!();
    write_gateway_pid()?;

    // 1. Load config
    let config = load_config(None);
    let defaults = &config.agents.defaults;

    // 2. Resolve workspace
    let workspace = helpers::expand_tilde(&defaults.workspace);
    std::fs::create_dir_all(&workspace)
        .with_context(|| format!("failed to create workspace: {}", workspace.display()))?;
    helpers::ensure_guide_in_workspace(&workspace);

    // 3. Create message bus (shared between agent + channels)
    let bus = Arc::new(MessageBus::new(100));

    // 4. Create provider
    let model = &defaults.model;
    let providers_map = config.providers.to_map();
    let provider = create_provider(model, &providers_map)
        .map_err(|e| anyhow::anyhow!(e))?;
    let subagent_provider =
        helpers::build_subagent_provider(&defaults.subagent_model, &providers_map);

    // 5. Brave API key
    let brave_key = if config.tools.web.search.api_key.is_empty() {
        None
    } else {
        Some(config.tools.web.search.api_key.clone())
    };

    // 6. Create session manager
    let session_manager = SessionManager::new(None)
        .context("failed to create session manager")?;

    // 7. Create agent loop (Arc-wrapped for sharing with cron callback)
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
    let memory_settings = crate::agent_builder::build_memory_settings(&config, &providers_map);
    let agent_loop = Arc::new(
        AgentLoop::new(
            bus.clone(),
            Arc::new(provider),
            workspace.clone(),
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
        .with_direct_chat_context(defaults.chat_context_length),
    );

    // 8. Create cron service
    let cron_service = Arc::new(CronService::new(bus.clone(), None));
    {
        let agent = agent_loop.clone();
        cron_service
            .set_on_job(Arc::new(move |job: metis_cron::CronJob| {
                let agent = agent.clone();
                Box::pin(async move {
                    // Date-scoped session: a recurring job that reuses one
                    // session forever accumulates its own past answers, and
                    // the model starts replying "I already did this" to a
                    // prompt it genuinely has answered — yesterday. Each day
                    // starts clean.
                    let session_id =
                        format!("{}-{}", job.id, chrono::Utc::now().format("%Y%m%d"));

                    let response = if job.payload.deliver {
                        let to = job.payload.to.clone().unwrap_or_default();
                        let channel = job
                            .payload
                            .channel
                            .clone()
                            .unwrap_or_else(|| "telegram".to_string());
                        let prompt = cron_delivery_prompt(&job.payload.message);
                        agent
                            .process_direct_session_with_reply(
                                "cron", &session_id, &prompt, &channel, &to,
                            )
                            .await
                    } else {
                        agent
                            .process_direct_session("cron", &session_id, &job.payload.message)
                            .await
                    }
                    .unwrap_or_else(|e| format!("Error: {e}"));

                    Ok(response)
                })
            }))
            .await;
    }

    // Pre-load to show job count in banner
    if let Err(e) = cron_service.load().await {
        tracing::warn!(error = %e, "failed to pre-load cron store");
    }
    let cron_jobs = cron_service.list_jobs().await;

    // 9. Create heartbeat service (isolated session `heartbeat:default`; not sent to Telegram)
    let heartbeat = {
        let agent = agent_loop.clone();
        let callback: metis_core::heartbeat::OnHeartbeatFn = Arc::new(move |prompt| {
            let agent = agent.clone();
            Box::pin(async move {
                agent
                    .process_direct_session("heartbeat", "default", &prompt)
                    .await
            })
        });
        Arc::new(HeartbeatService::new(
            workspace.clone(),
            Some(bus.clone()),
            Some(callback),
            Some(config.heartbeat.interval_seconds()),
            config.heartbeat.enabled,
        ))
    };

    // 10. Create channel manager
    // Register configured channels
    #[allow(unused_mut)]
    let mut channel_manager = ChannelManager::new(bus.clone());

    // Telegram
    #[cfg(feature = "telegram")]
    {
        let tg = &config.channels.telegram;
        if !tg.token.is_empty() {
            use metis_channels::telegram::TelegramChannel;
            let mut telegram = TelegramChannel::new(
                tg.token.clone(),
                bus.clone(),
                tg.allowed_users.clone(),
            );

            // Wire voice transcription if configured.
            //
            // - Provider `groq` (default): Groq-hosted Whisper over HTTPS (needs API key).
            // - `local` / `openai_compatible` / …: POST to an OpenAI-compatible `/audio/transcriptions` endpoint
            //   (typical local Whisper setups). Requires `transcription.apiBase`; API key optional.
            if config.transcription.enabled {
                let tc = &config.transcription;
                let model_eff = tc.model.trim();
                let model_eff = if model_eff.is_empty() {
                    "whisper-large-v3"
                } else {
                    model_eff
                };

                let prov_lc = tc.provider.trim().to_lowercase();

                enum TranscriptionBackend {
                    LocalOpenAiCompatible,
                    WhisperCpp,
                    Groq,
                }

                let backend = match prov_lc.as_str() {
                    "local"
                    | "openai_compatible"
                    | "custom"
                    | "openai-http"
                    | "openai_http"
                    | "local_whisper"
                    | "localwhisper"
                    => TranscriptionBackend::LocalOpenAiCompatible,
                    "whisper_cpp"
                    | "whispercpp"
                    | "whisper.cpp"
                    | "whisper-cpp"
                    | "cpp"
                    => TranscriptionBackend::WhisperCpp,
                    "groq" | "" => TranscriptionBackend::Groq,
                    unknown => {
                        if !unknown.is_empty() {
                            tracing::warn!(
                                provider = unknown,
                                "unknown transcription.provider — assuming Groq-hosted Whisper",
                            );
                        }
                        TranscriptionBackend::Groq
                    }
                };

                use metis_providers::{OpenAiCompatibleTranscriber, TranscriptionProvider, WhisperCppTranscriber};

                let transcriber: Option<Arc<dyn TranscriptionProvider>> = match backend {
                    TranscriptionBackend::LocalOpenAiCompatible => {
                        if let Some(ref base) = tc
                            .api_base
                            .as_ref()
                            .map(|s| s.trim())
                            .filter(|s| !s.is_empty())
                        {
                            let bearer = tc.api_key.trim();
                            let tok = (!bearer.is_empty()).then(|| bearer.to_string());
                            let t = OpenAiCompatibleTranscriber::local_openai_base(base, tok, model_eff);
                            info!(
                                display = %t.display_name(),
                                base = base,
                                "voice transcription enabled (OpenAI-compatible / local Whisper HTTP)",
                            );
                            Some(Arc::new(t))
                        } else {
                            tracing::warn!(
                                provider = %tc.provider.trim(),
                                "telegram voice notes: transcription.apiBase is required when using a local/OpenAI-compatible transcription provider",
                            );
                            println!(
                                "  ⚠  Voice transcription: disabled (provider={:?} requires transcription.apiBase)",
                                tc.provider.trim()
                            );
                            None
                        }
                    }
                    TranscriptionBackend::WhisperCpp => {
                        let wc = tc.whisper_cpp.as_ref();
                        let exe = wc
                            .and_then(|c| c.exe_path.as_ref())
                            .map(|s| s.trim())
                            .filter(|s| !s.is_empty())
                            .unwrap_or("whisper-cli.exe");
                        let model_path = wc.map(|c| c.model_path.trim()).unwrap_or("");
                        let extra_args: Vec<String> = wc
                            .map(|c| c.extra_args.clone())
                            .unwrap_or_default();

                        if model_path.is_empty() {
                            tracing::warn!(
                                provider = %tc.provider.trim(),
                                "telegram voice notes: transcription.provider=whisper_cpp requires transcription.whisperCpp.modelPath",
                            );
                            println!(
                                "  ⚠  Voice transcription: disabled (provider={:?} requires transcription.whisperCpp.modelPath)",
                                tc.provider.trim()
                            );
                            None
                        } else {
                            let t = WhisperCppTranscriber::new(exe, model_path.to_string(), extra_args);
                            info!(
                                display = %t.display_name(),
                                exe = exe,
                                model_path = model_path,
                                "voice transcription enabled (whisper.cpp CLI)"
                            );
                            Some(Arc::new(t))
                        }
                    }
                    TranscriptionBackend::Groq => {
                        let transcription_key = if !tc.api_key.is_empty() {
                            tc.api_key.clone()
                        } else if !config.providers.groq.api_key.is_empty() {
                            config.providers.groq.api_key.clone()
                        } else {
                            std::env::var("GROQ_API_KEY").unwrap_or_default()
                        };

                        let t = OpenAiCompatibleTranscriber::groq_cloud(
                            &transcription_key,
                            tc.api_base
                                .as_deref()
                                .map(str::trim)
                                .filter(|s| !s.is_empty()),
                            model_eff,
                        );

                        if !t.is_ready() {
                            tracing::warn!(
                                "telegram voice notes: transcription.provider=groq but no usable Groq API key — set transcription.apiKey, providers.groq.apiKey, or GROQ_API_KEY",
                            );
                            println!(
                                "  ⚠  Voice transcription: disabled (missing Groq API key — set transcription.apiKey, providers.groq.apiKey, or GROQ_API_KEY)",
                            );
                            None
                        } else {
                            info!(display = %t.display_name(), "voice transcription enabled (Groq)");
                            Some(Arc::new(t))
                        }
                    }
                };

                if let Some(t_voice) = transcriber {
                    let transcribe_arc = Arc::clone(&t_voice);
                    telegram = telegram.with_transcriber(Arc::new(move |path: String| {
                        let tt = Arc::clone(&transcribe_arc);
                        Box::pin(async move { tt.transcribe(std::path::Path::new(&path)).await })
                    }));
                }
            }

            channel_manager.register(Arc::new(telegram));
            info!("registered telegram channel");
        }
    }

    // Discord
    #[cfg(feature = "discord")]
    {
        let dc = &config.channels.discord;
        if !dc.token.is_empty() {
            use metis_channels::discord::DiscordChannel;
            let discord = DiscordChannel::new(
                dc.token.clone(),
                bus.clone(),
                dc.allowed_users.clone(),
            );
            channel_manager.register(Arc::new(discord));
            info!("registered discord channel");
        }
    }

    // WhatsApp
    #[cfg(feature = "whatsapp")]
    {
        let wa = &config.channels.whatsapp;
        if !wa.bridge_url.is_empty() {
            use metis_channels::whatsapp::WhatsAppChannel;
            let whatsapp = WhatsAppChannel::new(
                wa.bridge_url.clone(),
                bus.clone(),
                wa.allowed_users.clone(),
            );
            channel_manager.register(Arc::new(whatsapp));
            info!("registered whatsapp channel");
        }
    }

    // Slack
    #[cfg(feature = "slack")]
    {
        let sl = &config.channels.slack;
        if !sl.bot_token.is_empty() && !sl.app_token.is_empty() {
            use metis_channels::slack::SlackChannel;
            let slack = SlackChannel::new(sl.clone(), bus.clone());
            channel_manager.register(Arc::new(slack));
            info!("registered slack channel");
        }
    }

    // Email
    #[cfg(feature = "email")]
    {
        let em = &config.channels.email;
        // A Graph mailbox has no imap_host, so gating purely on that would
        // silently never start the channel for Office 365.
        let graph_ready = em.provider.trim().eq_ignore_ascii_case("graph")
            && !em.graph_tenant_id.is_empty()
            && !em.graph_client_id.is_empty()
            && !em.graph_client_secret.is_empty()
            && !em.graph_user_id.is_empty();
        if !em.imap_host.is_empty() || graph_ready {
            use metis_channels::email::EmailChannel;
            let email = EmailChannel::new(em.clone(), bus.clone());
            channel_manager.register(Arc::new(email));
            info!("registered email channel");
        }
    }
    info!(
        version = %metis_core::build::version_line(),
        model = %model,
        workspace = %workspace.display(),
        channels = ?channel_manager.channel_names(),
        "gateway starting"
    );

    println!(
        "  Version:   {}",
        metis_core::build::version_line()
    );
    println!(
        "  Model:     {}",
        model
    );
    println!(
        "  Workspace: {}",
        workspace.display()
    );
    println!(
        "  Channels:  {} registered",
        channel_manager.len()
    );
    if !cron_jobs.is_empty() {
        let enabled = cron_jobs.iter().filter(|j| j.enabled).count();
        println!("  Cron:      {} jobs ({} enabled)", cron_jobs.len(), enabled);
    }
    if config.heartbeat.enabled {
        println!("  Heartbeat: every {}m", config.heartbeat.interval_minutes.max(1));
    } else {
        println!("  Heartbeat: disabled");
    }
    println!();

    if channel_manager.is_empty() {
        println!("  ⚠  No channels registered. The agent loop will run but");
        println!("     only process messages from the internal bus.");
        println!("     Configure channels in ~/.metis/config.json");
        println!();
    }

    println!("  Ctrl+C to stop");
    println!();

    // 11. Run: agent loop + channel manager + cron + heartbeat concurrently
    //     Ctrl+C triggers graceful shutdown
    tokio::select! {
        _ = agent_loop.run() => {
            info!("agent loop exited");
        }
        result = channel_manager.start_all() => {
            if let Err(e) = result {
                tracing::error!(error = %e, "channel manager error");
            }
        }
        result = cron_service.start() => {
            if let Err(e) = result {
                tracing::error!(error = %e, "cron service error");
            }
        }
        result = heartbeat.start() => {
            if let Err(e) = result {
                tracing::error!(error = %e, "heartbeat service error");
            }
        }
        _ = run_approval_sender(bus.clone()) => {
            info!("approval sender exited");
        }
        _ = tokio::signal::ctrl_c() => {
            println!();
            println!("  Shutting down...");
            info!("received Ctrl+C, shutting down");
            heartbeat.stop();
            cron_service.stop().await;
            channel_manager.stop_all().await;
        }
    }
    remove_gateway_pid();

    println!("  Gateway stopped. Goodbye!");
    Ok(())
}

// ─────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────

#[cfg(test)]
mod tests {
    // Gateway integration tests would require a full runtime environment.
    // The component tests are in metis-channels and metis-agent crates.
    // Here we just verify the module compiles and the imports work.

    #[test]
    fn test_module_compiles() {
        // If this test runs, the gateway module compiles correctly
        assert!(true);
    }
}
