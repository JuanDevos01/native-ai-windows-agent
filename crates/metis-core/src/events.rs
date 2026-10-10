//! Event log — the harness's record of what actually happened.
//!
//! Every user message, tool call, tool result and agent reply is appended
//! here as a typed event, one JSON object per line, per session.
//!
//! This exists because the agent's own account of a turn cannot be trusted
//! as a record of it. Its replies are generated text, and generated text has
//! reported commands that never ran, process ids that never existed, and
//! messages "sent" that never left the machine. The event log is written by
//! the harness from the tool calls it actually executed and the results they
//! actually returned, so when the agent says one thing and the log says
//! another, the log is right.
//!
//! It is also the foundation multi-agent work builds on: a delegation from
//! one agent to another is just another event, so the conversation between
//! agents is recorded by the same mechanism, and a viewer only has to render
//! the log.

use std::io::Write;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Largest tool output stored per result. Page trees and file reads can be
/// megabytes; the log is a record, not an archive, so it keeps the head.
pub const MAX_STORED_OUTPUT: usize = 64 * 1024;

/// How an `exec` command ended.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ExecStatus {
    Success,
    Failed,
    Timeout,
    /// Stopped by the safety check; the command did NOT run.
    NotRun,
}

/// The typed result of one tool call.
///
/// Structured rather than text, so nothing downstream has to parse — or can
/// be fooled by — text that merely looks like tool output.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolOutcome {
    /// A shell command, with its real exit status and streams.
    Exec {
        command: String,
        exit_code: Option<i32>,
        status: ExecStatus,
        stdout: String,
        stderr: String,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        truncated: bool,
    },
    /// Any other tool that succeeded.
    Ok {
        output: String,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        truncated: bool,
    },
    /// Any other tool that failed.
    Failed { error: String },
}

impl ToolOutcome {
    /// Did the call achieve what it was asked to?
    pub fn succeeded(&self) -> bool {
        match self {
            ToolOutcome::Exec { status, .. } => *status == ExecStatus::Success,
            ToolOutcome::Ok { .. } => true,
            ToolOutcome::Failed { .. } => false,
        }
    }

    /// All the text this outcome carries — what a stated fact can be checked
    /// against.
    pub fn evidence_text(&self) -> String {
        match self {
            ToolOutcome::Exec { stdout, stderr, .. } => format!("{stdout}\n{stderr}"),
            ToolOutcome::Ok { output, .. } => output.clone(),
            ToolOutcome::Failed { error } => error.clone(),
        }
    }
}

/// What happened.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EventKind {
    /// A message from the person (or the scheduler) that started a turn.
    UserMessage { text: String },
    /// The agent asked for a tool to be run.
    ToolCall {
        call_id: String,
        tool: String,
        args: serde_json::Value,
    },
    /// What that tool actually returned.
    ToolResult {
        call_id: String,
        tool: String,
        outcome: ToolOutcome,
    },
    /// The text sent back to the person.
    AgentReply { text: String },
    /// What the model reasoned before acting, for one model call. Shown for
    /// troubleshooting: when an agent does something odd, its stated
    /// reasoning is usually where the misunderstanding is visible.
    Thinking { text: String },
    /// One agent handed a task to another.
    Delegation { to_agent: String, task: String },
    /// The delegated agent's answer, returned to the delegator.
    DelegationResult { from_agent: String, text: String },
}

/// One entry in the log.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Event {
    pub ts: DateTime<Utc>,
    /// Session the event belongs to, e.g. `telegram:8582973375`.
    pub session: String,
    /// Groups the events of one user message and everything it caused.
    pub turn: String,
    /// Which agent acted. `main` for the primary agent.
    pub agent: String,
    pub event: EventKind,
}

impl Event {
    pub fn new(session: &str, turn: &str, agent: &str, event: EventKind) -> Self {
        Self {
            ts: Utc::now(),
            session: session.to_string(),
            turn: turn.to_string(),
            agent: agent.to_string(),
            event,
        }
    }
}

/// Keep at most [`MAX_STORED_OUTPUT`] bytes, cut on a char boundary.
pub fn clip_output(s: &str) -> (String, bool) {
    if s.len() <= MAX_STORED_OUTPUT {
        return (s.to_string(), false);
    }
    let mut end = MAX_STORED_OUTPUT;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    (s[..end].to_string(), true)
}

/// Turn a session key into a safe file name.
fn file_stem(session: &str) -> String {
    session
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect()
}

/// Append-only, per-session JSONL store.
#[derive(Debug)]
pub struct EventLog {
    dir: PathBuf,
    /// Serialises appends within the process so concurrent turns cannot
    /// interleave half-written lines.
    lock: std::sync::Mutex<()>,
}

