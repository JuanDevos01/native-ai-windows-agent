//! Heartbeat service — periodic agent wake-up to check for tasks.
//!
//! Port of nanobot's `heartbeat/service.py`.
//!
//! The agent reads `HEARTBEAT.md` from the workspace and executes any
//! tasks listed there. If nothing needs attention, it replies `HEARTBEAT_OK`.
//! If `HEARTBEAT.md` is empty or contains only headers, the tick is skipped.

use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::future::Future;

use tokio::sync::Notify;
use tracing::{debug, error, info};

use crate::bus::queue::MessageBus;

// ─────────────────────────────────────────────
// Constants
// ─────────────────────────────────────────────

/// Default interval: 30 minutes.
pub const DEFAULT_HEARTBEAT_INTERVAL_S: u64 = 30 * 60;

/// Legacy static prompt (prefer [`build_heartbeat_prompt`]).
pub const HEARTBEAT_PROMPT: &str = r#"Read HEARTBEAT.md in your workspace (if it exists).
Follow any instructions or tasks listed there.
If nothing needs attention, reply with just: HEARTBEAT_OK"#;

/// Token that indicates "nothing to do".
const HEARTBEAT_OK_TOKEN: &str = "HEARTBEAT_OK";

// ─────────────────────────────────────────────
// Callback type
// ─────────────────────────────────────────────

/// Callback invoked on each heartbeat tick.
///
/// Receives the heartbeat prompt and returns the agent's response.
pub type OnHeartbeatFn = Arc<
    dyn Fn(String) -> Pin<Box<dyn Future<Output = anyhow::Result<String>> + Send>>
        + Send
        + Sync,
>;

// ─────────────────────────────────────────────
// Prompt builder
// ─────────────────────────────────────────────

/// Build the prompt for one heartbeat tick.
pub fn build_heartbeat_prompt(heartbeat_md: Option<&str>, inbound_pending: usize) -> String {
    let mut prompt = String::from(
        "You are running a periodic **heartbeat** (background maintenance, not a live user chat).\n\
         \n\
         1. Work through actionable items in HEARTBEAT.md (below if present).\n\
         2. **Reflect** on recent issues: failed exec/server checks, open problems from sessions, \
            MEMORY.md / today's memory, and anything that still needs a fix.\n",
    );
    if inbound_pending > 0 {
        prompt.push_str(&format!(
            "3. **Inbound queue:** {inbound_pending} message(s) are waiting for the agent — \
             note anything urgent; do not claim you processed them unless you actually did.\n",
        ));
    } else {
        prompt.push_str(
            "3. **Inbound queue:** empty — no user messages are waiting.\n",
        );
    }
    prompt.push_str(
        "4. Use tools only when needed (exec, read_file, write_file). Keep the reply concise.\n\
         5. If nothing needs attention after review, reply with **exactly**: HEARTBEAT_OK\n",
    );
    if let Some(md) = heartbeat_md {
        prompt.push_str("\n---\n\n## HEARTBEAT.md (current)\n\n");
        prompt.push_str(md);
    }
    prompt
}

// ─────────────────────────────────────────────
// HeartbeatService
// ─────────────────────────────────────────────

/// Periodic heartbeat that wakes the agent to check `HEARTBEAT.md`.
pub struct HeartbeatService {
    /// Workspace root (where `HEARTBEAT.md` lives).
    workspace: PathBuf,
    /// Optional bus — used to report inbound queue depth in the prompt.
    bus: Option<Arc<MessageBus>>,
    /// Callback to invoke (typically `agent.process_direct_session()`).
    on_heartbeat: Option<OnHeartbeatFn>,
    /// Interval in seconds between heartbeats.
    interval_s: u64,
    /// Whether the service is enabled.
    enabled: bool,
    /// Shutdown signal.
    shutdown: Arc<Notify>,
}

