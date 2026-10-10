//! `metis agents` and `metis events` — look at the system, not at what the
//! agent says about it.
//!
//! `metis events` prints the harness's own record of a session: what the
//! person said, which tools ran, what they really returned, and what was
//! sent back. When the agent's account of a turn and this record disagree,
//! this record is the truth.

use std::time::SystemTime;

use anyhow::Result;
use colored::Colorize;

use metis_core::config::loader::load_config;
use metis_core::events::{Event, EventKind, EventLog, ExecStatus, ToolOutcome};

// ─────────────────────────────────────────────
// metis agents
// ─────────────────────────────────────────────

/// `metis agents` — every agent, its model, and whether that model can be
/// reached with the current configuration.
pub fn agents() -> Result<()> {
    let config = load_config(None);
    let providers = config.providers.to_map();

    println!();
    println!("  {}", "Agents".bold());
    println!();
    println!(
        "  {:<12} {:<34} {:<26} {}",
        "NAME".bold(),
        "MODEL".bold(),
        "PROVIDER".bold(),
        "PURPOSE".bold()
    );
    println!("  {}", "─".repeat(100));

    for a in config.agents.all_agents() {
        let provider = match metis_providers::registry::match_provider(&a.model, &providers) {
            Some((_, spec)) if spec.is_local => format!("{} (local)", spec.display_name).green(),
            Some((_, spec)) => spec.display_name.to_string().green(),
            None => "NOT CONFIGURED — no API key".red(),
        };
        let model = if a.model.trim().is_empty() {
            "(none)".dimmed().to_string()
        } else {
            a.model.clone()
        };
        println!(
            "  {:<12} {:<34} {:<26} {}",
            a.name.cyan(),
            model,
            provider,
            a.description.dimmed()
        );
    }

    let problems = config.agents.roster_problems();
    if !problems.is_empty() {
        println!();
        for p in problems {
            println!("  {} {p}", "⚠".yellow());
        }
    }
    if config.agents.roster.is_empty() {
        println!();
        println!(
            "  {}",
            "Add agents under agents.roster in ~/.metis/config.json, e.g.:".dimmed()
        );
        println!(
            "  {}",
            r#"  "roster": { "worker": { "model": "ollama/qwen2.5-coder", "description": "local file work" } }"#
                .dimmed()
        );
    }
    println!();
    Ok(())
}

// ─────────────────────────────────────────────
// metis events
// ─────────────────────────────────────────────

/// `metis events [SESSION] [--turns N]`
///
/// Without a session: list recorded sessions. With one: print its last N
/// turns. A session can be named by its key (`telegram:8582973375`), its file
/// stem (`telegram_8582973375`), or any unique fragment of either.
pub fn events(session: Option<String>, turns: usize) -> Result<()> {
    let log = EventLog::new(EventLog::default_dir());
    let sessions = log.sessions();

    let Some(wanted) = session else {
        return list_sessions(&log, &sessions);
    };

    let needle = wanted.replace(':', "_").to_lowercase();
    let matches: Vec<_> = sessions
        .iter()
        .filter(|(stem, _, _)| stem.to_lowercase().contains(&needle))
        .collect();
    let (stem, path, _) = match matches.as_slice() {
        [] => anyhow::bail!(
            "No recorded session matches \"{wanted}\". Run `metis events` to list them."
        ),
        [one] => *one,
        many => {
            // An exact stem wins; otherwise refuse to guess.
            match many.iter().find(|(s, _, _)| s.to_lowercase() == needle) {
                Some(exact) => *exact,
                None => anyhow::bail!(
                    "\"{wanted}\" matches {} sessions: {}. Be more specific.",
                    many.len(),
                    many.iter().map(|(s, _, _)| s.as_str()).collect::<Vec<_>>().join(", ")
                ),
            }
        }
    };

    let all = EventLog::read_file(path);
    print_session(stem, &all, turns);
    Ok(())
}

fn list_sessions(
    log: &EventLog,
    sessions: &[(String, std::path::PathBuf, SystemTime)],
) -> Result<()> {
    println!();
    if sessions.is_empty() {
        println!(
            "  No events recorded yet in {}.\n  They appear once the gateway or desktop has handled a message.",
            log.dir().display()
        );
        println!();
        return Ok(());
    }
    println!("  {}  ({})", "Recorded sessions".bold(), log.dir().display());
    println!();
    println!("  {:<44} {:>7} {:>6}  {}", "SESSION".bold(), "EVENTS".bold(), "TURNS".bold(), "LAST ACTIVITY".bold());
    println!("  {}", "─".repeat(84));
    for (stem, path, mtime) in sessions {
        let events = EventLog::read_file(path);
        let mut turns: Vec<&str> = events.iter().map(|e| e.turn.as_str()).collect();
        turns.dedup();
        let when: chrono::DateTime<chrono::Local> = (*mtime).into();
        println!(
            "  {:<44} {:>7} {:>6}  {}",
            stem.cyan(),
            events.len(),
            turns.len(),
            when.format("%Y-%m-%d %H:%M")
        );
    }
    println!();
    println!("  {}", "Show one: metis events <session> [--turns N]".dimmed());
    println!();
    Ok(())
}

