//! Agent loop — the LLM ↔ tool-calling main loop.
//!
//! Port of nanobot's `agent/loop.py`.
//! Receives inbound messages, builds context, calls the LLM, dispatches
//! tool calls, and publishes outbound responses.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use serde_json::json;
use tracing::{debug, error, info};

use metis_core::bus::queue::MessageBus;
use metis_core::bus::types::{InboundMessage, OutboundMessage};
use metis_core::session::manager::SessionManager;
use metis_core::types::{Message, ToolCall, LlmResponse};
use metis_providers::traits::{LlmProvider, LlmRequestConfig};

use crate::context::ContextBuilder;
use crate::subagent::SubagentManager;
use crate::tools::message::MessageTool;
use crate::tools::registry::ToolRegistry;
use crate::tools::browser::BrowserTool;
use crate::tools::filesystem::{EditFileTool, ListDirTool, ReadFileTool, WriteFileTool};
use crate::tools::shell::ExecTool;
use crate::tools::spawn::SpawnTool;
use crate::tools::web::{WebFetchTool, WebSearchTool};

use regex::Regex;
use std::sync::OnceLock;

/// Default maximum LLM ↔ tool iterations per user message.
const DEFAULT_MAX_ITERATIONS: usize = 20;

/// Substring present in every real `exec` tool result (`tools/shell.rs`).
const EXEC_RESULT_MARKER: &str = "<<<EXEC_RESULT>>>";
const END_EXEC_RESULT_MARKER: &str = "<<<END_EXEC_RESULT>>>";

/// Max characters of the COMMAND line shown in chat-app exec summaries.
const CHAT_EXEC_COMMAND_PREVIEW: usize = 120;

/// Tracing target for model `reasoning_content` JSON lines (see `OutboundFormatting::log_thinking_json`).
pub const THINKING_LOG_TARGET: &str = "metis_thinking";

// ─────────────────────────────────────────────
// Outbound text (Telegram / Discord / WhatsApp)
// ─────────────────────────────────────────────

/// Controls optional model-reasoning logs and fenced-code stripping for chat apps.
#[derive(Clone, Debug)]
pub struct OutboundFormatting {
    /// Log each LLM response's `reasoning_content` as one JSON line at DEBUG (`target: metis_thinking`).
    pub log_thinking_json: bool,
    /// When false (default), outbound replies on Telegram, Discord, and WhatsApp have markdown
    /// fenced code blocks replaced by a short placeholder (session history is unchanged).
    pub include_fenced_code_in_chat_apps: bool,
    /// When false (default), `<<<EXEC_RESULT>>>` blocks (and stdout/stderr tails) are replaced
    /// with a one-line summary on Telegram, Discord, and WhatsApp. Session history is unchanged.
    pub include_exec_output_in_chat_apps: bool,
}

impl Default for OutboundFormatting {
    fn default() -> Self {
        Self {
            log_thinking_json: true,
            include_fenced_code_in_chat_apps: false,
            include_exec_output_in_chat_apps: false,
        }
    }
}

fn is_chat_app_channel(channel: &str) -> bool {
    channel.eq_ignore_ascii_case("telegram")
        || channel.eq_ignore_ascii_case("discord")
        || channel.eq_ignore_ascii_case("whatsapp")
}

static FENCED_CODE_BLOCK_RE: OnceLock<Regex> = OnceLock::new();

fn fenced_code_block_re() -> &'static Regex {
    FENCED_CODE_BLOCK_RE.get_or_init(|| Regex::new(r"(?s)```.*?```").expect("fenced code block regex"))
}