impl EventLog {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self {
            dir: dir.into(),
            lock: std::sync::Mutex::new(()),
        }
    }

    /// `~/.metis/events/`
    pub fn default_dir() -> PathBuf {
        crate::utils::get_data_path().join("events")
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn path_for(&self, session: &str) -> PathBuf {
        self.dir.join(format!("{}.jsonl", file_stem(session)))
    }

    /// Record one event. A failure to write is returned, never ignored by
    /// this layer — the caller decides how loudly to complain.
    pub fn append(&self, ev: &Event) -> std::io::Result<()> {
        let line = serde_json::to_string(ev)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        let _guard = self.lock.lock().unwrap_or_else(|p| p.into_inner());
        std::fs::create_dir_all(&self.dir)?;
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.path_for(&ev.session))?;
        // One write per line, so a reader never sees a partial record.
        f.write_all(format!("{line}\n").as_bytes())
    }

    /// Every event of a session, oldest first. Unparseable lines (a crash
    /// mid-write, a hand edit) are skipped rather than failing the read.
    pub fn read(&self, session: &str) -> Vec<Event> {
        Self::read_file(&self.path_for(session))
    }

    /// Read a log file directly by path.
    pub fn read_file(path: &Path) -> Vec<Event> {
        let Ok(text) = std::fs::read_to_string(path) else {
            return Vec::new();
        };
        text.lines()
            .filter(|l| !l.trim().is_empty())
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect()
    }

    /// Recorded sessions, newest activity first, as (file stem, path, mtime).
    pub fn sessions(&self) -> Vec<(String, PathBuf, std::time::SystemTime)> {
        let Ok(rd) = std::fs::read_dir(&self.dir) else {
            return Vec::new();
        };
        let mut out: Vec<_> = rd
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().is_some_and(|x| x == "jsonl"))
            .filter_map(|e| {
                let path = e.path();
                let stem = path.file_stem()?.to_string_lossy().to_string();
                let mtime = e.metadata().ok()?.modified().ok()?;
                Some((stem, path, mtime))
            })
            .collect();
        out.sort_by(|a, b| b.2.cmp(&a.2));
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn log() -> (tempfile::TempDir, EventLog) {
        let dir = tempfile::tempdir().unwrap();
        let log = EventLog::new(dir.path());
        (dir, log)
    }

    #[test]
    fn events_round_trip_in_order() {
        let (_d, log) = log();
        let s = "telegram:8582973375";
        log.append(&Event::new(s, "t1", "main", EventKind::UserMessage { text: "status?".into() }))
            .unwrap();
        log.append(&Event::new(
            s,
            "t1",
            "main",
            EventKind::ToolResult {
                call_id: "c1".into(),
                tool: "exec".into(),
                outcome: ToolOutcome::Exec {
                    command: "Get-Process python".into(),
                    exit_code: Some(0),
                    status: ExecStatus::Success,
                    stdout: "12968 python".into(),
                    stderr: String::new(),
                    truncated: false,
                },
            },
        ))
        .unwrap();
        log.append(&Event::new(s, "t1", "main", EventKind::AgentReply { text: "PID 12968".into() }))
            .unwrap();

        let back = log.read(s);
        assert_eq!(back.len(), 3);
        assert!(matches!(back[0].event, EventKind::UserMessage { .. }));
        assert!(matches!(back[2].event, EventKind::AgentReply { .. }));
        match &back[1].event {
            EventKind::ToolResult { outcome, .. } => {
                assert!(outcome.succeeded());
                assert!(outcome.evidence_text().contains("12968"));
            }
            other => panic!("wrong event: {other:?}"),
        }
    }

    #[test]
    fn sessions_are_kept_apart() {
        let (_d, log) = log();
        log.append(&Event::new("a:1", "t", "main", EventKind::UserMessage { text: "x".into() }))
            .unwrap();
        log.append(&Event::new("b:2", "t", "main", EventKind::UserMessage { text: "y".into() }))
            .unwrap();
        assert_eq!(log.read("a:1").len(), 1);
        assert_eq!(log.read("b:2").len(), 1);
        assert_eq!(log.sessions().len(), 2);
    }

    #[test]
    fn session_keys_become_safe_file_names() {
        assert_eq!(file_stem("telegram:8582973375"), "telegram_8582973375");
        assert_eq!(file_stem("cron:f3162251-20261008"), "cron_f3162251-20261008");
        assert_eq!(file_stem("..\\..\\evil"), "______evil");
    }

    #[test]
    fn a_corrupt_line_does_not_lose_the_rest() {
        let (_d, log) = log();
        log.append(&Event::new("s", "t", "main", EventKind::UserMessage { text: "before".into() }))
            .unwrap();
        // Simulate a crash mid-write.
        let path = log.path_for("s");
        let mut f = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
        f.write_all(b"{\"ts\":\"2026-10-07T00:00:00Z\",\"sess").unwrap();
        f.write_all(b"\n").unwrap();
        log.append(&Event::new("s", "t", "main", EventKind::UserMessage { text: "after".into() }))
            .unwrap();
        assert_eq!(log.read("s").len(), 2, "both intact events must survive");
    }

    #[test]
    fn huge_outputs_are_clipped_on_a_char_boundary() {
        let big = "é".repeat(MAX_STORED_OUTPUT); // 2 bytes per char
        let (clipped, truncated) = clip_output(&big);
        assert!(truncated);
        assert!(clipped.len() <= MAX_STORED_OUTPUT);
        // Valid UTF-8 by construction, or this would have panicked.
        assert!(clipped.chars().all(|c| c == 'é'));
    }

    #[test]
    fn a_not_run_command_is_not_a_success() {
        let o = ToolOutcome::Exec {
            command: "Remove-Item x -Recurse".into(),
            exit_code: None,
            status: ExecStatus::NotRun,
            stdout: String::new(),
            stderr: String::new(),
            truncated: false,
        };
        assert!(!o.succeeded());
    }
}
