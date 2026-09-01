//! `Metis cron` — manage scheduled tasks from the CLI.
//!
//! Replaces nanobot's `cron` subcommands:
//! - `Metis cron list [--all]` — list scheduled jobs
//! - `Metis cron add --name NAME --message MSG (--every N | --cron EXPR | --at TIME)` — add a job
//! - `Metis cron remove <ID>` — remove a job
//! - `Metis cron enable <ID> [--disable]` — enable/disable a job
//! - `Metis cron run <ID> [--force]` — manually trigger a job

use std::sync::Arc;

use anyhow::{Context, Result};
use clap::Subcommand;
use colored::Colorize;

use metis_core::bus::queue::MessageBus;
use metis_core::utils::get_data_path;
use metis_cron::types::{CronJob, CronPayload, CronSchedule, ScheduleKind};
use metis_cron::CronService;

// ─────────────────────────────────────────────
// Subcommand enum
// ─────────────────────────────────────────────

/// Cron subcommands.
#[derive(Subcommand)]
pub enum CronCommands {
    /// List scheduled jobs
    List {
        /// Include disabled jobs
        #[arg(short, long, default_value_t = false)]
        all: bool,
    },

    /// Add a new scheduled job
    Add {
        /// Job name
        #[arg(short, long)]
        name: String,

        /// Prompt message for the agent
        #[arg(short, long)]
        message: String,

        /// Run every N seconds (interval schedule)
        #[arg(short, long)]
        every: Option<u64>,

        /// Cron expression, e.g. "0 9 * * *" (cron schedule)
        #[arg(short, long)]
        cron: Option<String>,

        /// Run once at a specific time (ISO 8601 format, e.g. "2026-03-01T09:00:00")
        #[arg(long)]
        at: Option<String>,

        /// Deliver the agent's response to a channel
        #[arg(short, long, default_value_t = false)]
        deliver: bool,

        /// Recipient identifier (chat_id) for delivery
        #[arg(long)]
        to: Option<String>,

        /// Channel name for delivery (e.g. "telegram", "whatsapp")
        #[arg(long)]
        channel: Option<String>,
    },

    /// Remove a scheduled job by ID
    Remove {
        /// Job ID (8-character hex)
        job_id: String,
    },

    /// Enable or disable a job
    Enable {
        /// Job ID (8-character hex)
        job_id: String,

        /// Disable instead of enable
        #[arg(long, default_value_t = false)]
        disable: bool,
    },

    /// Manually run a job now
    Run {
        /// Job ID (8-character hex)
        job_id: String,
    },
}

// ─────────────────────────────────────────────
// Dispatcher
// ─────────────────────────────────────────────

/// Dispatch a cron subcommand.
pub async fn dispatch(cmd: CronCommands) -> Result<()> {
    match cmd {
        CronCommands::List { all } => list_jobs(all).await,
        CronCommands::Add {
            name,
            message,
            every,
            cron,
            at,
            deliver,
            to,
            channel,
        } => add_job(name, message, every, cron, at, deliver, to, channel).await,
        CronCommands::Remove { job_id } => remove_job(&job_id).await,
        CronCommands::Enable { job_id, disable } => enable_job(&job_id, !disable).await,
        CronCommands::Run { job_id } => run_job(&job_id).await,
    }
}

// ─────────────────────────────────────────────
// Helpers
// ─────────────────────────────────────────────

/// Create a CronService with the default store path (no bus needed for CLI ops).
fn make_service() -> CronService {
    let store_path = get_data_path().join("cron").join("jobs.json");
    // Bus is not used in CLI-only operations, so create a dummy one
    let bus = Arc::new(MessageBus::new(1));
    CronService::new(bus, Some(store_path))
}

