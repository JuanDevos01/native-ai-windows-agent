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

        // Format status. A finished one-time job stays in the list disabled;
        // labelling it "done" stops that being read as "never consumed".
        let finished_one_time = job.schedule.kind == ScheduleKind::At
            && job.state.next_run_at_ms.is_none()
            && job.state.last_run_at_ms.is_some();
        let status = if finished_one_time {
            "done".dimmed().to_string()
        } else if job.enabled {
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


/// Parse the `--at` argument into epoch milliseconds.
///
/// Accepts RFC 3339 with an explicit zone ("2026-09-02T13:00:00Z",
/// "2026-09-02T08:00:00-05:00") as well as naive forms, which are read as
/// LOCAL time. Both matter: this used to reject every timestamp with a "Z",
/// and the fallback — writing UTC wall-clock into a parser that reads local —
/// silently landed a "10:00" job five hours late. The caller echoes the
/// parsed time back so a wrong zone is visible the moment the job is added.
fn parse_at_datetime(at_str: &str) -> anyhow::Result<i64> {
    let at_str = at_str.trim();
    // Explicit zone first: unambiguous, so honour it exactly.
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(at_str) {
        return Ok(dt.timestamp_millis());
    }
    let dt = chrono::NaiveDateTime::parse_from_str(at_str, "%Y-%m-%dT%H:%M:%S")
        .or_else(|_| chrono::NaiveDateTime::parse_from_str(at_str, "%Y-%m-%d %H:%M:%S"))
        .or_else(|_| chrono::NaiveDateTime::parse_from_str(at_str, "%Y-%m-%dT%H:%M"))
        .map_err(|_| {
            anyhow::anyhow!(
                "Invalid datetime '{at_str}'. Use local time (2026-03-01T09:00:00) or an \
                 explicit zone (2026-03-01T14:00:00Z, 2026-03-01T09:00:00-05:00)."
            )
        })?;
    match dt.and_local_timezone(chrono::Local::now().timezone()) {
        chrono::LocalResult::Single(dt) => Ok(dt.timestamp_millis()),
        _ => anyhow::bail!("Ambiguous or invalid local time: {at_str}"),
    }
}

/// "in 2h 5m" / "OVERDUE by 37m" — so a job scheduled into the past is
/// caught while the command output is still on screen.
fn describe_delay_from_now(ts_ms: i64) -> String {
    let delta_ms = ts_ms - chrono::Utc::now().timestamp_millis();
    let (label, ms) = if delta_ms < 0 {
        ("OVERDUE by", -delta_ms)
    } else {
        ("in", delta_ms)
    };
    let mins = ms / 60_000;
    let text = if mins >= 60 {
        format!("{}h {}m", mins / 60, mins % 60)
    } else {
        format!("{mins}m")
    };
    format!("{label} {text}")
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
        CronSchedule::at(parse_at_datetime(&at_str)?)
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

    let added = service.get_job(&id).await;
    println!(
        "  {} Added job {} ({})",
        "✓".green(),
        id.cyan(),
        added.as_ref().map(|j| j.name.clone()).unwrap_or_default()
    );
    // Echo the schedule as UNDERSTOOD, not as typed. A timezone mistake in
    // --at is invisible in the input but obvious here.
    if let Some(next) = added.as_ref().and_then(|j| j.state.next_run_at_ms) {
        println!(
            "    next run: {} ({} UTC) — {}",
            format_timestamp_ms(next),
            chrono::DateTime::from_timestamp_millis(next)
                .map(|d| d.format("%H:%M").to_string())
                .unwrap_or_default(),
            describe_delay_from_now(next)
        );
    }
    // A job whose output goes nowhere looks identical to a broken scheduler
    // from the outside. Say it while the add is still on screen.
    if let Some(j) = &added {
        if j.payload.deliver {
            println!(
                "    delivers to {}:{}",
                j.payload.channel.as_deref().unwrap_or("telegram"),
                j.payload.to.as_deref().unwrap_or("?")
            );
        } else {
            println!(
                "    {} output will NOT be sent anywhere. The job runs, the result is discarded. \
Add --deliver --to <chat_id> --channel telegram to receive it.",
                "⚠".yellow()
            );
        }
    }

    Ok(())
}

/// `Metis cron remove <ID>`
async fn remove_job(id: &str) -> Result<()> {
    let service = make_service();
    service.load().await.context("failed to load cron store")?;

    if service.remove_job(id).await? {
        println!("  {} Removed job {}", "✓".green(), id.cyan());
        Ok(())
    } else {
        // Exit nonzero: "removed something that was not there" reported as
        // success is how a cleanup ends with the caller believing jobs are
        // gone that never existed.
        anyhow::bail!("Job {id} not found — nothing was removed. Run `cron list --all` for valid ids.");
    }
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


    // ── Regression: the night of failed --at timestamps ──────────────────
    //
    // "Every single --at I ran tonight used Z (2026-09-02T12:00:00Z), and
    // every single one failed." They failed because only naive local forms
    // were accepted. The fallback was worse: writing UTC wall-clock into the
    // local-time parser landed a "10:00 Colombia" job at 15:00.

    #[test]
    fn at_accepts_utc_z_suffix() {
        let ms = parse_at_datetime("2026-09-02T12:00:00Z").unwrap();
        assert_eq!(ms, 1_788_350_400_000, "12:00 UTC that day, exactly");
    }

    #[test]
    fn at_accepts_explicit_offsets() {
        // 08:00 in Colombia (UTC-5) is 13:00 UTC.
        let bogota = parse_at_datetime("2026-09-02T08:00:00-05:00").unwrap();
        let utc = parse_at_datetime("2026-09-02T13:00:00Z").unwrap();
        assert_eq!(bogota, utc, "the same instant written two ways");
    }

    #[test]
    fn at_still_accepts_naive_local_forms() {
        // Whatever the machine's zone, all three naive spellings must agree.
        let a = parse_at_datetime("2026-09-02T08:00:00").unwrap();
        let b = parse_at_datetime("2026-09-02 08:00:00").unwrap();
        let c = parse_at_datetime("2026-09-02T08:00").unwrap();
        assert_eq!(a, b);
        assert_eq!(b, c);
    }

    #[test]
    fn at_rejects_garbage_with_a_usable_message() {
        let err = parse_at_datetime("tomorrow at 8").unwrap_err().to_string();
        assert!(err.contains("2026-03-01T09:00:00"), "should show a working example: {err}");
        assert!(err.contains('Z'), "should show the UTC form too: {err}");
    }

    #[test]
    fn overdue_times_say_so() {
        let past = chrono::Utc::now().timestamp_millis() - 37 * 60_000;
        let text = describe_delay_from_now(past);
        assert!(text.starts_with("OVERDUE"), "{text}");
        let future = chrono::Utc::now().timestamp_millis() + 83 * 60_000;
        let text = describe_delay_from_now(future);
        assert!(text.contains("1h 2"), "{text}");
    }

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
