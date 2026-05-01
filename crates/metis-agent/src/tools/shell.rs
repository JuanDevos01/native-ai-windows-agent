//! Shell tool — execute commands in a subprocess.
//!
//! Port of nanobot's `agent/tools/shell.py` `ExecTool`.
//! Includes deny-pattern safety guard and optional workspace restriction.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use regex::Regex;
use serde_json::{json, Value};
use tokio::process::Command;
use tokio::sync::Mutex;
use tracing::{info, warn};

use super::base::{optional_string, require_string, Tool};

/// Maximum output length before truncation (characters).
const MAX_OUTPUT_LEN: usize = 10_000;

/// Default command timeout in seconds.
const DEFAULT_TIMEOUT_SECS: u64 = 60;

/// Dangerous command patterns that are always blocked.
const DENY_PATTERNS: &[&str] = &[
    r"\brm\s+-[rf]{1,2}\b",
    r"\bdel\s+/[fq]\b",
    r"\brmdir\s+/s\b",
    r"\b(format|mkfs|diskpart)\b",
    r"\bdd\s+if=",
    r">\s*/dev/sd",
    r"\b(shutdown|reboot|poweroff)\b",
    r":\(\)\s*\{.*\};\s*:",   // fork bomb
    r"\berase\b",
    r"\brd\s+/s\b",
    r"\bremove-item\b.*\b(recurse|force)\b",
    r"\breg\s+delete\b",
];