impl HeartbeatService {
    /// Create a new heartbeat service.
    pub fn new(
        workspace: PathBuf,
        bus: Option<Arc<MessageBus>>,
        on_heartbeat: Option<OnHeartbeatFn>,
        interval_s: Option<u64>,
        enabled: bool,
    ) -> Self {
        Self {
            workspace,
            bus,
            on_heartbeat,
            interval_s: interval_s.unwrap_or(DEFAULT_HEARTBEAT_INTERVAL_S),
            enabled,
            shutdown: Arc::new(Notify::new()),
        }
    }

    /// Set the heartbeat callback.
    pub fn set_on_heartbeat(&mut self, callback: OnHeartbeatFn) {
        self.on_heartbeat = Some(callback);
    }

    /// Path to `HEARTBEAT.md`.
    pub fn heartbeat_file(&self) -> PathBuf {
        self.workspace.join("HEARTBEAT.md")
    }

    /// Read `HEARTBEAT.md` content, returning `None` if it doesn't exist.
    pub fn read_heartbeat_file(&self) -> Option<String> {
        let path = self.heartbeat_file();
        if path.exists() {
            std::fs::read_to_string(&path).ok()
        } else {
            None
        }
    }

    /// True when `HEARTBEAT.md` has no actionable lines.
    pub fn is_file_empty(&self) -> bool {
        Self::is_heartbeat_empty(self.read_heartbeat_file().as_deref())
    }

    /// Check if `HEARTBEAT.md` has no actionable content.
    ///
    /// Lines that are empty, headers (#), HTML comments, or bare checkboxes
    /// are not considered actionable.
    pub fn is_heartbeat_empty(content: Option<&str>) -> bool {
        let content = match content {
            Some(c) if !c.is_empty() => c,
            _ => return true,
        };

        for line in content.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty()
                || trimmed.starts_with('#')
                || trimmed.starts_with("<!--")
                || is_bare_checkbox_line(trimmed)
            {
                continue;
            }
            return false;
        }

        true
    }

    fn inbound_pending(&self) -> usize {
        self.bus.as_ref().map(|b| b.inbound_pending()).unwrap_or(0)
    }

    /// Start the heartbeat service (blocking async loop).
    ///
    /// Returns when `stop()` is called.
    pub async fn start(&self) -> anyhow::Result<()> {
        if !self.enabled {
            info!("heartbeat disabled");
            self.shutdown.notified().await;
            return Ok(());
        }

        info!(interval_s = self.interval_s, "heartbeat service started");

        // Run once soon after startup (do not wait a full interval first).
        self.tick().await;

        loop {
            let sleep_duration = std::time::Duration::from_secs(self.interval_s);

            tokio::select! {
                _ = tokio::time::sleep(sleep_duration) => {
                    self.tick().await;
                }
                _ = self.shutdown.notified() => {
                    info!("heartbeat service shutting down");
                    return Ok(());
                }
            }
        }
    }

    /// Stop the heartbeat service.
    pub fn stop(&self) {
        info!("stopping heartbeat service");
        self.shutdown.notify_waiters();
    }

    /// Execute a single heartbeat tick.
    pub async fn tick(&self) {
        let content = self.read_heartbeat_file();

        if Self::is_heartbeat_empty(content.as_deref()) {
            debug!("heartbeat: no tasks (HEARTBEAT.md empty)");
            return;
        }

        info!(
            inbound_pending = self.inbound_pending(),
            "heartbeat: checking for tasks"
        );

        let prompt = build_heartbeat_prompt(content.as_deref(), self.inbound_pending());

        if let Some(ref callback) = self.on_heartbeat {
            match callback(prompt).await {
                Ok(response) => {
                    let normalized = response.to_uppercase().replace('_', "");
                    let token = HEARTBEAT_OK_TOKEN.replace('_', "");
                    if normalized.contains(&token) {
                        info!("heartbeat: OK (no action needed)");
                    } else {
                        info!(
                            preview = %truncate_for_log(&response, 200),
                            "heartbeat: completed task"
                        );
                    }
                }
                Err(e) => {
                    error!(error = %e, "heartbeat execution failed");
                }
            }
        }
    }

    /// Manually trigger a heartbeat (CLI or testing). Runs even when the file looks empty if `force`.
    pub async fn trigger_now(&self, force: bool) -> Option<anyhow::Result<String>> {
        let content = self.read_heartbeat_file();
        if !force && Self::is_heartbeat_empty(content.as_deref()) {
            return Some(Ok("HEARTBEAT_SKIPPED (no actionable tasks in HEARTBEAT.md)".to_string()));
        }
        let prompt = build_heartbeat_prompt(content.as_deref(), self.inbound_pending());
        if let Some(ref callback) = self.on_heartbeat {
            Some(callback(prompt).await)
        } else {
            None
        }
    }
}