/// One-line summary of a tool outcome.
fn outcome_line(o: &ToolOutcome) -> String {
    match o {
        ToolOutcome::Exec { exit_code, status, stdout, stderr, truncated, .. } => {
            let mark = match status {
                ExecStatus::Success => "✓ ran".green(),
                ExecStatus::Failed => "✗ failed".red(),
                ExecStatus::Timeout => "✗ timed out".red(),
                ExecStatus::NotRun => "⋯ NOT RUN (safety check)".yellow(),
            };
            let code = exit_code.map(|c| format!(" · exit {c}")).unwrap_or_default();
            let lines = stdout.lines().count();
            let err = if stderr.trim().is_empty() { String::new() } else { " · stderr".to_string() };
            let cut = if *truncated { " · truncated" } else { "" };
            format!("{mark}{code} · {lines} line(s) stdout{err}{cut}")
        }
        ToolOutcome::Ok { output, truncated } => format!(
            "{} · {} chars{}",
            "✓ ok".green(),
            output.chars().count(),
            if *truncated { " · truncated" } else { "" }
        ),
        ToolOutcome::Failed { error } => format!("{} {}", "✗".red(), first_line(error, 120)),
    }
}

fn first_line(s: &str, max: usize) -> String {
    let line = s.lines().find(|l| !l.trim().is_empty()).unwrap_or("").trim();
    if line.chars().count() > max {
        format!("{}…", line.chars().take(max).collect::<String>())
    } else {
        line.to_string()
    }
}

/// Indented preview of the first lines of some output.
fn preview(s: &str, lines: usize) -> String {
    s.lines()
        .filter(|l| !l.trim().is_empty())
        .take(lines)
        .map(|l| format!("              │ {}", first_line(l, 110)))
        .collect::<Vec<_>>()
        .join("\n")
}

fn print_session(stem: &str, all: &[Event], turns: usize) {
    // Keep the last N distinct turns, in order.
    let mut order: Vec<&str> = Vec::new();
    for e in all {
        if order.last() != Some(&e.turn.as_str()) && !order.contains(&e.turn.as_str()) {
            order.push(&e.turn);
        }
    }
    let keep: Vec<&str> = order.iter().rev().take(turns.max(1)).rev().copied().collect();

    println!();
    println!(
        "  {}  — last {} of {} turn(s)",
        stem.cyan().bold(),
        keep.len(),
        order.len()
    );
    for turn in keep {
        println!();
        println!("  {}", format!("── turn {turn} ").dimmed());
        for e in all.iter().filter(|e| e.turn == turn) {
            let t: chrono::DateTime<chrono::Local> = e.ts.into();
            let ts = t.format("%H:%M:%S").to_string().dimmed();
            let who = e.agent.as_str();
            match &e.event {
                EventKind::UserMessage { text } => {
                    println!("  {ts}  {:<8} {}", "user".bold(), first_line(text, 110));
                }
                EventKind::ToolCall { tool, args, .. } => {
                    let a = args.to_string();
                    println!("  {ts}  {:<8} → {} {}", who, tool.yellow(), first_line(&a, 90).dimmed());
                }
                EventKind::ToolResult { tool, outcome, .. } => {
                    println!("  {ts}  {:<8} ← {} {}", who, tool, outcome_line(outcome));
                    let body = match outcome {
                        ToolOutcome::Exec { stdout, stderr, .. } => {
                            if stdout.trim().is_empty() { stderr.clone() } else { stdout.clone() }
                        }
                        ToolOutcome::Ok { output, .. } => output.clone(),
                        ToolOutcome::Failed { .. } => String::new(),
                    };
                    let p = preview(&body, 4);
                    if !p.is_empty() {
                        println!("{}", p.dimmed());
                    }
                }
                EventKind::Thinking { text } => {
                    println!("  {ts}  {:<8} {} {}", who, "thinks:".dimmed(), first_line(text, 100).dimmed());
                }
                EventKind::AgentReply { text } => {
                    println!("  {ts}  {:<8} {} {}", who, "reply:".bold(), first_line(text, 100));
                }
                EventKind::Delegation { to_agent, task } => {
                    println!("  {ts}  {:<8} ⇢ {} {}", who, to_agent.cyan(), first_line(task, 90));
                }
                EventKind::DelegationResult { from_agent, text } => {
                    println!("  {ts}  {:<8} ⇠ {} {}", who, from_agent.cyan(), first_line(text, 90));
                }
            }
        }
    }
    println!();
}