/// Commands that may exfiltrate data and must always require approval.
const EXFIL_PATTERNS: &[&str] = &[
    r"\bftp\b",
    r"\bftps\b",
    r"\bscp\b",
    r"\bsftp\b",
    r"\brsync\b",
    r"\bcurl\b.*\b(-t|--upload-file|-f\s+@|--form)\b",
    r"\bwget\b.*\b(--post-file|--body-file)\b",
    r"\bsend-mailmessage\b",
    r"\bblat\b",
    r"\bpowershell\b.*\bsend-mailmessage\b",
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PermissionMode {
    UnsafeOnly,
    Always,
    PowerUser,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ShellBackend {
    Cmd,
    PowerShell,
    Sh,
}

static APPROVAL_COUNTER: AtomicU64 = AtomicU64::new(1);

// ─────────────────────────────────────────────
// ExecTool
// ─────────────────────────────────────────────

/// Execute shell commands in a subprocess.
pub struct ExecTool {
    /// Working directory for commands.
    working_dir: PathBuf,
    /// Command timeout.
    timeout: Duration,
    /// If true, block commands that reference paths outside `working_dir`.
    restrict_to_workspace: bool,
    /// Compiled deny regexes (built once at construction).
    deny_regexes: Vec<Regex>,
    /// Compiled data-exfiltration regexes requiring explicit approval.
    exfil_regexes: Vec<Regex>,
    /// Shell backend to use when executing commands.
    shell_backend: ShellBackend,
    /// Permission policy for command execution.
    permission_mode: PermissionMode,
    /// Pending commands waiting for explicit approval.
    pending_approvals: Mutex<HashMap<String, (String, String)>>,
}

impl ExecTool {
    /// Create a new `ExecTool`.
    pub fn new(
        working_dir: PathBuf,
        timeout_secs: Option<u64>,
        shell: Option<String>,
        permission_mode: Option<String>,
        restrict_to_workspace: bool,
    ) -> Self {
        let deny_regexes: Vec<Regex> = DENY_PATTERNS
            .iter()
            .filter_map(|p| Regex::new(p).ok())
            .collect();
        let exfil_regexes: Vec<Regex> = EXFIL_PATTERNS
            .iter()
            .filter_map(|p| Regex::new(p).ok())
            .collect();

        let shell_backend = Self::parse_shell_backend(shell.as_deref());
        let permission_mode = Self::parse_permission_mode(permission_mode.as_deref());

        Self {
            working_dir,
            timeout: Duration::from_secs(timeout_secs.unwrap_or(DEFAULT_TIMEOUT_SECS)),
            restrict_to_workspace,
            deny_regexes,
            exfil_regexes,
            shell_backend,
            permission_mode,
            pending_approvals: Mutex::new(HashMap::new()),
        }
    }

    fn make_approval_token() -> String {
        format!("{:08x}", APPROVAL_COUNTER.fetch_add(1, Ordering::Relaxed))
    }

    fn parse_permission_mode(mode: Option<&str>) -> PermissionMode {
        match mode.unwrap_or("unsafe_only").trim().to_lowercase().as_str() {
            "always" => PermissionMode::Always,
            "poweruser" => PermissionMode::PowerUser,
            _ => PermissionMode::UnsafeOnly,
        }
    }

    fn parse_shell_backend(shell: Option<&str>) -> ShellBackend {
        let default_shell = if cfg!(target_os = "windows") {
            "powershell"
        } else {
            "sh"
        };
        match shell.unwrap_or(default_shell).trim().to_lowercase().as_str() {
            "cmd" if cfg!(target_os = "windows") => ShellBackend::Cmd,
            "powershell" | "pwsh" if cfg!(target_os = "windows") => ShellBackend::PowerShell,
            "sh" | "bash" => ShellBackend::Sh,
            _ => {
                if cfg!(target_os = "windows") {
                    ShellBackend::PowerShell
                } else {
                    ShellBackend::Sh
                }
            }
        }
    }

    fn is_exfiltration_command(&self, command: &str) -> bool {
        let lower = command.to_lowercase();
        self.exfil_regexes.iter().any(|re| re.is_match(&lower))
    }

    fn shell_backend_label(&self) -> &'static str {
        match self.shell_backend {
            ShellBackend::Cmd => "cmd",
            ShellBackend::PowerShell => "powershell",
            ShellBackend::Sh => "sh",
        }
    }

    /// Wrap tool output so the model/human can tell real execution from narration.
    /// Only successful `execute_command` completions use this wrapper.
    fn wrap_exec_report(
        command: &str,
        cwd: &str,
        shell_backend: &str,
        exit_code: i32,
        stdout: &str,
        stderr: &str,
    ) -> String {
        let header = format!(
            "<<<EXEC_RESULT>>>\nCOMMAND: {command}\nWORKING_DIR: {cwd}\nSHELL_BACKEND: {shell_backend}\nEXIT_CODE: {exit_code}\nSTATUS: {}\n<<<END_EXEC_RESULT>>>",
            if exit_code == 0 { "SUCCESS" } else { "FAILED" }
        );

        let mut body = String::new();
        if !stdout.is_empty() {
            body.push_str("--- STDOUT ---\n");
            body.push_str(stdout);
            if !body.ends_with('\n') {
                body.push('\n');
            }
        }
        if !stderr.is_empty() {
            body.push_str("--- STDERR ---\n");
            body.push_str(stderr);
            if !body.ends_with('\n') {
                body.push('\n');
            }
        }
        if stdout.is_empty() && stderr.is_empty() {
            body.push_str("(no stdout/stderr)\n");
        }

        let sep_len = 1usize; // newline between header block and stdout/stderr
        let avail_for_body = MAX_OUTPUT_LEN.saturating_sub(header.len() + sep_len + 64);
        if body.len() > avail_for_body {
            let omit = body.len() - avail_for_body;
            body.truncate(avail_for_body);
            body.push_str(&format!(
                "\n... (truncated stdout/stderr; {omit} chars omitted)"
            ));
        }

        format!("{header}\n{body}")
    }

    fn wrap_exec_timeout(command: &str, cwd: &str, shell_backend: &str, secs: u64) -> String {
        format!(
            "<<<EXEC_RESULT>>>\nCOMMAND: {command}\nWORKING_DIR: {cwd}\nSHELL_BACKEND: {shell_backend}\nSTATUS: TIMEOUT\nTIMEOUT_SECONDS: {secs}\n<<<END_EXEC_RESULT>>>\n(no process output — command exceeded timeout)"
        )
    }

    async fn execute_command(&self, command: &str, cwd: &str) -> anyhow::Result<String> {
        info!(command = %command, cwd = %cwd, "executing shell command");

        let shell_label = self.shell_backend_label();

        let mut process = match self.shell_backend {
            ShellBackend::Cmd => {
                let mut c = Command::new("cmd");
                c.args(["/C", command]);
                c
            }
            ShellBackend::PowerShell => {
                let mut c = Command::new("powershell");
                c.args(["-NoProfile", "-Command", command]);
                c
            }
            ShellBackend::Sh => {
                let mut c = Command::new("sh");
                c.args(["-c", command]);
                c
            }
        };

        let child = process
            .current_dir(cwd)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(|e| anyhow::anyhow!("Failed to spawn command: {e}"))?;

        let result = tokio::time::timeout(self.timeout, child.wait_with_output()).await;
        match result {
            Ok(Ok(output)) => {
                let stdout = String::from_utf8_lossy(&output.stdout).to_string();
                let stderr = String::from_utf8_lossy(&output.stderr).to_string();
                let code = output.status.code().unwrap_or(-1);

                Ok(Self::wrap_exec_report(
                    command,
                    cwd,
                    shell_label,
                    code,
                    &stdout,
                    &stderr,
                ))
            }
            Ok(Err(e)) => anyhow::bail!("Command failed: {e}"),
            Err(_) => Ok(Self::wrap_exec_timeout(
                command,
                cwd,
                shell_label,
                self.timeout.as_secs(),
            )),
        }
    }

    /// Check command policy and return an approval-needed message or block reason.
    fn guard_command(&self, command: &str, cwd: &str) -> Option<String> {
        let lower = command.to_lowercase();

        if self.permission_mode == PermissionMode::Always {
            return Some(format!(
                "Permission required: approval_mode=always. Command not executed.\nProposed command: {command}"
            ));
        }

        // Exfiltration always needs approval in every mode.
        if self.is_exfiltration_command(command) {
            return Some(format!(
                "Permission required: potential data exfiltration command detected. Command not executed.\nProposed command: {command}"
            ));
        }

        if self.permission_mode == PermissionMode::PowerUser {
            return None;
        }

        // Check deny patterns
        for re in &self.deny_regexes {
            if re.is_match(&lower) {
                warn!(command = command, "command blocked by safety guard");
                return Some(
                    format!(
                        "Permission required: unsafe command pattern detected. Command not executed.\nProposed command: {command}"
                    ),
                );
            }
        }

        // Workspace restriction
        if self.restrict_to_workspace {
            // Block path traversal
            if command.contains("../") || command.contains("..\\") {
                return Some(
                    "Error: Command blocked — path traversal (../) not allowed in restricted mode"
                        .into(),
                );
            }

            // Check for absolute paths outside workspace
            let cwd_path = PathBuf::from(cwd);
            let abs_path_re = Regex::new(r#"(?:/[^\s"']+|[A-Za-z]:\\[^\s"']+)"#).ok();
            if let Some(re) = abs_path_re {
                for cap in re.find_iter(command) {
                    let p = PathBuf::from(cap.as_str());
                    let resolved = if p.exists() {
                        p.canonicalize().unwrap_or(p)
                    } else {
                        p
                    };
                    if !resolved.starts_with(&cwd_path) {
                        return Some(format!(
                            "Permission required: command references path '{}' outside workspace. Command not executed.\nProposed command: {}",
                            cap.as_str(),
                            command
                        ));
                    }
                }
            }
        }

        None
    }
}

#[async_trait]
impl Tool for ExecTool {
    fn name(&self) -> &str {
        "exec"
    }

    fn description(&self) -> &str {
        "Execute a shell command and return captured output. \
         When a command actually runs, the result includes a fenced block <<<EXEC_RESULT>>>…<<<END_EXEC_RESULT>>> \
         with COMMAND, WORKING_DIR, SHELL_BACKEND, EXIT_CODE, and STATUS — treat only that block as proof the tool ran something. \
         Blocked commands and approval prompts do not contain this block."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": "The shell command to execute"
                },
                "working_dir": {
                    "type": "string",
                    "description": "Optional working directory (defaults to workspace root)"
                },
                "approve_token": {
                    "type": "string",
                    "description": "Approval token from a previous permission-required result"
                }
            },
            "required": ["command"]
        })
    }

    async fn execute(&self, params: HashMap<String, Value>) -> anyhow::Result<String> {
        let command = require_string(&params, "command")?;
        let cwd = optional_string(&params, "working_dir")
            .unwrap_or_else(|| self.working_dir.to_string_lossy().to_string());
        let approve_token = optional_string(&params, "approve_token");

        if let Some(token) = approve_token {
            let pending = {
                let mut pending = self.pending_approvals.lock().await;
                pending.remove(&token)
            };
            if let Some((approved_command, approved_cwd)) = pending {
                return self.execute_command(&approved_command, &approved_cwd).await;
            }
            return Ok(format!(
                "Error: approval token '{}' not found or already used.",
                token
            ));
        }

        // Safety check
        if let Some(err) = self.guard_command(&command, &cwd) {
            let token = Self::make_approval_token();
            {
                let mut pending = self.pending_approvals.lock().await;
                pending.insert(token.clone(), (command.clone(), cwd.clone()));
            }
            return Ok(format!(
                "{err}\nApproval token: {token}\nTo approve, run exec again with same command and `approve_token`."
            ));
        }
        self.execute_command(&command, &cwd).await
    }
}