fn is_bare_checkbox_line(trimmed: &str) -> bool {
    trimmed == "- [ ]"
        || trimmed == "* [ ]"
        || trimmed == "- [x]"
        || trimmed == "* [x]"
}

fn truncate_for_log(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    format!(
        "{}…",
        s.chars()
            .take(max_chars.saturating_sub(1))
            .collect::<String>()
    )
}

// ─────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_heartbeat_prompt_includes_queue() {
        let p = build_heartbeat_prompt(Some("# Tasks\nFix port 5000\n"), 3);
        assert!(p.contains("Inbound queue"));
        assert!(p.contains('3'));
        assert!(p.contains("Fix port 5000"));
    }

    #[test]
    fn test_is_heartbeat_empty_none() {
        assert!(HeartbeatService::is_heartbeat_empty(None));
    }

    #[test]
    fn test_is_heartbeat_empty_blank() {
        assert!(HeartbeatService::is_heartbeat_empty(Some("")));
        assert!(HeartbeatService::is_heartbeat_empty(Some("  \n  \n")));
    }

    #[test]
    fn test_is_heartbeat_empty_headers_only() {
        let content = "# Heartbeat Tasks\n\n## Active\n\n<!-- comment -->\n";
        assert!(HeartbeatService::is_heartbeat_empty(Some(content)));
    }

    #[test]
    fn test_is_heartbeat_not_empty() {
        let content = "# Tasks\n\nCheck the deployments\n";
        assert!(!HeartbeatService::is_heartbeat_empty(Some(content)));
    }

    #[test]
    fn test_is_heartbeat_empty_checkboxes() {
        let content = "# Tasks\n- [ ]\n* [x]\n";
        assert!(HeartbeatService::is_heartbeat_empty(Some(content)));
    }

    #[test]
    fn test_is_heartbeat_not_empty_with_task() {
        let content = "# Tasks\n- [ ] Deploy v2.0\n";
        assert!(!HeartbeatService::is_heartbeat_empty(Some(content)));
    }

    #[test]
    fn test_user_heartbeat_md_is_not_empty() {
        let content = r#"# Heartbeat Tasks
### Hourly System Check
- **Schedule:** Every hour
- **Task:** Run system diagnostics check
"#;
        assert!(!HeartbeatService::is_heartbeat_empty(Some(content)));
    }

    #[tokio::test]
    async fn test_trigger_now_no_callback() {
        let service = HeartbeatService::new(
            PathBuf::from("/tmp/test-heartbeat"),
            None,
            None,
            Some(60),
            true,
        );
        let result = service.trigger_now(true).await;
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_trigger_now_with_callback() {
        let callback: OnHeartbeatFn = Arc::new(|_prompt| {
            Box::pin(async { Ok("HEARTBEAT_OK".to_string()) })
        });
        let service = HeartbeatService::new(
            PathBuf::from("/tmp/test-heartbeat"),
            None,
            Some(callback),
            Some(60),
            true,
        );
        let result = service.trigger_now(true).await;
        assert!(result.is_some());
        assert_eq!(result.unwrap().unwrap(), "HEARTBEAT_OK");
    }

    #[tokio::test]
    async fn test_stop_exits_loop() {
        let service = Arc::new(HeartbeatService::new(
            PathBuf::from("/tmp/test-heartbeat"),
            None,
            None,
            Some(1),
            true,
        ));

        let svc = service.clone();
        let handle = tokio::spawn(async move { svc.start().await });

        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        service.stop();

        let result = handle.await.unwrap();
        assert!(result.is_ok());
    }
}