fn truncate_chars(s: &str, max_chars: usize) -> String {
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

fn parse_exec_exit_code(block: &str) -> Option<i32> {
    for line in block.lines() {
        if let Some(rest) = line.strip_prefix("EXIT_CODE: ") {
            return rest.trim().parse().ok();
        }
    }
    None
}

/// True when a wrapped exec tool report indicates failure.
pub fn exec_report_failed(block: &str) -> bool {
    if block.contains("STATUS: FAILED") || block.contains("STATUS: TIMEOUT") {
        return true;
    }
    parse_exec_exit_code(block).is_some_and(|c| c != 0)
}

fn first_stderr_summary_line(block: &str) -> Option<String> {
    let mut in_stderr = false;
    for line in block.lines() {
        if line == "--- STDERR ---" {
            in_stderr = true;
            continue;
        }
        if in_stderr {
            let t = line.trim();
            if t.is_empty() || t.starts_with("At line:") || t.starts_with('+') || t.starts_with('~') {
                continue;
            }
            if t.starts_with("+ CategoryInfo") || t.starts_with("+ FullyQualifiedErrorId") {
                continue;
            }
            return Some(truncate_chars(t, 100));
        }
    }
    None
}

/// One-line summary of an `<<<EXEC_RESULT>>>` report (header + optional stderr hint).
pub fn summarize_exec_block(block: &str) -> String {
    let mut command = None;
    let mut exit_code = None;
    let mut status = None;
    for line in block.lines() {
        if let Some(rest) = line.strip_prefix("COMMAND: ") {
            command = Some(rest);
        } else if let Some(rest) = line.strip_prefix("EXIT_CODE: ") {
            exit_code = Some(rest.trim());
        } else if let Some(rest) = line.strip_prefix("STATUS: ") {
            status = Some(rest.trim());
        }
    }
    let preview = truncate_chars(command.unwrap_or("(unknown command)"), CHAT_EXEC_COMMAND_PREVIEW);
    if exec_report_failed(block) {
        let ec = exit_code.unwrap_or("?");
        let err = first_stderr_summary_line(block).unwrap_or_else(|| "command failed".to_string());
        return format!("✗ `{preview}` failed (exit {ec}): {err}");
    }
    match (exit_code, status) {
        (Some(ec), Some(st)) => format!("✓ Ran `{preview}` (exit {ec}, {st})"),
        (Some(ec), None) => format!("✓ Ran `{preview}` (exit {ec})"),
        (None, Some(st)) => format!("✓ Ran `{preview}` ({st})"),
        _ => format!("✓ Ran `{preview}`"),
    }
}

/// Remove all `<<<EXEC_RESULT>>>` reports (and stdout/stderr tails) from model-authored text.
pub fn strip_all_exec_reports_from_text(text: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(start) = rest.find(EXEC_RESULT_MARKER) {
        out.push_str(&rest[..start]);
        let block = &rest[start..];
        if let Some(end_rel) = block.find(END_EXEC_RESULT_MARKER) {
            let header_end = end_rel + END_EXEC_RESULT_MARKER.len();
            rest = skip_exec_process_output_tail(&block[header_end..]);
        } else {
            break;
        }
    }
    out.push_str(rest);
    out.trim().to_string()
}

/// Drop short optimistic "it's running" lines the model repeats when checks fail.
fn strip_optimistic_running_claims(text: &str) -> String {
    let kept: Vec<&str> = text
        .lines()
        .filter(|line| {
            let t = line.trim();
            let l = t.to_lowercase();
            if t.is_empty() {
                return true;
            }
            if l.contains("running!") || l.contains("server is running") || l.contains("server running!") {
                return false;
            }
            if l.starts_with("open http://") && (l.contains("localhost") || l.contains("127.0.0.1")) {
                return false;
            }
            true
        })
        .collect();
    kept.join("\n").trim().to_string()
}

fn skip_exec_process_output_tail(s: &str) -> &str {
    let s = s.trim_start_matches('\n');
    loop {
        if s.starts_with("--- STDOUT ---")
            || s.starts_with("--- STDERR ---")
            || s.starts_with("(no stdout/stderr)")
        {
            if let Some(pos) = s.find(EXEC_RESULT_MARKER) {
                return &s[pos..];
            }
            return "";
        }
        break;
    }
    s
}

/// Replace exec tool reports with short summaries (for Telegram/Discord/WhatsApp).
pub fn compact_exec_output_for_chat(text: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(start) = rest.find(EXEC_RESULT_MARKER) {
        out.push_str(&rest[..start]);
        let block = &rest[start..];
        if let Some(end_rel) = block.find(END_EXEC_RESULT_MARKER) {
            let header_end = end_rel + END_EXEC_RESULT_MARKER.len();
            out.push_str(&summarize_exec_block(&block[..header_end]));
            rest = skip_exec_process_output_tail(&block[header_end..]);
        } else {
            out.push_str(&summarize_exec_block(block));
            return out.trim().to_string();
        }
    }
    out.push_str(rest);
    out.trim().to_string()
}

/// Remove ``` ... ``` blocks (non-greedy). Used for chat-app outbound messages.
pub fn strip_markdown_fenced_code_blocks(text: &str) -> String {
    let re = fenced_code_block_re();
    let replaced = re.replace_all(text, "\n_[code omitted]_\n");
    let trimmed = replaced.trim();
    if trimmed.is_empty() {
        "(Assistant reply was only fenced code; set `agents.defaults.includeFencedCodeInChatApps` to show it.)"
            .to_string()
    } else {
        trimmed.to_string()
    }
}

/// Merge real exec tool output into the assistant reply.
///
/// Strips hallucinated `<<<EXEC_RESULT>>>` blocks from model text when real exec ran this turn,
/// so pasted `EXIT_CODE: 0` fiction cannot hide a failed tool call.
fn reconcile_exec_with_reply(content: String, exec_outputs: &[String], compact_exec: bool) -> String {
    let mut text = if exec_outputs.is_empty() {
        content
    } else {
        strip_all_exec_reports_from_text(&content)
    };

    if exec_outputs.iter().any(|o| exec_report_failed(o)) {
        text = strip_optimistic_running_claims(&text);
    }

    if exec_outputs.is_empty() {
        return text;
    }

    let block = if compact_exec {
        exec_outputs
            .iter()
            .map(|o| summarize_exec_block(o))
            .collect::<Vec<_>>()
            .join("\n")
    } else {
        exec_outputs.join("\n\n")
    };

    if exec_outputs.iter().any(|o| exec_report_failed(o)) {
        let intro = "The command did not succeed — the server/process is not confirmed running.";
        if text.trim().is_empty() {
            return format!("{intro}\n\n{block}");
        }
        return format!("{intro}\n\n{text}\n\n{block}");
    }

    let trimmed = text.trim_end();
    if trimmed.is_empty() {
        block
    } else {
        format!("{trimmed}\n\n{block}")
    }
}

fn has_untrusted_exec_block(content: &str, exec_outputs: &[String]) -> bool {
    exec_outputs.is_empty() && content.contains(EXEC_RESULT_MARKER)
}

fn looks_like_unverified_success_claim(content: &str) -> bool {
    let lower = content.to_lowercase();
    let has_claim = lower.contains("done! ✅")
        || lower.contains("found it! ✅")
        || lower.contains("running! ✅")
        || lower.contains("server is running")
        || lower.contains("server running!")
        || lower.contains("installed")
        || lower.contains("now installed")
        || lower.contains("downloaded")
        || lower.contains("model downloaded")
        || lower.contains("all files are there")
        || lower.contains("success");
    let has_execution_evidence = lower.contains("exit_code:")
        || lower.contains("status: success")
        || lower.contains("<<<end_exec_result>>>");
    has_claim && !has_execution_evidence
}

/// User is pushing back on a false "success" / "running" claim (do not run path-check guardrail).
fn is_user_challenging_agent_claims(input: &str) -> bool {
    let lower = input.trim().to_lowercase();
    lower.contains("how can you say")
        || lower.contains("why do you say")
        || lower.contains("you said it")
        || lower.contains("something different")
        || lower.contains("while the powershell")
        || lower.contains("but powershell")
        || lower.contains("not running")
        || lower.contains("isn't running")
        || lower.contains("is not running")
        || lower.contains("doesn't work")
        || lower.contains("does not work")
        || lower.contains("unable to connect")
        || lower.contains("connection refused")
        || (lower.contains("failed") && lower.contains("<<<exec_result>>>"))
}

fn is_execution_or_install_request(input: &str) -> bool {
    if is_user_challenging_agent_claims(input) {
        return false;
    }
    let lower = input.trim().to_lowercase();
    if lower.is_empty() {
        return false;
    }
    looks_like_direct_shell_command(&lower)
        || lower.contains("run ")
        || lower.contains("execute ")
        || lower.contains("install")
        || lower.contains("download")
        || lower.contains("run this script")
        || lower.contains("powershell script")
        || lower.contains("cmd /")
        || lower.contains("terminal")
        || lower.contains("verify that")
        || lower.contains("verify the")
}

fn should_use_script_file_mode(input: &str) -> bool {
    let lower = input.to_lowercase();
    (lower.contains("run the script")
        || lower.contains("execute script")
        || lower.contains("run this script")
        || lower.contains("powershell script"))
        && lower.contains("```powershell")
}

fn is_whisper_cpp_install_request(input: &str) -> bool {
    let lower = input.to_lowercase();
    let asks_install = lower.contains("install") || lower.contains("setup") || lower.contains("set up");
    let mentions_whisper = lower.contains("whisper.cpp") || lower.contains("whisper cpp") || lower.contains("whisper");
    asks_install && mentions_whisper
}

fn extract_powershell_code_block(input: &str) -> Option<String> {
    let marker = "```powershell";
    let start = input.to_lowercase().find(marker)?;
    let rest = &input[start + marker.len()..];
    let end = rest.find("```")?;
    let body = rest[..end].trim();
    if body.is_empty() {
        None
    } else {
        Some(body.to_string())
    }
}

/// Heuristic: treat plain command-like user messages as direct shell commands.
///
/// This avoids LLM tool-routing ambiguity for simple commands like:
/// `type C:\path\to\file`, `dir`, `ls`, `cat file.txt`, etc.
fn looks_like_direct_shell_command(input: &str) -> bool {
    let trimmed = input.trim();
    if trimmed.is_empty() || trimmed.contains('\n') {
        return false;
    }

    let lower = trimmed.to_lowercase();
    let conversational_prefixes = [
        "please ",
        "can you ",
        "could you ",
        "would you ",
        "run ",
        "execute ",
        "show ",
        "what ",
        "why ",
        "how ",
    ];
    if conversational_prefixes.iter().any(|p| lower.starts_with(p)) {
        return false;
    }

    let first = lower.split_whitespace().next().unwrap_or_default();
    let command_heads = [
        "type", "cat", "dir", "ls", "pwd", "whoami", "echo", "git", "cargo", "npm", "pnpm",
        "python", "node", "cmd", "powershell", "pwsh",
    ];
    command_heads.contains(&first)
}

/// Heuristic: natural-language request to output/read the long-term memory file.
fn is_memory_file_request(input: &str) -> bool {
    let lower = input.trim().to_lowercase();
    if lower.is_empty() {
        return false;
    }

    let mentions_memory_file =
        lower.contains("memory.md") || lower.contains("memory file") || lower.contains("memory.md file");
    let asks_to_show =
        lower.contains("output") || lower.contains("show") || lower.contains("read") || lower.contains("print");

    mentions_memory_file && asks_to_show
}

fn is_read_file_request(input: &str) -> bool {
    let lower = input.trim().to_lowercase();
    if lower.is_empty() {
        return false;
    }
    let asks_to_read =
        lower.contains("read") || lower.contains("show") || lower.contains("output") || lower.contains("print");
    let asks_to_write = lower.contains("write") || lower.contains("save") || lower.contains("update");
    asks_to_read && !asks_to_write
}

fn extract_probable_file_path(input: &str) -> Option<String> {
    for raw in input.split_whitespace() {
        let token = raw
            .trim_matches(|c| c == '"' || c == '\'' || c == '`' || c == ',' || c == '.' || c == ')' || c == '(');
        let looks_windows_abs = token.len() > 3
            && token.as_bytes().get(1) == Some(&b':')
            && (token.contains('\\') || token.contains('/'));
        let looks_unix_abs = token.starts_with('/');
        let looks_file = token.contains('.');
        if (looks_windows_abs || looks_unix_abs) && looks_file {
            return Some(token.to_string());
        }
    }
    None
}

fn extract_probable_paths(input: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for raw in input.split_whitespace() {
        let token = raw.trim_matches(|c| {
            c == '"' || c == '\'' || c == '`' || c == ',' || c == '.' || c == ')' || c == '(' || c == ':'
        });
        let looks_windows_abs = token.len() >= 7
            && token.as_bytes().get(1) == Some(&b':')
            && (token.contains('\\') || token.contains('/'));
        let looks_unix_abs = token.starts_with('/');
        // Skip likely malformed / shell-noisy path tokens.
        let has_illegal_shell_chars =
            token.contains('"') || token.contains(';') || token.contains('|') || token.contains('<') || token.contains('>');
        if (looks_windows_abs || looks_unix_abs)
            && !has_illegal_shell_chars
            && !out.iter().any(|p| p == token)
        {
            out.push(token.to_string());
        }
        if out.len() >= 5 {
            break;
        }
    }
    out
}

fn build_test_path_command(paths: &[String]) -> Option<String> {
    if paths.is_empty() {
        return None;
    }
    let quoted: Vec<String> = paths
        .iter()
        .map(|p| format!("'{}'", p.replace('\'', "''")))
        .collect();
    let array = quoted.join(", ");
    Some(format!(
        "$paths = @({array}); $paths | ForEach-Object {{ \"PATH=$($_) EXISTS=$(Test-Path -LiteralPath $_)\" }}"
    ))
}

fn looks_like_unexecuted_read_narration(content: &str) -> bool {
    let lower = content.to_lowercase();
    (lower.contains("reading back now") || lower.contains("reading now") || lower.contains("verifying:"))
        && (lower.contains("get-content") || lower.contains("type ") || lower.contains("cat "))
}

fn looks_like_unexecuted_exec_narration(content: &str) -> bool {
    let lower = content.to_lowercase();
    if lower.contains(EXEC_RESULT_MARKER) {
        return false;
    }
    let has_progress_words =
        lower.contains("running:") || lower.contains("executing:") || lower.contains("verifying:");
    let has_command_hint = lower.contains("get-content")
        || lower.contains("get-childitem")
        || lower.contains("select-object")
        || lower.contains("type ")
        || lower.contains("cat ")
        || lower.contains("powershell")
        || lower.contains(".exe ")
        || lower.contains(" cmd ")
        || lower.contains("bash ")
        || lower.contains("sh ");
    has_progress_words && has_command_hint
}

fn looks_like_powershell_cmdlet(token: &str) -> bool {
    let mut parts = token.splitn(2, '-');
    let Some(verb) = parts.next() else {
        return false;
    };
    let Some(noun) = parts.next() else {
        return false;
    };
    let valid = |s: &str| !s.is_empty() && s.chars().all(|c| c.is_ascii_alphabetic());
    valid(verb) && valid(noun)
}

fn extract_probable_command_from_narration(content: &str) -> Option<String> {
    for line in content.lines() {
        let trimmed = line.trim().trim_matches('`').trim_matches('"').trim();
        if trimmed.is_empty() || trimmed.starts_with('<') || trimmed.starts_with("---") {
            continue;
        }
        let lower = trimmed.to_lowercase();
        if lower.starts_with("running:")
            || lower.starts_with("executing:")
            || lower.starts_with("verifying:")
            || lower.starts_with("copy")
            || lower == "powershell"
            || lower.contains("reading now")
        {
            continue;
        }

        // Very permissive by design: this is a recovery path when model narrated
        // a command but omitted the actual tool call.
        if lower.contains(".exe ")
            || lower.starts_with("get-content ")
            || lower.starts_with("get-childitem ")
            || lower.starts_with("select-object ")
            || lower.starts_with("type ")
            || lower.starts_with("cat ")
            || lower.starts_with("dir ")
            || lower.starts_with("ls ")
            || lower.starts_with("git ")
            || lower.starts_with("cargo ")
            || lower.starts_with("npm ")
            || lower.starts_with("pnpm ")
            || lower.starts_with("python ")
            || lower.starts_with("node ")
            || lower.starts_with("powershell ")
            || lower.starts_with("cmd ")
            || lower
                .split_whitespace()
                .next()
                .is_some_and(looks_like_powershell_cmdlet)
        {
            return Some(trimmed.to_string());
        }
    }
    None
}

/// Configuration for the exec tool.
#[derive(Clone, Debug)]
pub struct ExecToolConfig {
    /// Timeout in seconds (default 60).
    pub timeout: u64,
    /// Shell backend ("powershell", "cmd", "sh").
    pub shell: String,
    /// Permission mode ("unsafe_only", "always", "poweruser").
    pub permission_mode: String,
}

impl Default for ExecToolConfig {
    fn default() -> Self {
        Self {
            timeout: 60,
            shell: if cfg!(target_os = "windows") {
                "powershell".to_string()
            } else {
                "sh".to_string()
            },
            permission_mode: "unsafe_only".to_string(),
        }
    }
}

// ─────────────────────────────────────────────
// AgentLoop
// ─────────────────────────────────────────────

/// The main agent loop: polls the message bus, calls the LLM, dispatches tools.
pub struct AgentLoop {
    /// Message bus for inbound/outbound messages.
    bus: Arc<MessageBus>,
    /// LLM provider.
    provider: Arc<dyn LlmProvider>,
    /// Workspace root.
    _workspace: PathBuf,
    /// Model to use (overrides provider default if set).
    model: String,
    /// Max LLM ↔ tool iterations per message.
    max_iterations: usize,
    /// LLM request config (temperature, max_tokens).
    request_config: LlmRequestConfig,
    /// Tool registry.
    tools: ToolRegistry,
    /// Context builder.
    context: ContextBuilder,
    /// Session manager.
    sessions: SessionManager,
    /// Reference to the message tool (for set_context).
    message_tool: Arc<MessageTool>,
    /// Spawn tool reference (for set_context).
    spawn_tool: Arc<SpawnTool>,
    /// Subagent manager (also held by SpawnTool; kept for direct access).
    #[allow(dead_code)]
    subagent_manager: Arc<SubagentManager>,
    /// Outbound formatting (thinking logs, fenced-code stripping for chat apps).
    outbound: OutboundFormatting,
}

impl AgentLoop {
    /// Create a new agent loop.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        bus: Arc<MessageBus>,
        provider: Arc<dyn LlmProvider>,
        workspace: PathBuf,
        model: Option<String>,
        max_iterations: Option<usize>,
        request_config: Option<LlmRequestConfig>,
        brave_api_key: Option<String>,
        exec_config: Option<ExecToolConfig>,
        restrict_to_workspace: bool,
        session_manager: Option<SessionManager>,
        agent_name: Option<String>,
        outbound_formatting: Option<OutboundFormatting>,
    ) -> Self {
        let model = model.unwrap_or_else(|| provider.default_model().to_string());
        let max_iterations = max_iterations.unwrap_or(DEFAULT_MAX_ITERATIONS);
        let request_config = request_config.unwrap_or_default();
        let exec_config = exec_config.unwrap_or_default();
        let agent_name = agent_name.unwrap_or_else(|| "Metis".into());
        let outbound = outbound_formatting.unwrap_or_default();
        let sessions =
            session_manager.unwrap_or_else(|| SessionManager::new(None).expect("failed to create session manager"));

        let context = ContextBuilder::new(&workspace, &agent_name);

        // Build tool registry
        let mut tools = ToolRegistry::new();
        let allowed_dir = if restrict_to_workspace {
            Some(workspace.clone())
        } else {
            None
        };

        tools.register(Arc::new(ReadFileTool::new(allowed_dir.clone())));
        tools.register(Arc::new(WriteFileTool::new(allowed_dir.clone())));
        tools.register(Arc::new(EditFileTool::new(allowed_dir.clone())));
        tools.register(Arc::new(ListDirTool::new(allowed_dir)));
        tools.register(Arc::new(ExecTool::new(
            workspace.clone(),
            Some(exec_config.timeout),
            Some(exec_config.shell.clone()),
            Some(exec_config.permission_mode.clone()),
            restrict_to_workspace,
        )));
        if brave_api_key.is_some() {
            tools.register(Arc::new(WebSearchTool::new(brave_api_key.clone())));
        } else {
            info!("web_search tool disabled (no Brave API key configured)");
        }
        tools.register(Arc::new(WebFetchTool::new()));
        tools.register(Arc::new(BrowserTool::new(
            workspace.clone(),
            restrict_to_workspace,
        )));

        let message_tool = Arc::new(MessageTool::new(None));
        tools.register(message_tool.clone());

        // Subagent manager + spawn tool
        let subagent_manager = Arc::new(SubagentManager::new(
            provider.clone(),
            workspace.clone(),
            bus.clone(),
            model.clone(),
            brave_api_key,
            exec_config,
            restrict_to_workspace,
            request_config.clone(),
        ));

        let spawn_tool = Arc::new(SpawnTool::new(subagent_manager.clone()));
        tools.register(spawn_tool.clone());

        info!(
            model = %model,
            tools = tools.len(),
            max_iterations = max_iterations,
            "agent loop initialized"
        );

        Self {
            bus,
            provider,
            _workspace: workspace,
            model,
            max_iterations,
            request_config,
            tools,
            context,
            sessions,
            message_tool,
            spawn_tool,
            subagent_manager,
            outbound,
        }
    }

    fn log_llm_thinking_json(
        &self,
        channel: &str,
        chat_id: &str,
        session_key: &str,
        iteration: usize,
        response: &LlmResponse,
    ) {
        if !self.outbound.log_thinking_json {
            return;
        }
        let Some(ref rc) = response.reasoning_content else {
            return;
        };
        let t = rc.trim();
        if t.is_empty() {
            return;
        }
        let line = json!({
            "channel": channel,
            "chatId": chat_id,
            "session": session_key,
            "iteration": iteration,
            "reasoning": t,
        });
        tracing::debug!(target: THINKING_LOG_TARGET, "{}", line);
    }

    /// Body as sent on the wire to `channel` (may compact exec output and strip fenced code).
    fn outbound_text_for_channel(&self, channel: &str, stored: &str) -> String {
        if !is_chat_app_channel(channel) {
            return stored.to_string();
        }
        let mut text = stored.to_string();
        if !self.outbound.include_exec_output_in_chat_apps {
            text = compact_exec_output_for_chat(&text);
        }
        if !self.outbound.include_fenced_code_in_chat_apps {
            text = strip_markdown_fenced_code_blocks(&text);
        }
        text
    }

    fn outbound_message(&self, channel: &str, chat_id: &str, stored: &str) -> OutboundMessage {
        OutboundMessage::new(channel, chat_id, &self.outbound_text_for_channel(channel, stored))
    }

    /// Run the event loop: poll inbound messages and process them.
    ///
    /// This runs indefinitely until the inbound channel is closed.
    pub async fn run(&self) {
        info!("agent loop started, waiting for messages");
        loop {
            match self.bus.consume_inbound().await {
                Some(msg) => {
                    let session_key = msg.session_key();
                    debug!(session_key = %session_key, "received message");

                    // Route system messages (from subagents) vs regular messages
                    let result = if msg.channel == "system" && msg.sender_id == "subagent" {
                        self.process_system_message(&msg).await
                    } else {
                        self.process_message(&msg).await
                    };

                    match result {
                        Ok(response) => {
                            if let Err(e) = self.bus.publish_outbound(response).await {
                                error!(error = %e, "failed to publish outbound message");
                            }
                        }
                        Err(e) => {
                            error!(error = %e, session_key = %session_key, "message processing error");
                            let err_msg = self.outbound_message(
                                &msg.channel,
                                &msg.chat_id,
                                &format!("I encountered an error: {e}"),
                            );
                            let _ = self.bus.publish_outbound(err_msg).await;
                        }
                    }
                }
                None => {
                    info!("inbound channel closed, agent loop exiting");
                    break;
                }
            }
        }
    }

    /// Process a single inbound message → outbound response.
    ///
    /// This is the core agent logic:
    /// 1. Get/create session, load history
    /// 2. Build context messages
    /// 3. LLM ↔ tool loop
    /// 4. Save session, return response
    pub async fn process_message(&self, msg: &InboundMessage) -> Result<OutboundMessage> {
        let session_key = msg.session_key();

        // Set message tool context for this conversation
        self.message_tool
            .set_context(&msg.channel, &msg.chat_id)
            .await;

        // Set spawn tool context for this conversation
        self.spawn_tool
            .set_context(&msg.channel, &msg.chat_id)
            .await;

        // Fast path for explicit requests to output the long-term memory file.
        if is_memory_file_request(&msg.content) {
            let mut params: HashMap<String, serde_json::Value> = HashMap::new();
            let memory_path = self._workspace.join("memory").join("MEMORY.md");
            params.insert(
                "path".to_string(),
                serde_json::Value::String(memory_path.to_string_lossy().to_string()),
            );
            let content = self.tools.execute("read_file", params).await;
            self.sessions
                .add_message(&session_key, Message::user(&msg.content));
            self.sessions
                .add_message(&session_key, Message::assistant(&content));
            return Ok(self.outbound_message(&msg.channel, &msg.chat_id, &content));
        }

        // Fast path for natural-language read-file requests with explicit path.
        if is_read_file_request(&msg.content) {
            if let Some(path) = extract_probable_file_path(&msg.content) {
                let mut params: HashMap<String, serde_json::Value> = HashMap::new();
                params.insert("path".to_string(), serde_json::Value::String(path));
                let content = self.tools.execute("read_file", params).await;
                self.sessions
                    .add_message(&session_key, Message::user(&msg.content));
                self.sessions
                    .add_message(&session_key, Message::assistant(&content));
                return Ok(self.outbound_message(&msg.channel, &msg.chat_id, &content));
            }
        }

        // Fast path for direct shell commands.
        // This ensures command-style messages produce deterministic exec output
        // (including EXIT_CODE), instead of relying on LLM tool selection.
        if looks_like_direct_shell_command(&msg.content) {
            let mut params: HashMap<String, serde_json::Value> = HashMap::new();
            params.insert(
                "command".to_string(),
                serde_json::Value::String(msg.content.trim().to_string()),
            );
            let content = self.tools.execute("exec", params).await;
            self.sessions
                .add_message(&session_key, Message::user(&msg.content));
            self.sessions
                .add_message(&session_key, Message::assistant(&content));
            return Ok(self.outbound_message(&msg.channel, &msg.chat_id, &content));
        }

        // Script-file mode for explicit "run this PowerShell script" requests.
        // We write one .ps1 and execute exactly one command (`powershell -File ...`).
        if should_use_script_file_mode(&msg.content) {
            if let Some(script_body) = extract_powershell_code_block(&msg.content) {
                let script_path = self._workspace.join("agent_script_run.ps1");
                if let Err(e) = std::fs::write(&script_path, script_body.as_bytes()) {
                    let err = format!("Failed to write script file '{}': {e}", script_path.display());
                    self.sessions
                        .add_message(&session_key, Message::user(&msg.content));
                    self.sessions
                        .add_message(&session_key, Message::assistant(&err));
                    return Ok(self.outbound_message(&msg.channel, &msg.chat_id, &err));
                }
                let exec_cmd = format!(
                    "powershell -NoProfile -ExecutionPolicy Bypass -File \"{}\"",
                    script_path.display()
                );
                let mut params: HashMap<String, serde_json::Value> = HashMap::new();
                params.insert("command".to_string(), serde_json::Value::String(exec_cmd));
                let content = self.tools.execute("exec", params).await;
                self.sessions
                    .add_message(&session_key, Message::user(&msg.content));
                self.sessions
                    .add_message(&session_key, Message::assistant(&content));
                return Ok(self.outbound_message(&msg.channel, &msg.chat_id, &content));
            }
        }

        // Deterministic Windows fast path: install whisper.cpp + model via one script execution.
        // This avoids narration-only "installed ✅" replies for this common workflow.
        if cfg!(target_os = "windows") && is_whisper_cpp_install_request(&msg.content) {
            let script = r#"$ErrorActionPreference = "Stop"
$root = "C:\whisper-cpp"
$bin = Join-Path $root "bin"
$mdl = Join-Path $root "models"
New-Item -ItemType Directory -Force -Path $root | Out-Null
New-Item -ItemType Directory -Force -Path $bin | Out-Null
New-Item -ItemType Directory -Force -Path $mdl | Out-Null

$zipPath = Join-Path $env:TEMP "whisper.zip"
$releaseUrls = @(
  "https://github.com/ggerganov/whisper.cpp/releases/download/v1.7.1/whisper-bin-win64-x64.zip",
  "https://github.com/ggerganov/whisper.cpp/releases/latest/download/whisper-bin-x64.zip"
)
$downloaded = $false
foreach ($u in $releaseUrls) {
  try {
    Invoke-WebRequest -Uri $u -OutFile $zipPath
    $downloaded = $true
    break
  } catch {
    # try next release URL
  }
}
if (-not $downloaded) { throw "Failed to download whisper.cpp release zip from known URLs." }
Expand-Archive -Path $zipPath -DestinationPath $root -Force

# Common executable names by build/release package.
$candidateExe = @(
  (Join-Path $root "whisper-cli.exe"),
  (Join-Path $root "main.exe"),
  (Join-Path $root "bin\whisper-cli.exe"),
  (Join-Path $root "bin\main.exe")
)
$exePath = $candidateExe | Where-Object { Test-Path -LiteralPath $_ } | Select-Object -First 1
if (-not $exePath) { throw "Could not find whisper executable after extraction." }

$modelPath = Join-Path $mdl "ggml-base.bin"
Invoke-WebRequest -Uri "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-base.bin" -OutFile $modelPath
if (!(Test-Path -LiteralPath $modelPath)) { throw "Model download failed: $modelPath" }

$cfgPath = "$HOME\.metis\config.json"
if (!(Test-Path -LiteralPath $cfgPath)) { throw "Missing Metis config: $cfgPath" }
$cfg = Get-Content $cfgPath -Raw | ConvertFrom-Json
if (-not $cfg.transcription) { $cfg | Add-Member -NotePropertyName transcription -NotePropertyValue (@{}) }
$cfg.transcription.enabled = $true
$cfg.transcription.provider = "whisper_cpp"
if (-not $cfg.transcription.whisperCpp) { $cfg.transcription | Add-Member -NotePropertyName whisperCpp -NotePropertyValue (@{}) }
$cfg.transcription.whisperCpp.exePath = $exePath
$cfg.transcription.whisperCpp.modelPath = $modelPath
if (-not $cfg.transcription.whisperCpp.extraArgs) { $cfg.transcription.whisperCpp.extraArgs = @() }
$cfg | ConvertTo-Json -Depth 20 | Set-Content $cfgPath -Encoding UTF8

Write-Host "WHISPER_CPP_EXE=$exePath"
Write-Host "WHISPER_CPP_MODEL=$modelPath"
Write-Host "CONFIG_UPDATED=$cfgPath"
"#;
            let script_path = self._workspace.join("install_whisper_cpp.ps1");
            if let Err(e) = std::fs::write(&script_path, script.as_bytes()) {
                let err = format!(
                    "Failed to write whisper.cpp installer script '{}': {e}",
                    script_path.display()
                );
                self.sessions
                    .add_message(&session_key, Message::user(&msg.content));
                self.sessions
                    .add_message(&session_key, Message::assistant(&err));
                return Ok(self.outbound_message(&msg.channel, &msg.chat_id, &err));
            }

            let exec_cmd = format!(
                "powershell -NoProfile -ExecutionPolicy Bypass -File \"{}\"",
                script_path.display()
            );
            let mut params: HashMap<String, serde_json::Value> = HashMap::new();
            params.insert("command".to_string(), serde_json::Value::String(exec_cmd));
            let content = self.tools.execute("exec", params).await;
            self.sessions
                .add_message(&session_key, Message::user(&msg.content));
            self.sessions
                .add_message(&session_key, Message::assistant(&content));
            return Ok(self.outbound_message(&msg.channel, &msg.chat_id, &content));
        }

        // Get session history
        let history = self.sessions.get_history(&session_key, 50);

        // Build LLM messages
        let media_paths: Vec<String> = msg.media.iter().map(|m| m.path.clone()).collect();
        let mut messages = self.context.build_messages(
            &history,
            &msg.content,
            &media_paths,
            &msg.channel,
            &msg.chat_id,
        );

        // Get tool definitions
        let tool_defs = self.tools.get_definitions();

        // Agent loop: LLM ↔ tool calling
        let mut final_content: Option<String> = None;
        let mut exec_tool_outputs: Vec<String> = Vec::new();
        let strict_exec_mode = is_execution_or_install_request(&msg.content);
        let mut exec_calls_executed: usize = 0;

        for iteration in 0..self.max_iterations {
            debug!(iteration = iteration, "LLM call");

            let response = self
                .provider
                .chat(
                    &messages,
                    Some(&tool_defs),
                    &self.model,
                    &self.request_config,
                )
                .await;

            self.log_llm_thinking_json(&msg.channel, &msg.chat_id, &session_key, iteration, &response);

            if response.has_tool_calls() {
                // Add assistant message with tool calls
                let tool_calls: Vec<ToolCall> = response.tool_calls.clone();
                ContextBuilder::add_assistant_message(
                    &mut messages,
                    response.content.clone(),
                    tool_calls.clone(),
                );

                // Execute each tool call
                for tc in &tool_calls {
                    if strict_exec_mode && tc.function.name == "exec" && exec_calls_executed >= 1 {
                        let result = "Strict exec mode: only one command is executed per request. \
Please send a follow-up message to run the next command."
                            .to_string();
                        ContextBuilder::add_tool_result(&mut messages, &tc.id, &result);
                        continue;
                    }
                    let params: HashMap<String, serde_json::Value> =
                        serde_json::from_str(&tc.function.arguments).unwrap_or_default();

                    info!(
                        tool = %tc.function.name,
                        iteration = iteration,
                        "executing tool call"
                    );

                    let result = self.tools.execute(&tc.function.name, params).await;
                    if tc.function.name == "exec" {
                        exec_calls_executed += 1;
                        exec_tool_outputs.push(result.clone());
                    }

                    debug!(
                        tool = %tc.function.name,
                        result_len = result.len(),
                        "tool result"
                    );

                    ContextBuilder::add_tool_result(&mut messages, &tc.id, &result);
                }
            } else {
                // No tool calls → final answer
                final_content = response.content;
                break;
            }
        }

        // If we exhausted iterations without a final answer
        let compact_exec = is_chat_app_channel(&msg.channel);
        let mut content = reconcile_exec_with_reply(
            final_content
                .unwrap_or_else(|| "I've completed processing but have no response to give.".into()),
            &exec_tool_outputs,
            compact_exec,
        );

        // Recovery guard: if the model narrated a file-read command (e.g. "Verifying: Get-Content ...")
        // but never executed a tool call, force a deterministic read_file response.
        if exec_tool_outputs.is_empty() && looks_like_unexecuted_read_narration(&content) {
            if let Some(path) = extract_probable_file_path(&content) {
                let mut params: HashMap<String, serde_json::Value> = HashMap::new();
                params.insert("path".to_string(), serde_json::Value::String(path));
                let recovered = self.tools.execute("read_file", params).await;
                content = format!("{content}\n\n---\n\n{recovered}");
            }
        }
        if exec_tool_outputs.is_empty() && looks_like_unexecuted_exec_narration(&content) {
            if let Some(command) = extract_probable_command_from_narration(&content) {
                let mut params: HashMap<String, serde_json::Value> = HashMap::new();
                params.insert("command".to_string(), serde_json::Value::String(command));
                let recovered = self.tools.execute("exec", params).await;
                content = format!("{content}\n\n---\n\n{recovered}");
            }
        }
        // User challenged a false "running" claim — prefer real exec failure output over guardrail noise.
        if is_user_challenging_agent_claims(&msg.content) {
            if exec_tool_outputs.iter().any(|o| exec_report_failed(o)) {
                let block = if compact_exec {
                    exec_tool_outputs
                        .iter()
                        .map(|o| summarize_exec_block(o))
                        .collect::<Vec<_>>()
                        .join("\n")
                } else {
                    exec_tool_outputs.join("\n\n")
                };
                content = format!(
                    "You're right — the check failed; I should not claim the server is running.\n\n{block}"
                );
            }
        }

        // Guardrail: if the model pasted an `EXEC_RESULT` block without any real exec tool call,
        // treat it as untrusted and force deterministic path verification.
        let user_requested_execution = is_execution_or_install_request(&msg.content);
        let should_apply_exec_guardrail = !is_user_challenging_agent_claims(&msg.content)
            && user_requested_execution
            && (has_untrusted_exec_block(&content, &exec_tool_outputs)
                || (exec_tool_outputs.is_empty() && looks_like_unverified_success_claim(&content)));
        if should_apply_exec_guardrail {
            let mut hardened = "⚠ I cannot confirm this operation.\nNo real `exec` tool result was produced in this turn, so install/success claims are considered unverified.".to_string();
            let paths = extract_probable_paths(&content);
            if let Some(cmd) = build_test_path_command(&paths) {
                let mut params: HashMap<String, serde_json::Value> = HashMap::new();
                params.insert("command".to_string(), serde_json::Value::String(cmd));
                let recovered = self.tools.execute("exec", params).await;
                let all_exist = !recovered.contains("EXISTS=False");
                if all_exist && !paths.is_empty() {
                    hardened = "✅ Verification passed via actual path checks.\nThe operation appears completed on disk, even though this turn had no direct exec run.".to_string();
                }
                hardened.push_str("\n\nPath verification (actual tool output):\n\n");
                hardened.push_str(&recovered);
            }
            if paths.is_empty() {
                hardened.push_str("\n\nNo reliable paths were found to verify from the response text.");
            }
            content = hardened;
        }

        // Save conversation to session
        self.sessions
            .add_message(&session_key, Message::user(&msg.content));
        self.sessions
            .add_message(&session_key, Message::assistant(&content));

        Ok(self.outbound_message(&msg.channel, &msg.chat_id, &content))
    }

    /// Process a system message (from a subagent or cron).
    ///
    /// Parses the original `channel:chat_id` from `msg.chat_id`,
    /// loads the original session, runs a full LLM call to summarize
    /// the result, and routes the response back to the correct channel.
    async fn process_system_message(&self, msg: &InboundMessage) -> Result<OutboundMessage> {
        info!(
            sender = %msg.sender_id,
            chat_id = %msg.chat_id,
            "processing system message"
        );

        // Parse origin from chat_id format "channel:chat_id"
        let (origin_channel, origin_chat_id) = match msg.chat_id.split_once(':') {
            Some((ch, cid)) => (ch.to_string(), cid.to_string()),
            None => {
                return Err(anyhow::anyhow!(
                    "Invalid system message chat_id format: {}",
                    msg.chat_id
                ));
            }
        };

        let session_key = format!("{origin_channel}:{origin_chat_id}");

        // Set tools context to the original channel/chat
        self.message_tool
            .set_context(&origin_channel, &origin_chat_id)
            .await;
        self.spawn_tool
            .set_context(&origin_channel, &origin_chat_id)
            .await;

        // Load the original session
        let history = self.sessions.get_history(&session_key, 50);

        // Build messages with the subagent result as the "user" message
        let mut messages =
            self.context
                .build_messages(&history, &msg.content, &[], &origin_channel, &origin_chat_id);

        let tool_defs = self.tools.get_definitions();
        let mut final_content: Option<String> = None;
        let mut exec_tool_outputs: Vec<String> = Vec::new();

        for iteration in 0..self.max_iterations {
            debug!(iteration = iteration, "system message LLM call");

            let response = self
                .provider
                .chat(&messages, Some(&tool_defs), &self.model, &self.request_config)
                .await;

            self.log_llm_thinking_json(
                &origin_channel,
                &origin_chat_id,
                &session_key,
                iteration,
                &response,
            );

            if response.has_tool_calls() {
                let tool_calls: Vec<ToolCall> = response.tool_calls.clone();
                ContextBuilder::add_assistant_message(
                    &mut messages,
                    response.content.clone(),
                    tool_calls.clone(),
                );

                for tc in &tool_calls {
                    let params: HashMap<String, serde_json::Value> =
                        serde_json::from_str(&tc.function.arguments).unwrap_or_default();
                    let result = self.tools.execute(&tc.function.name, params).await;
                    if tc.function.name == "exec" {
                        exec_tool_outputs.push(result.clone());
                    }
                    ContextBuilder::add_tool_result(&mut messages, &tc.id, &result);
                }
            } else {
                final_content = response.content;
                break;
            }
        }

        let compact_exec = is_chat_app_channel(&origin_channel);
        let content = reconcile_exec_with_reply(
            final_content
                .unwrap_or_else(|| "I've completed processing but have no response to give.".into()),
            &exec_tool_outputs,
            compact_exec,
        );

        // Save to the original session
        self.sessions
            .add_message(&session_key, Message::user(&msg.content));
        self.sessions
            .add_message(&session_key, Message::assistant(&content));

        // Route response to the original channel/chat
        Ok(self.outbound_message(&origin_channel, &origin_chat_id, &content))
    }

    /// Direct processing mode (CLI entry point).
    ///
    /// Wraps text into an `InboundMessage` on the "cli" channel and processes.
    pub async fn process_direct(&self, text: &str) -> Result<String> {
        let msg = InboundMessage::new("cli", "user", "direct", text);
        let response = self.process_message(&msg).await?;
        Ok(response.content)
    }

    /// Like [`process_direct`](Self::process_direct), but uses an explicit `(channel, chat_id)` pair
    /// so session history stays isolated — required for cron jobs (`channel = "cron"`,
    /// `chat_id = job id` → session key `cron:<id>`).
    ///
    /// Sender is `scheduler` so transcripts do not resemble a user's chat handle.
    pub async fn process_direct_session(
        &self,
        channel: &str,
        chat_id: &str,
        text: &str,
    ) -> Result<String> {
        let msg = InboundMessage::new(channel, "scheduler", chat_id, text);
        let response = self.process_message(&msg).await?;
        Ok(response.content)
    }

    /// Get a reference to the tool registry (for testing/extension).
    pub fn tools(&self) -> &ToolRegistry {
        &self.tools
    }

    /// Get the model name.
    pub fn model(&self) -> &str {
        &self.model
    }
}