// ─────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn make_params(pairs: &[(&str, &str)]) -> HashMap<String, Value> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), Value::String(v.to_string())))
            .collect()
    }

    #[tokio::test]
    async fn test_exec_echo() {
        let dir = tempfile::tempdir().unwrap();
        let tool = ExecTool::new(dir.path().to_path_buf(), Some(10), None, None, false);
        let result = tool
            .execute(make_params(&[("command", "echo hello")]))
            .await
            .unwrap();
        assert!(result.contains("<<<EXEC_RESULT>>>"));
        assert!(result.contains("EXIT_CODE:"));
        assert!(result.contains("hello"));
    }

    #[tokio::test]
    async fn test_exec_exit_code() {
        let dir = tempfile::tempdir().unwrap();
        let tool = ExecTool::new(dir.path().to_path_buf(), Some(10), None, None, false);
        let result = tool
            .execute(make_params(&[("command", "exit 42")]))
            .await
            .unwrap();
        assert!(result.contains("EXIT_CODE: 42"));
        assert!(result.contains("STATUS: FAILED"));
    }

    #[test]
    fn test_guard_blocks_rm_rf() {
        let cwd = std::env::temp_dir();
        let cwd_str = cwd.to_string_lossy().to_string();
        let tool = ExecTool::new(cwd.clone(), None, None, None, false);
        let guard = tool.guard_command("rm -rf /", &cwd_str);
        assert!(guard.is_some());
        assert!(guard.unwrap().contains("Permission required"));
    }

    #[test]
    fn test_guard_blocks_fork_bomb() {
        let cwd = std::env::temp_dir();
        let cwd_str = cwd.to_string_lossy().to_string();
        let tool = ExecTool::new(cwd.clone(), None, None, None, false);
        let guard = tool.guard_command(":() { :|:& };:", &cwd_str);
        assert!(guard.is_some());
    }

    #[test]
    fn test_guard_blocks_shutdown() {
        let cwd = std::env::temp_dir();
        let cwd_str = cwd.to_string_lossy().to_string();
        let tool = ExecTool::new(cwd.clone(), None, None, None, false);
        let guard = tool.guard_command("sudo shutdown -h now", &cwd_str);
        assert!(guard.is_some());
    }

    #[test]
    fn test_guard_allows_safe_commands() {
        let cwd = std::env::temp_dir();
        let cwd_str = cwd.to_string_lossy().to_string();
        let tool = ExecTool::new(cwd.clone(), None, None, None, false);
        assert!(tool.guard_command("echo hello", &cwd_str).is_none());
        assert!(tool.guard_command("ls -la", &cwd_str).is_none());
        assert!(tool.guard_command("cat file.txt", &cwd_str).is_none());
        assert!(tool.guard_command("cargo test", &cwd_str).is_none());
    }

    #[test]
    fn test_guard_blocks_traversal_in_restricted_mode() {
        let workspace = std::env::temp_dir().join("METIS_workspace_guard");
        let workspace_str = workspace.to_string_lossy().to_string();
        let traversal_cmd = if cfg!(target_os = "windows") {
            "type ..\\..\\..\\windows\\system32\\drivers\\etc\\hosts"
        } else {
            "cat ../../../etc/passwd"
        };
        let tool = ExecTool::new(workspace, None, None, None, true);
        let guard = tool.guard_command(traversal_cmd, &workspace_str);
        assert!(guard.is_some());
        assert!(guard.unwrap().contains("path traversal"));
    }

    #[tokio::test]
    async fn test_exec_timeout() {
        let dir = tempfile::tempdir().unwrap();
        let tool = ExecTool::new(dir.path().to_path_buf(), Some(1), None, None, false);
        let sleep_cmd = if cfg!(target_os = "windows") {
            // `ping` is broadly available on Windows and gives a stable delay.
            "ping 127.0.0.1 -n 31 > nul"
        } else {
            "sleep 30"
        };
        let result = tool
            .execute(make_params(&[("command", sleep_cmd)]))
            .await
            .unwrap();
        assert!(result.contains("<<<EXEC_RESULT>>>"));
        assert!(result.contains("STATUS: TIMEOUT"));
    }

    #[test]
    fn test_tool_definition() {
        let tool = ExecTool::new(std::env::temp_dir(), None, None, None, false);
        let def = tool.to_definition();
        assert_eq!(def.function.name, "exec");
        assert_eq!(def.tool_type, "function");
    }

    #[tokio::test]
    async fn test_approval_token_flow_always_mode() {
        let dir = tempfile::tempdir().unwrap();
        let tool = ExecTool::new(
            dir.path().to_path_buf(),
            Some(10),
            None,
            Some("always".to_string()),
            false,
        );

        let blocked = tool
            .execute(make_params(&[("command", "echo hello")]))
            .await
            .unwrap();
        assert!(blocked.contains("Approval token:"));

        let token = blocked
            .split("Approval token:")
            .nth(1)
            .map(|s| s.lines().next().unwrap_or("").trim().to_string())
            .unwrap_or_default();
        assert!(!token.is_empty());

        let approved = tool
            .execute(make_params(&[
                ("command", "echo hello"),
                ("approve_token", token.as_str()),
            ]))
            .await
            .unwrap();
        assert!(approved.contains("<<<EXEC_RESULT>>>"));
        assert!(approved.contains("hello"));
    }
}