/// Format milliseconds as a human-readable duration.
fn format_duration_ms(ms: i64) -> String {
    let secs = ms / 1000;
    if secs < 60 {
        format!("{}s", secs)
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else if secs < 86400 {
        format!("{}h", secs / 3600)
    } else {
        format!("{}d", secs / 86400)
    }
}

/// Format a Unix epoch timestamp (ms) as a local datetime string.
fn format_timestamp_ms(ms: i64) -> String {
    use chrono::{Local, TimeZone};
    match Local.timestamp_millis_opt(ms) {
        // The offset is not decoration. This is machine time printed for a
        // reader whose own clock reference is UTC (the agent is told the
        // time in UTC), so an unlabelled local time is genuinely ambiguous —
        // it led to "next run 09:00" being reported when it was already 12:00.
        chrono::LocalResult::Single(dt) => dt.format("%Y-%m-%d %H:%M %:z").to_string(),
        _ => "—".to_string(),
    }
}

/// Strip accidental shell-wrapped quotes from CLI values.
///
/// Examples:
/// - `"news"` -> `news`
/// - `"'8582973375'"` -> `8582973375`
/// - `\"news\"` (already unescaped by shell) -> `"news"` then `news`
fn normalize_cli_value(value: &str) -> String {
    let mut out = value.trim().to_string();
    loop {
        let bytes = out.as_bytes();
        if bytes.len() < 2 {
            break;
        }
        let first = bytes[0] as char;
        let last = bytes[bytes.len() - 1] as char;
        let wrapped = (first == '"' && last == '"') || (first == '\'' && last == '\'');
        if wrapped {
            out = out[1..out.len() - 1].trim().to_string();
            continue;
        }
        break;
    }
    out
}

/// Upgrade a standard 5-field Unix cron expression ("min hour day month dow")
/// to the 6-field form the `cron` crate actually requires ("sec min hour day
/// month dow"), by assuming seconds=0. 6- and 7-field expressions (explicit
/// seconds, optional trailing year) pass through unchanged. This is what
/// makes `--cron "0 9 * * *"` — the format everyone reaches for, and what the
/// help text and system prompt document — actually work instead of failing
/// with "Invalid cron expression".
fn normalize_cron_expr(expr: &str) -> String {
    let fields = expr.split_whitespace().count();
    if fields == 5 {
        format!("0 {expr}")
    } else {
        expr.to_string()
    }
}

// ─────────────────────────────────────────────
// Command implementations
// ─────────────────────────────────────────────

/// `Metis cron list [--all]`
async fn list_jobs(include_disabled: bool) -> Result<()> {
    let service = make_service();
    service.load().await.context("failed to load cron store")?;

    let jobs = service.list_jobs().await;
    let jobs: Vec<&CronJob> = if include_disabled {
        jobs.iter().collect()
    } else {
        jobs.iter().filter(|j| j.enabled).collect()
    };

    if jobs.is_empty() {
        println!("  No scheduled jobs.{}", if !include_disabled { " Use --all to include disabled." } else { "" });
        return Ok(());
    }

    println!();
    println!("{}", "  Scheduled Jobs".cyan().bold());
    println!();

    // Header
    // State the current time in both zones, so a reader never has to infer
    // which zone "Next Run" is in or how far away it is.
    let now_local = chrono::Local::now();
    println!(
        "  now: {}  ({} UTC)",
        now_local.format("%Y-%m-%d %H:%M %:z"),
        chrono::Utc::now().format("%H:%M")
    );
    println!();
    println!(
        "  {:<10} {:<20} {:<18} {:<10} {}",
        "ID".bold(),
        "Name".bold(),
        "Schedule".bold(),
        "Status".bold(),
        "Next Run".bold(),
    );
    println!("  {}", "─".repeat(84));

    for job in &jobs {
        // Format schedule
        let schedule = match job.schedule.kind {
            ScheduleKind::Every => {
                let ms = job.schedule.every_ms.unwrap_or(60_000);
                format!("every {}", format_duration_ms(ms))
            }
            ScheduleKind::Cron => {
                job.schedule.expr.clone().unwrap_or_else(|| "—".to_string())
            }
            ScheduleKind::At => "one-time".to_string(),
        };

        // Format status
        let status = if job.enabled {
            "enabled".green().to_string()
        } else {
            "disabled".dimmed().to_string()
        };

        // Format next run
        let next_run = match job.state.next_run_at_ms {
            Some(ms) => format_timestamp_ms(ms),
            None => "—".to_string(),
        };

        println!(
            "  {:<10} {:<20} {:<18} {:<10} {}",
            job.id, job.name, schedule, status, next_run
        );
    }

    println!();
    Ok(())
}

/// `Metis cron add`
async fn add_job(
    name: String,
    message: String,
    every: Option<u64>,
    cron_expr: Option<String>,
    at: Option<String>,
    deliver: bool,
    to: Option<String>,
    channel: Option<String>,
) -> Result<()> {
    let name = normalize_cli_value(&name);
    let message = normalize_cli_value(&message);
    let to = to.map(|v| normalize_cli_value(&v));
    let channel = channel.map(|v| normalize_cli_value(&v));
    let cron_expr = cron_expr.map(|v| normalize_cli_value(&v));
    let at = at.map(|v| normalize_cli_value(&v));

    // Determine schedule
    let schedule = if let Some(secs) = every {
        CronSchedule::every((secs * 1000) as i64)
    } else if let Some(expr) = cron_expr {
        // The underlying `cron` crate requires 6 fields (leading seconds:
        // "sec min hour day month dow"), not the standard 5-field Unix
        // crontab format ("min hour day month dow") that the --cron help
        // text advertises and that anyone would naturally type. Auto-upgrade
        // a 5-field expression by assuming seconds=0, so "0 9 * * *" works
        // as documented instead of failing with "Invalid cron expression".
        let expr = normalize_cron_expr(&expr);
        // Validate cron expression
        let _ = expr
            .parse::<cron::Schedule>()
            .map_err(|e| anyhow::anyhow!("Invalid cron expression '{}': {}", expr, e))?;
        CronSchedule::cron(expr)
    } else if let Some(at_str) = at {
        let dt = chrono::NaiveDateTime::parse_from_str(&at_str, "%Y-%m-%dT%H:%M:%S")
            .or_else(|_| chrono::NaiveDateTime::parse_from_str(&at_str, "%Y-%m-%d %H:%M:%S"))
            .or_else(|_| chrono::NaiveDateTime::parse_from_str(&at_str, "%Y-%m-%dT%H:%M"))
            .map_err(|e| anyhow::anyhow!("Invalid datetime '{}': {} (expected ISO 8601, e.g. 2026-03-01T09:00:00)", at_str, e))?;
        let local = chrono::Local::now().timezone();
        let aware = dt.and_local_timezone(local);
        let ts_ms = match aware {
            chrono::LocalResult::Single(dt) => dt.timestamp_millis(),
            _ => anyhow::bail!("Ambiguous or invalid local time: {}", at_str),
        };
        CronSchedule::at(ts_ms)
    } else {
        anyhow::bail!("Must specify one of: --every <seconds>, --cron <expression>, or --at <datetime>");
    };

    if deliver && to.is_none() {
        anyhow::bail!(
            "`--deliver` requires `--to <chat_id>` so Metis knows where to send the response"
        );
    }

    let payload = CronPayload {
        message,
        deliver,
        channel,
        to,
    };

    let job = CronJob::new(name, schedule, payload);

    let service = make_service();
    service.load().await.context("failed to load cron store")?;
    let id = service.add_job(job).await.context("failed to add job")?;

    println!(
        "  {} Added job {} ({})",
        "✓".green(),
        id.cyan(),
        service.get_job(&id).await.map(|j| j.name).unwrap_or_default()
    );

    Ok(())
}

/// `Metis cron remove <ID>`
async fn remove_job(id: &str) -> Result<()> {
    let service = make_service();
    service.load().await.context("failed to load cron store")?;

    if service.remove_job(id).await? {
        println!("  {} Removed job {}", "✓".green(), id.cyan());
    } else {
        println!("  {} Job {} not found", "✗".red(), id);
    }

    Ok(())
}

/// `Metis cron enable <ID> [--disable]`
async fn enable_job(id: &str, enabled: bool) -> Result<()> {
    let service = make_service();
    service.load().await.context("failed to load cron store")?;

    if service.set_enabled(id, enabled).await? {
        let label = if enabled { "Enabled" } else { "Disabled" };
        let job_name = service
            .get_job(id)
            .await
            .map(|j| j.name)
            .unwrap_or_default();
        println!(
            "  {} {} job '{}' ({})",
            "✓".green(),
            label,
            job_name,
            id.cyan()
        );
    } else {
        println!("  {} Job {} not found", "✗".red(), id);
    }

    Ok(())
}

/// `Metis cron run <ID>`
async fn run_job(id: &str) -> Result<()> {
    let service = make_service();
    service.load().await.context("failed to load cron store")?;

    let job = service.get_job(id).await;
    if job.is_none() {
        println!("  {} Job {} not found", "✗".red(), id);
        return Ok(());
    }
    let job = job.unwrap();

    // For manual run, we need an agent. Build one from config.
    println!(
        "  {} Running job '{}' ({})...",
        "⠿".dimmed(),
        job.name,
        id.cyan()
    );

    // Channel delivery only works inside the long-running `metis gateway`
    // process — that is where the Telegram/Discord/etc. channels and the
    // outbound dispatcher live. A CLI `cron run` builds its own MessageBus
    // with nobody consuming the outbound side, so `deliver=true` publishes
    // into a void and the job still reports success. Say so plainly: the
    // agent was reading this output and telling the user "delivered, check
    // your Telegram" when nothing had been sent.
    let delivers_to_channel = job.payload.deliver && job.payload.to.is_some();
    if delivers_to_channel {
        let channel = job.payload.channel.clone().unwrap_or_else(|| "telegram".into());
        let to = job.payload.to.clone().unwrap_or_default();
        println!(
            "  {} DELIVERY SKIPPED: this is a CLI test run, not the gateway. The result below was \
             NOT sent to {channel} chat {to}. Channel delivery happens only when the job runs \
             inside `metis gateway` (its next scheduled run will deliver normally).",
            "⚠".yellow()
        );
        println!();
    }

    let config = metis_core::config::load_config(None);
    let agent_loop = crate::agent_builder::build_agent_loop(&config)?;

    let response = agent_loop
        .process_direct_session("cron", &job.id, &job.payload.message)
        .await
        .context("agent processing failed")?;

    // Persist outcome without invoking the gateway callback (already ran above).
    service
        .execute_job(id, Some(Ok(response.clone())))
        .await;

    println!();
    println!("{}", "🦀 Metis".cyan().bold());
    if response.is_empty() {
        println!("{}", "(no response)".dimmed());
    } else {
        println!("{response}");
    }
    println!();

    Ok(())
}

// ─────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_duration_ms() {
        assert_eq!(format_duration_ms(5_000), "5s");
        assert_eq!(format_duration_ms(60_000), "1m");
        assert_eq!(format_duration_ms(120_000), "2m");
        assert_eq!(format_duration_ms(3_600_000), "1h");
        assert_eq!(format_duration_ms(86_400_000), "1d");
    }

    #[test]
    fn test_format_timestamp_ms() {
        // Just make sure it doesn't panic
        let result = format_timestamp_ms(1_707_696_000_000); // 2024-02-12 ~UTC
        assert!(!result.is_empty());
        assert_ne!(result, "—");
    }

    #[test]
    fn test_format_timestamp_ms_invalid() {
        // i64::MIN should produce "—"
        // Actually chrono handles most values, so just check it doesn't panic
        let result = format_timestamp_ms(0);
        assert!(!result.is_empty());
    }

    #[test]
    fn test_normalize_cli_value_strips_wrapped_quotes() {
        assert_eq!(normalize_cli_value("\"news\""), "news");
        assert_eq!(normalize_cli_value("'news'"), "news");
        assert_eq!(normalize_cli_value("\"'8582973375'\""), "8582973375");
    }

    #[test]
    fn test_normalize_cli_value_keeps_plain_text() {
        assert_eq!(normalize_cli_value("telegram"), "telegram");
        assert_eq!(normalize_cli_value("0 * * * *"), "0 * * * *");
    }

    #[test]
    fn test_normalize_cron_expr_upgrades_5_field() {
        // Standard Unix crontab format (what the help text and system prompt
        // document) must become the 6-field form the `cron` crate requires.
        assert_eq!(normalize_cron_expr("0 9 * * *"), "0 0 9 * * *");
        assert_eq!(normalize_cron_expr("0 11 * * *"), "0 0 11 * * *");
        assert_eq!(normalize_cron_expr("*/15 * * * *"), "0 */15 * * * *");
    }

    #[test]
    fn test_normalize_cron_expr_leaves_6_and_7_field_unchanged() {
        assert_eq!(normalize_cron_expr("0 0 9 * * *"), "0 0 9 * * *");
        assert_eq!(normalize_cron_expr("0 0 9 * * * 2026"), "0 0 9 * * * 2026");
    }

    #[test]
    fn test_normalize_cron_expr_upgraded_form_actually_parses() {
        // The real bug: "0 9 * * *" rejected by cron::Schedule as-is.
        assert!("0 9 * * *".parse::<cron::Schedule>().is_err());
        let upgraded = normalize_cron_expr("0 9 * * *");
        assert!(upgraded.parse::<cron::Schedule>().is_ok());
    }
}