// ─────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use metis_core::types::{LlmResponse, ToolDefinition};

    /// A mock LLM provider that returns canned responses.
    struct MockProvider {
        /// Responses to return in sequence.
        responses: std::sync::Mutex<Vec<LlmResponse>>,
    }

    impl MockProvider {
        fn new(responses: Vec<LlmResponse>) -> Self {
            Self {
                responses: std::sync::Mutex::new(responses),
            }
        }

        fn simple(text: &str) -> Self {
            Self::new(vec![LlmResponse {
                content: Some(text.into()),
                ..Default::default()
            }])
        }
    }

    #[async_trait]
    impl LlmProvider for MockProvider {
        async fn chat(
            &self,
            _messages: &[Message],
            _tools: Option<&[ToolDefinition]>,
            _model: &str,
            _config: &LlmRequestConfig,
        ) -> LlmResponse {
            let mut responses = self.responses.lock().unwrap();
            if responses.is_empty() {
                LlmResponse {
                    content: Some("(no more responses)".into()),
                    ..Default::default()
                }
            } else {
                responses.remove(0)
            }
        }

        fn default_model(&self) -> &str {
            "mock-model"
        }

        fn display_name(&self) -> &str {
            "MockProvider"
        }
    }

    fn create_test_loop(provider: Arc<dyn LlmProvider>) -> AgentLoop {
        let bus = Arc::new(MessageBus::new(32));
        let workspace = std::env::temp_dir().join("METIS_test_agent");
        let _ = std::fs::create_dir_all(&workspace);

        AgentLoop::new(
            bus,
            provider,
            workspace,
            None,
            Some(5),
            None,
            None,
            None,
            false,
            None,
            None,
            None,
        )
    }

    #[tokio::test]
    async fn test_agent_simple_response() {
        let provider = Arc::new(MockProvider::simple("Hello from Metis!"));
        let agent = create_test_loop(provider);

        let result = agent.process_direct("Hi").await.unwrap();
        assert_eq!(result, "Hello from Metis!");
    }

    #[test]
    fn test_strip_markdown_fenced_code_blocks() {
        let s = "Hello\n```rust\nfn main() {}\n```\nBye";
        let out = super::strip_markdown_fenced_code_blocks(s);
        assert!(out.contains("Hello"));
        assert!(out.contains("Bye"));
        assert!(out.contains("code omitted"));
        assert!(!out.contains("fn main"));
    }

    #[test]
    fn test_strip_fenced_only_placeholder_line() {
        let out = super::strip_markdown_fenced_code_blocks("```\nx\n```");
        assert!(out.contains("code omitted"));
        assert!(!out.contains('x'));
    }

    #[test]
    fn test_looks_like_direct_shell_command() {
        assert!(looks_like_direct_shell_command("type C:\\foo\\bar.txt"));
        assert!(looks_like_direct_shell_command("dir"));
        assert!(looks_like_direct_shell_command("git status"));
        assert!(!looks_like_direct_shell_command("Can you run type C:\\foo\\bar.txt"));
        assert!(!looks_like_direct_shell_command("please run git status"));
        assert!(!looks_like_direct_shell_command("what is git status"));
        assert!(!looks_like_direct_shell_command(""));
    }

    #[test]
    fn test_is_memory_file_request() {
        assert!(is_memory_file_request("can you output the memory file ?"));
        assert!(is_memory_file_request("show MEMORY.md"));
        assert!(is_memory_file_request("read memory.md file"));
        assert!(!is_memory_file_request("what is memory?"));
        assert!(!is_memory_file_request("show the daily note"));
    }

    #[test]
    fn test_read_file_request_with_path_extraction() {
        assert!(is_read_file_request("can you read this file C:\\tmp\\x.md"));
        assert!(!is_read_file_request("save this file C:\\tmp\\x.md"));
        assert_eq!(
            extract_probable_file_path("read \"C:\\Users\\chack\\.metis\\workspace\\memory\\MEMORY.md\""),
            Some("C:\\Users\\chack\\.metis\\workspace\\memory\\MEMORY.md".to_string())
        );
        assert_eq!(
            extract_probable_file_path("show /home/user/notes/todo.txt please"),
            Some("/home/user/notes/todo.txt".to_string())
        );
    }

    #[test]
    fn test_looks_like_unexecuted_read_narration() {
        let sample = r#"Done! ✅ Saved!

Verifying:
Get-Content "C:\Users\chack\.metis\workspace\memory\MEMORY.md"

Reading back now! 📩"#;
        assert!(looks_like_unexecuted_read_narration(sample));
        assert!(!looks_like_unexecuted_read_narration("All done, no verification command."));
    }

    #[test]
    fn test_exec_narration_recovery_helpers() {
        let sample = r#"
Running:

C:\codding\Metis-main\target\release\metis.exe cron list

Reading now!
"#;
        assert!(looks_like_unexecuted_exec_narration(sample));
        assert_eq!(
            extract_probable_command_from_narration(sample),
            Some("C:\\codding\\Metis-main\\target\\release\\metis.exe cron list".to_string())
        );
        assert!(!looks_like_unexecuted_exec_narration("No command here."));
    }

    #[test]
    fn test_exec_narration_recovery_helpers_powershell_find() {
        let sample = r#"
Running:
PowerShell
Get-ChildItem -Path "C:\Users\chack\.metis" -Recurse -Filter "*.toml" | Select-Object FullName
Finding config now! 📩
"#;
        assert!(looks_like_unexecuted_exec_narration(sample));
        assert_eq!(
            extract_probable_command_from_narration(sample),
            Some(
                r#"Get-ChildItem -Path "C:\Users\chack\.metis" -Recurse -Filter "*.toml" | Select-Object FullName"#
                    .to_string()
            )
        );
    }

    #[test]
    fn test_untrusted_exec_block_detection() {
        let fake = "Done\n<<<EXEC_RESULT>>>\nSTATUS: SUCCESS\n<<<END_EXEC_RESULT>>>";
        assert!(has_untrusted_exec_block(fake, &[]));
        let real = vec!["<<<EXEC_RESULT>>>\nEXIT_CODE: 0\n<<<END_EXEC_RESULT>>>".to_string()];
        assert!(!has_untrusted_exec_block(fake, &real));
    }

    #[test]
    fn test_extract_probable_paths_and_build_check_command() {
        let text = r#"installed:
C:\whisper-cpp\bin\main.exe
C:\whisper-cpp\models\ggml-base.bin"#;
        let paths = extract_probable_paths(text);
        assert_eq!(paths.len(), 2);
        let cmd = build_test_path_command(&paths).unwrap_or_default();
        assert!(cmd.contains("Test-Path"));
        assert!(cmd.contains("C:\\whisper-cpp\\bin\\main.exe"));
    }

    #[test]
    fn test_extract_probable_paths_skips_malformed_tokens() {
        let text = r#"C:\whisper-cpp";
C:\valid\file.txt
|C:\bad\path"#;
        let paths = extract_probable_paths(text);
        assert_eq!(paths, vec![r#"C:\valid\file.txt"#.to_string()]);
    }

    #[test]
    fn test_extract_probable_paths_skips_too_short_windows_paths() {
        let text = r#"C:\wh C:\ok\real\path.txt"#;
        let paths = extract_probable_paths(text);
        assert_eq!(paths, vec![r#"C:\ok\real\path.txt"#.to_string()]);
    }

    #[test]
    fn test_execution_request_detection() {
        assert!(is_execution_or_install_request("run this script"));
        assert!(is_execution_or_install_request("install whisper cpp"));
        assert!(is_execution_or_install_request("powershell Get-ChildItem"));
        assert!(!is_execution_or_install_request("hello i am here"));
        assert!(!is_execution_or_install_request("how are you"));
    }

    #[test]
    fn test_script_file_mode_detection_and_extract() {
        let msg = r#"run this script please
```powershell
Write-Output "hello"
```
"#;
        assert!(should_use_script_file_mode(msg));
        let body = extract_powershell_code_block(msg).unwrap_or_default();
        assert!(body.contains("Write-Output"));
    }

    #[test]
    fn test_whisper_cpp_install_request_detection() {
        assert!(is_whisper_cpp_install_request("please install whisper.cpp"));
        assert!(is_whisper_cpp_install_request("set up whisper cpp on windows"));
        assert!(!is_whisper_cpp_install_request("hello whisper"));
        assert!(!is_whisper_cpp_install_request("install telegram"));
    }

    #[test]
    fn test_guardrail_all_exist_detection_signal() {
        let recovered = "PATH=C:\\a EXISTS=True\nPATH=C:\\b EXISTS=True\n";
        assert!(!recovered.contains("EXISTS=False"));
        let recovered2 = "PATH=C:\\a EXISTS=True\nPATH=C:\\b EXISTS=False\n";
        assert!(recovered2.contains("EXISTS=False"));
    }

    #[tokio::test]
    async fn test_agent_tool_calling() {
        // First response: LLM requests read_file tool call
        // Second response: LLM gives final answer
        let dir = tempfile::tempdir().unwrap();
        let test_file = dir.path().join("test.txt");
        std::fs::write(&test_file, "file content here").unwrap();

        let tool_call = ToolCall::new(
            "call_1",
            "read_file",
            serde_json::json!({"path": test_file.to_str().unwrap()}).to_string(),
        );

        let responses = vec![
            LlmResponse {
                content: None,
                tool_calls: vec![tool_call],
                ..Default::default()
            },
            LlmResponse {
                content: Some("The file contains: file content here".into()),
                ..Default::default()
            },
        ];

        let provider = Arc::new(MockProvider::new(responses));
        let bus = Arc::new(MessageBus::new(32));

        let agent = AgentLoop::new(
            bus,
            provider,
            dir.path().to_path_buf(),
            None,
            Some(10),
            None,
            None,
            None,
            false,
            None,
            None,
            None,
        );

        let result = agent.process_direct("Read test.txt").await.unwrap();
        assert_eq!(result, "The file contains: file content here");
    }

    #[tokio::test]
    async fn test_agent_max_iterations() {
        // All responses are tool calls → should exhaust max_iterations
        let tool_call = ToolCall::new("call_loop", "list_dir", r#"{"path": "/tmp"}"#);
        let responses: Vec<LlmResponse> = (0..10)
            .map(|_| LlmResponse {
                content: None,
                tool_calls: vec![tool_call.clone()],
                ..Default::default()
            })
            .collect();

        let provider = Arc::new(MockProvider::new(responses));
        let agent = create_test_loop(provider);

        let result = agent.process_direct("loop forever").await.unwrap();
        assert!(result.contains("completed processing"));
    }

    #[test]
    fn reconcile_exec_strips_hallucinated_blocks_when_real_exec_ran() {
        let fake = "Running! ✅\n\n<<<EXEC_RESULT>>>\nEXIT_CODE: 0\nSTATUS: SUCCESS\n<<<END_EXEC_RESULT>>>";
        let real = vec![
            "<<<EXEC_RESULT>>>\nCOMMAND: Invoke-WebRequest http://localhost:5000\nEXIT_CODE: 1\nSTATUS: FAILED\n<<<END_EXEC_RESULT>>>\n--- STDERR ---\nUnable to connect to the remote server\n".to_string(),
        ];
        let merged = super::reconcile_exec_with_reply(fake.to_string(), &real, true);
        assert!(!merged.contains("Running!"));
        assert!(!merged.contains("EXIT_CODE: 0"));
        assert!(merged.contains("not succeed") || merged.contains("✗"));
        assert!(merged.contains("Unable to connect"));
    }

    #[test]
    fn reconcile_exec_appends_when_model_omits_proof_block() {
        let raw = vec!["<<<EXEC_RESULT>>>\nEXIT_CODE: 0\nSTATUS: SUCCESS\n<<<END_EXEC_RESULT>>>".to_string()];
        let merged = super::reconcile_exec_with_reply("Executing now! 📩".to_string(), &raw, false);
        assert!(merged.contains("Executing now"));
        assert!(merged.contains("<<<EXEC_RESULT>>>"));
    }

    #[test]
    fn reconcile_exec_compact_for_chat_apps() {
        let raw = vec![
            "<<<EXEC_RESULT>>>\nCOMMAND: [System.IO.File]::WriteAllText(\"C:\\\\big.html\", \"<!DOCTYPE html>...\")\nEXIT_CODE: 0\nSTATUS: SUCCESS\n<<<END_EXEC_RESULT>>>".to_string(),
        ];
        let merged = super::reconcile_exec_with_reply("Done!".to_string(), &raw, true);
        assert!(merged.contains("Done!"));
        assert!(merged.contains("✓ Ran"));
        assert!(merged.len() < 300, "compact merge should stay short, got {} chars", merged.len());
    }

    #[test]
    fn test_exec_report_failed_detects_exit_code_one() {
        let block = "<<<EXEC_RESULT>>>\nEXIT_CODE: 1\nSTATUS: FAILED\n<<<END_EXEC_RESULT>>>";
        assert!(super::exec_report_failed(block));
    }

    #[test]
    fn test_compact_exec_output_strips_huge_command() {
        let huge = format!(
            "<<<EXEC_RESULT>>>\nCOMMAND: {}\nEXIT_CODE: 0\nSTATUS: SUCCESS\n<<<END_EXEC_RESULT>>>\n--- STDOUT ---\nmore noise",
            "x".repeat(5000)
        );
        let out = super::compact_exec_output_for_chat(&huge);
        assert!(out.contains("✓ Ran"));
        assert!(!out.contains(&"x".repeat(200)));
        assert!(!out.contains("STDOUT"));
    }

    #[test]
    fn test_default_tools_registered() {
        let provider = Arc::new(MockProvider::simple("ok"));
        let agent = create_test_loop(provider);

        let names = agent.tools().tool_names();
        assert!(names.contains(&"read_file".into()));
        assert!(names.contains(&"write_file".into()));
        assert!(names.contains(&"edit_file".into()));
        assert!(names.contains(&"list_dir".into()));
        assert!(names.contains(&"exec".into()));
        assert!(!names.contains(&"web_search".into()));
        assert!(names.contains(&"web_fetch".into()));
        assert!(names.contains(&"browser".into()));
        assert!(names.contains(&"message".into()));
        assert!(names.contains(&"spawn".into()));
        assert_eq!(names.len(), 9);
    }

    #[test]
    fn test_model_defaults_to_provider() {
        let provider = Arc::new(MockProvider::simple("ok"));
        let agent = create_test_loop(provider);
        assert_eq!(agent.model(), "mock-model");
    }

    #[test]
    fn test_exec_tool_config_default() {
        let config = ExecToolConfig::default();
        assert_eq!(config.timeout, 60);
        assert_eq!(config.permission_mode, "unsafe_only");
    }

    #[tokio::test]
    async fn test_process_system_message() {
        let provider = Arc::new(MockProvider::simple("Here's a summary of the result."));
        let bus = Arc::new(MessageBus::new(32));
        let workspace = std::env::temp_dir().join("METIS_test_system_msg");
        let _ = std::fs::create_dir_all(&workspace);

        let agent = AgentLoop::new(
            bus,
            provider,
            workspace,
            None,
            Some(5),
            None,
            None,
            None,
            false,
            None,
            None,
            None,
        );

        // Simulate a subagent result message
        let msg = InboundMessage::new(
            "system",
            "subagent",
            "telegram:chat_42",
            "## Subagent Result\n**Task**: test\n\nDone!",
        );

        let response = agent.process_system_message(&msg).await.unwrap();

        // Response should be routed to the original channel/chat
        assert_eq!(response.channel, "telegram");
        assert_eq!(response.chat_id, "chat_42");
        assert_eq!(response.content, "Here's a summary of the result.");
    }

    #[tokio::test]
    async fn test_process_system_message_invalid_format() {
        let provider = Arc::new(MockProvider::simple("ok"));
        let agent = create_test_loop(provider);

        // Missing colon separator
        let msg = InboundMessage::new("system", "subagent", "invalid_chat_id", "test");

        let result = agent.process_system_message(&msg).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_run_routes_system_messages() {
        // Verify that the run loop correctly routes system messages
        let provider = Arc::new(MockProvider::simple("Summary of result"));
        let bus = Arc::new(MessageBus::new(32));
        let workspace = std::env::temp_dir().join("METIS_test_run_route");
        let _ = std::fs::create_dir_all(&workspace);

        let agent = AgentLoop::new(
            bus.clone(),
            provider,
            workspace,
            None,
            Some(5),
            None,
            None,
            None,
            false,
            None,
            None,
            None,
        );

        // Publish a system message
        let msg = InboundMessage::new(
            "system",
            "subagent",
            "discord:guild_1",
            "Subagent result content",
        );
        bus.publish_inbound(msg).await.unwrap();

        // Drop the inbound sender by dropping our handle — but we need
        // a different approach since MessageBus owns the sender.
        // Instead, just test process_message routing directly.

        // We already test process_system_message above, so just verify
        // the agent has the spawn tool
        assert!(agent.tools().has("spawn"));
    }

    #[tokio::test]
    async fn test_subagent_manager_accessible() {
        let provider = Arc::new(MockProvider::simple("ok"));
        let agent = create_test_loop(provider);

        // Subagent manager should start with 0 tasks
        assert_eq!(agent.subagent_manager.task_count().await, 0);
    }
}
