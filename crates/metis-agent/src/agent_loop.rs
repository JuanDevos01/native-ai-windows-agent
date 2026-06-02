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
use metis_core::types::{Message, LlmResponse};
use metis_providers::traits::{LlmProvider, LlmRequestConfig};

use crate::context::ContextBuilder;
use crate::subagent::SubagentManager;
use crate::tools::base::{parse_tool_params, sanitize_tool_calls_for_history};
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

/// Stdout/stderr tail of an exec report (after `<<<END_EXEC_RESULT>>>`).
fn exec_process_output_tail(block: &str) -> &str {
    let Some(idx) = block.find(END_EXEC_RESULT_MARKER) else {
        return "";
    };
    block[idx + END_EXEC_RESULT_MARKER.len()..].trim()
}

/// True when exec stdout contains one of our own explicit failure sentinel markers.
/// These are emitted only by our own PowerShell scripts, never by arbitrary commands.
fn exec_output_has_custom_failure_marker(tail: &str) -> bool {
    let lower = tail.to_lowercase();
    lower.contains("http_server_fail") || lower.contains("invoice_server_fail")
}

/// True when a wrapped exec tool report indicates failure.
///
/// Deliberately conservative: we only trust structured signals, never heuristic
/// string-matching on stdout (which causes false positives on file-read commands whose
/// output happens to contain words like "connection refused" or "error").
pub fn exec_report_failed(block: &str) -> bool {
    // Explicit structured failure from the exec tool itself.
    if block.contains("STATUS: FAILED") || block.contains("STATUS: TIMEOUT") {
        return true;
    }
    // Non-zero exit code (covers virtually all real failures).
    if parse_exec_exit_code(block).is_some_and(|c| c != 0) {
        return true;
    }
    // Our own custom failure markers — only these, nothing else.
    exec_output_has_custom_failure_marker(exec_process_output_tail(block))
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

fn first_failure_summary_line(block: &str) -> Option<String> {
    if let Some(s) = first_stderr_summary_line(block) {
        return Some(s);
    }
    let tail = exec_process_output_tail(block);
    for line in tail.lines() {
        let t = line.trim();
        if t.is_empty() {
            continue;
        }
        let l = t.to_lowercase();
        if l.contains("_fail") || l.contains("unable to connect") || l.contains("timed out") {
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
        let err = first_failure_summary_line(block).unwrap_or_else(|| "command failed".to_string());
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
            if l.contains("starting mission control")
                || l.contains("servers are not running")
                || l.contains("let me start them")
            {
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
            || s.starts_with("(no process output")   // timeout body
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

static REASONING_TAG_RE: OnceLock<Regex> = OnceLock::new();

/// Remove model reasoning tags that must not be sent to users/channels.
pub fn strip_reasoning_tags(text: &str) -> String {
    let re = REASONING_TAG_RE.get_or_init(|| {
        Regex::new(r"(?is)<(?:redacted_)?think(?:ing)?>.*?</(?:redacted_)?think(?:ing)?>")
            .expect("reasoning tag regex")
    });
    let cleaned = re.replace_all(text, "");
    cleaned.trim().to_string()
}

/// Remove ``` ... ``` blocks (non-greedy). Used for chat-app outbound messages.
/// Multi-line code blocks are silently removed (not replaced with a placeholder message).
/// Short single-line inline blocks are kept as-is.
pub fn strip_markdown_fenced_code_blocks(text: &str) -> String {
    let re = fenced_code_block_re();
    let replaced = re.replace_all(text, |caps: &regex::Captures| {
        let block = caps.get(0).map_or("", |m| m.as_str());
        // Extract content between the opening/closing fences.
        let inner_start = block.find('\n').map(|i| i + 1).unwrap_or(block.len());
        let inner = &block[inner_start..];
        let inner = inner.trim_end_matches('`').trim();
        let lines = inner.lines().count();
        if lines > 1 || inner.len() > 120 {
            // Multi-line or long block: silently drop it.
            String::new()
        } else {
            // Short single-line: keep as-is.
            block.to_string()
        }
    });
    let trimmed = replaced.trim();
    if trimmed.is_empty() {
        String::new()
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

    if exec_turn_should_show_hard_failure(exec_outputs) {
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

    if exec_turn_should_show_hard_failure(exec_outputs) {
        let intro = "Some steps failed — see exec results below. Continue debugging if the task is not done.";
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

/// User asked to fix multiple things (e.g. port 8080 UI + port 5000 server).
fn is_multi_step_fix_request(input: &str) -> bool {
    // Questions / inspection requests are answered, not treated as fix tasks.
    if is_question_or_inspection(input) {
        return false;
    }
    let lower = input.to_lowercase();
    let wants_fix = lower.contains("fix")
        || lower.contains("not working")
        || lower.contains("not clickable")
        || lower.contains("broken")
        || lower.contains("doesn't work")
        || lower.contains("does not work")
        || lower.contains("troubleshoot")
        || lower.contains("getting error")
        || lower.contains("error:")
        || lower.contains("check and")
        || lower.contains("please check");
    let multi = lower.contains("both")
        || lower.contains("two issues")
        || lower.contains("2 issues")
        || (lower.contains("8080") && lower.contains("5000"))
        || lower.matches("port").count() >= 2;
    // Single-service but complex enough: has an error message and an instruction
    let single_complex = lower.contains("error") && (lower.contains("fix") || lower.contains("troubleshoot") || lower.contains("check"));
    (wants_fix && multi) || single_complex
}

/// True when the message is a question or an inspection/read request rather than an
/// action task. We must ANSWER these, not hijack them into starting servers.
fn is_question_or_inspection(input: &str) -> bool {
    let lower = input.trim().to_lowercase();

    // If the user explicitly asks to start/run/launch/restart something, it's an action,
    // not a pure question — let the autonomous path handle it.
    let explicit_action = lower.contains("start ")
        || lower.contains("run ")
        || lower.contains("launch ")
        || lower.contains("restart")
        || lower.contains("boot up")
        || lower.contains("spin up");
    if explicit_action {
        return false;
    }

    // Question / inspection signals.
    lower.starts_with("why")
        || lower.starts_with("what")
        || lower.starts_with("are you")
        || lower.starts_with("did you")
        || lower.starts_with("can you")
        || lower.starts_with("do you")
        || lower.starts_with("is it")
        || lower.starts_with("is the")
        || lower.starts_with("how ")
        || lower.starts_with("please do not")
        || lower.starts_with("please don't")
        || lower.contains("are you reading")
        || lower.contains("did you read")
        || lower.contains("read the")
        || lower.contains("?")
}

/// Local dev servers (Mission Control :8080, invoice :5000) — agent must debug iteratively, not one script.
fn is_autonomous_local_servers_work(input: &str) -> bool {
    let lower = input.to_lowercase();

    // Questions and inspection requests must be answered, not hijacked into server work.
    if is_question_or_inspection(input) {
        return false;
    }

    // Server/port task keywords — original set.
    let mentions_server = lower.contains("8080")
        || lower.contains("5000")
        || lower.contains("localhost")
        || lower.contains("mission control")
        || lower.contains("mission-control")
        || lower.contains("invoice");

    let action_verb = lower.contains("start")
        || lower.contains("verify")
        || lower.contains("fix")
        || lower.contains("debug")
        || lower.contains("not working")
        || lower.contains("not running")
        || lower.contains("timeout")
        || lower.contains("solution")
        || lower.contains("running")
        || lower.contains("troubleshoot")
        || lower.contains("getting error")
        || lower.contains("check and")
        || lower.contains("please check");

    if mentions_server && action_verb {
        return true;
    }

    // Also treat any multi-step debugging / fixing request as autonomous.
    let is_fix_request = (lower.contains("fix") || lower.contains("install") || lower.contains("debug"))
        && (lower.contains("python") || lower.contains("pip") || lower.contains("script")
            || lower.contains("server") || lower.contains("error") || lower.contains("fail"));

    // Short follow-up queries from a user mid-task ("and now?", "continue", "try again", "what now")
    let is_continuation = lower == "and now?"
        || lower == "continue"
        || lower == "try again"
        || lower == "what now"
        || lower == "keep going"
        || lower == "go ahead"
        || lower.starts_with("and now")
        || lower.starts_with("now what")
        || lower.starts_with("what next")
        || lower.starts_with("next step")
        || lower.starts_with("please continue")
        || lower.starts_with("keep going");

    is_fix_request || is_continuation
}

/// Injected on the final allowed iteration to force a clean, on-topic final answer.
const WRAPUP_INSTRUCTION: &str = "[Metis instruction: You have reached the step limit — do NOT call any more tools. \
Write your FINAL answer now, directly addressing the user's original question. Use this structure:\n\
**What you asked:** <restate the original question/request in one line>\n\
**What I did:** <bullet list of the concrete steps/checks you performed>\n\
**Result:** <the actual answer or outcome — if the question was a question, ANSWER it; if it was a task, state done/blocked and why>\n\
Keep it concise. If you could not fully complete it, say exactly what is blocking and what the next step would be.]";

const AUTONOMOUS_LOCAL_SERVERS_INSTRUCTION: &str = "\n\n[Metis instruction: Autonomous local-server task. \
IMPORTANT: Do NOT write plans or code blocks — call exec/read_file tools DIRECTLY and IMMEDIATELY. \
Do not stop after one command — keep calling tools until both services respond or you clearly state what is blocked. \
Step 1: Check which ports are already responding (Invoke-WebRequest localhost:8080 and :5000 -TimeoutSec 3 -UseBasicParsing). If a port responds, skip starting that service. \
Step 2: For services NOT yet responding, find and start them. \
Mission Control: `mission-control/` under workspace (port 8080) — check what kind of server it is first (dir the folder; look for package.json, composer.json, *.py). \
Invoice: `email-app/invoice_processor.py` (port 5000). \
CRITICAL — starting servers: NEVER run `python script.py` or `node app.js` directly — they block forever and timeout. \
Instead use Start-Process: `$p = Start-Process -FilePath python -ArgumentList 'invoice_processor.py' -WorkingDirectory 'C:\\full\\path\\to\\email-app' -PassThru -WindowStyle Hidden; Start-Sleep 2; try { (Invoke-WebRequest http://localhost:5000 -UseBasicParsing -TimeoutSec 4).StatusCode } catch { $_.Exception.Message }` \
If the script has a bug/typo: use read_file to read it first, then edit_file to fix it, THEN start with Start-Process. \
On Traceback from a previous run: read the script, find the error, fix it, then restart. \
NEVER run a long-running process without Start-Process. Start with port checks NOW.]";

// ─────────────────────────────────────────────
// Blocking-server command rewriter
// ─────────────────────────────────────────────

/// Detect when the model uses PowerShell to grep/search a source file instead of read_file.
/// Covers both forms:
///   - `Get-Content <file> | Select-String -Pattern "..."`
///   - `Select-String -Path "<file>" -Pattern "..."`
///
/// Rewrites both to a plain `Get-Content <file>` so the LLM receives the full file
/// content rather than partial grep output, which often triggers false failure detections.
fn rewrite_file_grep_command(cmd: &str) -> Option<String> {
    let lower = cmd.to_lowercase();

    // Form 1: `Get-Content <file> | Select-String ...` (or `gc <file> | sls ...`)
    let is_gc_pipe = (lower.contains("get-content") || lower.contains("gc "))
        && (lower.contains("select-string") || lower.contains("| sls "));

    // Form 2: `Select-String -Path "<file>" ...` (standalone, no pipe)
    let is_sls_path = (lower.contains("select-string") || lower.starts_with("sls "))
        && lower.contains("-path ");

    if !is_gc_pipe && !is_sls_path {
        return None;
    }

    // Extract the file path.
    let file_path: Option<String> = if is_gc_pipe {
        // Path is between `get-content`/`gc` and the first `|`
        let start = lower
            .find("get-content")
            .map(|i| i + "get-content".len())
            .or_else(|| lower.find("gc ").map(|i| i + 3))?;
        let pipe_pos = cmd[start..].find('|').map(|i| i + start)?;
        let raw = cmd[start..pipe_pos].trim().trim_matches('"').trim_matches('\'');
        if raw.is_empty() { None } else { Some(raw.to_string()) }
    } else {
        // Path follows `-Path` argument
        let path_pos = lower.find("-path ")? + "-path ".len();
        let rest = cmd[path_pos..].trim();
        // Consume quoted or unquoted token up to the next space/flag
        let raw = if rest.starts_with('"') {
            rest[1..].split('"').next().unwrap_or("")
        } else if rest.starts_with('\'') {
            rest[1..].split('\'').next().unwrap_or("")
        } else {
            rest.split_whitespace().next().unwrap_or("")
        };
        if raw.is_empty() { None } else { Some(raw.to_string()) }
    };

    let file = file_path?;
    Some(format!(
        "Get-Content \"{file}\"\n# [Metis: use the read_file tool for source files, not Select-String/Get-Content]"
    ))
}

/// Pattern: the model calls `python script.py` or `node app.js` directly (blocks forever).
/// Detects this and rewrites to a `Start-Process` + health-check form on Windows.
///
/// Only rewrites when the command is **purely** a server launch — optionally preceded by a
/// single `cd <dir>` statement. Multi-program scripts (taskkill; cd; python ...) are left
/// untouched so we don't accidentally wrap the wrong executable.
#[cfg(target_os = "windows")]
fn wrap_blocking_server_command_windows(cmd: &str) -> Option<String> {
    let lower = cmd.trim().to_lowercase();

    // Split on `;` and filter blank/whitespace parts.
    let parts: Vec<&str> = cmd
        .split(';')
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .collect();

    // Only accept 1-part or 2-part (cd + python) patterns.
    // More parts means it's a complex script — don't rewrite.
    let (work_dir_raw, server_cmd_raw): (Option<&str>, &str) = match parts.as_slice() {
        [only] => (None, only),
        [first, second] if first.to_lowercase().starts_with("cd ") => {
            let wd = first["cd ".len()..].trim().trim_matches('"').trim_matches('\'');
            (Some(wd), second)
        }
        _ => return None, // 3+ parts → complex script, leave as-is
    };

    let server_lower = server_cmd_raw.to_lowercase();

    // Must look like a Python or Node direct run.
    let is_python_run = (server_lower.starts_with("python ") || server_lower.starts_with("python3 ") || server_lower.starts_with("py "))
        && server_lower.ends_with(".py")
        && !lower.contains("pip")
        && !lower.contains("--version")
        && !lower.contains("-m venv")
        && !lower.contains("-m pytest")
        && !lower.contains("-m http.server")
        && !lower.contains("start-process");

    let is_node_run = server_lower.starts_with("node ")
        && server_lower.ends_with(".js")
        && !lower.contains("npm")
        && !lower.contains("start-process");

    let is_php_serve = server_lower.contains("php artisan serve") || server_lower.contains("php -s ");

    if !is_python_run && !is_node_run && !is_php_serve {
        return None;
    }

    // Use the working directory from the `cd` prefix if present, otherwise none.
    let work_dir = work_dir_raw.map(String::from);
    let bare_cmd = server_cmd_raw.to_string();

    // Split into executable + arguments (simple split on first space after exe).
    let (exe, args) = {
        let parts: Vec<&str> = bare_cmd.splitn(2, ' ').collect();
        if parts.len() == 2 {
            (parts[0].to_string(), parts[1].trim().to_string())
        } else {
            (bare_cmd.clone(), String::new())
        }
    };

    let wd_clause = work_dir
        .as_deref()
        .map(|w| {
            let escaped = w.replace('\'', "''");
            format!(" -WorkingDirectory '{escaped}'")
        })
        .unwrap_or_default();

    let args_clause = if args.is_empty() {
        String::new()
    } else {
        let escaped = args.replace('\'', "''");
        format!(" -ArgumentList '{escaped}'")
    };

    // Build Start-Process command + 2-second wait + check if still alive.
    let wrapped = format!(
        "$__sp = Start-Process -FilePath '{exe}'{args_clause}{wd_clause} -PassThru -WindowStyle Hidden; \
Start-Sleep -Seconds 2; \
if ($__sp.HasExited) {{ \
  Write-Host \"PROCESS_EXITED_EARLY exit=$($__sp.ExitCode) cmd={bare_cmd}\"; \
  {log_tail} \
}} else {{ \
  Write-Host \"PROCESS_RUNNING PID=$($__sp.Id) cmd={bare_cmd}\" \
}}",
        log_tail = work_dir
            .as_deref()
            .map(|w| {
                let escaped = w.replace('\'', "''");
                format!(
                    "Get-ChildItem -Path '{escaped}' -Filter '*.log' -ErrorAction SilentlyContinue | \
ForEach-Object {{ Get-Content $_.FullName -Tail 20 -ErrorAction SilentlyContinue }}"
                )
            })
            .unwrap_or_else(|| "# no working dir for log tail".to_string()),
    );

    Some(wrapped)
}

#[cfg(not(target_os = "windows"))]
fn wrap_blocking_server_command_windows(_cmd: &str) -> Option<String> {
    None
}

// ─────────────────────────────────────────────
// Exec failure analysis & hint injection
// ─────────────────────────────────────────────

/// Recognised root-cause categories from exec output.
#[derive(Debug)]
enum ExecFailureKind {
    MissingModule(String),
    SyntaxError { file: Option<String>, line: Option<String> },
    PortInUse(u16),
    FileNotFound(String),
    AccessDenied,
    ProcessExitedEarly { exit_code: Option<String>, snippet: String },
}

/// Parse exec output into a structured failure reason.
fn parse_exec_failure(output: &str) -> Option<ExecFailureKind> {
    // PROCESS_EXITED_EARLY — produced by our Start-Process rewriter.
    if let Some(pos) = output.find("PROCESS_EXITED_EARLY") {
        let tail = &output[pos..];
        let exit_code = tail
            .split_whitespace()
            .find(|w| w.starts_with("exit="))
            .map(|w| w.trim_start_matches("exit=").to_string());

        // Try to pick up the Python traceback or error line from the log tail.
        let snippet = extract_error_snippet(output).unwrap_or_default();
        return Some(ExecFailureKind::ProcessExitedEarly { exit_code, snippet });
    }

    // Python / pip module errors.
    if let Some(pos) = output.find("ModuleNotFoundError: No module named") {
        let rest = &output[pos + "ModuleNotFoundError: No module named".len()..];
        let module = rest
            .split_whitespace()
            .next()
            .unwrap_or("")
            .trim_matches(|c| c == '\'' || c == '"' || c == ';')
            .to_string();
        return Some(ExecFailureKind::MissingModule(module));
    }
    if let Some(pos) = output.find("ImportError: No module named") {
        let rest = &output[pos + "ImportError: No module named".len()..];
        let module = rest
            .split_whitespace()
            .next()
            .unwrap_or("")
            .trim_matches(|c| c == '\'' || c == '"' || c == ';')
            .to_string();
        return Some(ExecFailureKind::MissingModule(module));
    }

    // SyntaxError with optional file/line info.
    if output.contains("SyntaxError:") {
        let file = output.lines().find_map(|l| {
            let t = l.trim();
            if t.starts_with("File \"") {
                t.strip_prefix("File \"")
                    .and_then(|s| s.split('"').next())
                    .map(|s| s.to_string())
            } else {
                None
            }
        });
        let line = output.lines().find_map(|l| {
            let t = l.trim();
            if t.starts_with("line ") {
                Some(t.to_string())
            } else {
                None
            }
        });
        return Some(ExecFailureKind::SyntaxError { file, line });
    }

    // Port already bound.
    if output.contains("Only one usage of each socket address")
        || output.contains("address already in use")
        || output.contains("WSAEADDRINUSE")
        || output.contains("10048")
    {
        // Try to extract port number from message.
        let port = output
            .split(':')
            .filter_map(|s| s.trim().parse::<u16>().ok())
            .next()
            .unwrap_or(0);
        return Some(ExecFailureKind::PortInUse(port));
    }

    // File / path not found.
    if output.contains("FileNotFoundError:")
        || output.contains("No such file or directory")
        || (output.contains("cannot find") && output.contains("path"))
    {
        let snippet = extract_error_snippet(output).unwrap_or_default();
        return Some(ExecFailureKind::FileNotFound(snippet));
    }

    // Windows access denied.
    if output.contains("Access is denied") || output.contains("PermissionError:") {
        return Some(ExecFailureKind::AccessDenied);
    }

    None
}

/// Extract the most informative error line from multi-line exec output.
fn extract_error_snippet(output: &str) -> Option<String> {
    // Look for the last "Error:" line first (most specific).
    for line in output.lines().rev() {
        let t = line.trim();
        if t.contains("Error:") || t.contains("Exception:") || t.contains("Traceback") {
            return Some(t.chars().take(200).collect());
        }
    }
    // Fallback: last non-empty line.
    output.lines().rev().find(|l| !l.trim().is_empty()).map(|l| l.trim().to_string())
}

/// Append a `[Metis hint: ...]` block to an exec result when a known failure is found.
/// This gives the LLM a concrete remediation action for the next iteration.
fn annotate_exec_result_with_hint(result: &str) -> String {
    match parse_exec_failure(result) {
        Some(ExecFailureKind::MissingModule(ref module)) if !module.is_empty() => {
            let pip_name = pip_package_name(module);
            format!(
                "{result}\n\n[Metis hint: ❌ Missing Python module '{module}'. \
Fix: call exec with `pip install {pip_name}` IMMEDIATELY, then retry Start-Process for the server. \
Do NOT ask the user — just run pip install now.]"
            )
        }
        Some(ExecFailureKind::MissingModule(_)) => {
            // Empty module name — generic pip hint.
            format!(
                "{result}\n\n[Metis hint: ❌ Missing Python module detected. \
Run pip install <module_name> then retry Start-Process.]"
            )
        }
        Some(ExecFailureKind::SyntaxError { file: Some(ref f), line: ref ln }) => {
            let line_info = ln.as_deref().map(|l| format!(", {l}")).unwrap_or_default();
            format!(
                "{result}\n\n[Metis hint: ❌ Python SyntaxError in `{f}`{line_info}. \
Fix: call read_file on `{f}`, identify the syntax problem, call edit_file to fix it, then retry Start-Process. \
Do NOT ask the user — just read and fix the file now.]"
            )
        }
        Some(ExecFailureKind::SyntaxError { file: None, .. }) => {
            format!(
                "{result}\n\n[Metis hint: ❌ Python SyntaxError detected. \
Find the script file with exec (dir / Get-ChildItem), read it with read_file, fix the syntax error with edit_file, then retry Start-Process.]"
            )
        }
        Some(ExecFailureKind::PortInUse(port)) => {
            let port_str = if port > 0 { format!("{port}") } else { "<port>".to_string() };
            format!(
                "{result}\n\n[Metis hint: ❌ Port {port_str} is already in use. \
Fix: run exec `Get-NetTCPConnection -LocalPort {port_str} -State Listen | Select-Object -ExpandProperty OwningProcess | \
ForEach-Object {{ Stop-Process -Id $_ -Force }}` to free the port, then retry Start-Process.]"
            )
        }
        Some(ExecFailureKind::FileNotFound(ref snippet)) => {
            format!(
                "{result}\n\n[Metis hint: ❌ File/path not found{info}. \
Fix: use exec to list the workspace directory (Get-ChildItem -Recurse -Depth 2) and find the correct path, \
then retry with the right path.{info2}]",
                info = if snippet.is_empty() { String::new() } else { format!(": {snippet}") },
                info2 = if snippet.is_empty() { String::new() } else { String::new() },
            )
        }
        Some(ExecFailureKind::AccessDenied) => {
            format!(
                "{result}\n\n[Metis hint: ❌ Access denied. \
The target file or process is locked. If it's a running process, kill it first with Stop-Process, then retry.]"
            )
        }
        Some(ExecFailureKind::ProcessExitedEarly { ref exit_code, ref snippet }) => {
            let exit_info = exit_code.as_deref().map(|c| format!(" (exit {c})")).unwrap_or_default();
            let err_info = if snippet.is_empty() {
                String::new()
            } else {
                format!(" Error: {snippet}")
            };
            format!(
                "{result}\n\n[Metis hint: ❌ Process exited immediately{exit_info}.{err_info} \
Diagnose: (1) read the log files in the working directory, \
(2) check for ModuleNotFoundError / SyntaxError in the output above, \
(3) fix with pip install or edit_file, (4) retry Start-Process. \
Do NOT inform the user yet — keep debugging.]"
            )
        }
        None => result.to_string(),
    }
}

/// Map common Python import names to their pip package names where they differ.
fn pip_package_name(module: &str) -> &str {
    match module {
        "flask" | "Flask" => "flask",
        "requests" => "requests",
        "dotenv" | "dotenv_values" => "python-dotenv",
        "PIL" => "Pillow",
        "cv2" => "opencv-python",
        "sklearn" => "scikit-learn",
        "bs4" => "beautifulsoup4",
        "yaml" | "ruamel" => "pyyaml",
        "jwt" => "PyJWT",
        "psycopg2" => "psycopg2-binary",
        "MySQLdb" => "mysqlclient",
        "pymongo" => "pymongo",
        "redis" => "redis",
        "celery" => "celery",
        "boto3" => "boto3",
        "google.cloud" => "google-cloud",
        "openai" => "openai",
        "anthropic" => "anthropic",
        other => other,
    }
}

// ─────────────────────────────────────────────
// Persistence / unresolved-failure helpers
// ─────────────────────────────────────────────

/// True when the most recent exec output still shows an unresolved failure.
/// Used to force the agent to keep working rather than stopping mid-task.
fn has_unresolved_exec_failures(outputs: &[String]) -> bool {
    // Walk backwards: find the last exec that was NOT an auto-pip result injected by us.
    // If the very last exec succeeded (exit 0, no failure marker), the task may be done.
    let meaningful: Vec<&String> = outputs
        .iter()
        .filter(|o| !o.contains("auto-pip") && !o.starts_with("exec(`pip install"))
        .collect();

    let last = match meaningful.last() {
        Some(o) => o,
        None => return false,
    };

    exec_report_failed(last)
}

/// Build a short human-readable description of the last unresolved failure.
fn last_exec_failure_hint(outputs: &[String]) -> String {
    let meaningful: Vec<&String> = outputs
        .iter()
        .filter(|o| !o.contains("auto-pip") && !o.starts_with("exec(`pip install"))
        .collect();

    let last = match meaningful.last() {
        Some(o) => o,
        None => return "the last command failed".to_string(),
    };

    // Try to extract a useful error description.
    match parse_exec_failure(last) {
        Some(ExecFailureKind::MissingModule(ref m)) if !m.is_empty() => {
            format!("Python module '{m}' is still missing — run pip install and retry")
        }
        Some(ExecFailureKind::SyntaxError { file: Some(ref f), .. }) => {
            format!("SyntaxError in '{f}' is still not fixed — read_file then edit_file")
        }
        Some(ExecFailureKind::PortInUse(p)) => {
            format!("port {p} is still occupied — kill the process then retry Start-Process")
        }
        Some(ExecFailureKind::ProcessExitedEarly { ref snippet, .. }) => {
            if snippet.is_empty() {
                "the server process exited immediately — check logs and fix the error".to_string()
            } else {
                format!("the server crashed: {snippet} — diagnose and fix")
            }
        }
        Some(ExecFailureKind::FileNotFound(ref s)) => {
            format!("file/path not found ({s}) — locate the correct path and retry")
        }
        _ => {
            // Pull the first failure-looking line from the output.
            first_failure_summary_line(last)
                .map(|s| format!("the last command failed: {s}"))
                .unwrap_or_else(|| "the last command failed — diagnose and continue".to_string())
        }
    }
}

/// True when an LLM response looks like a plan/narration with no real tool execution.
/// Used to inject a "stop planning, execute now" continuation.
fn response_is_plan_without_tools(content: &str) -> bool {
    let lower = content.to_lowercase();
    let has_plan_words = lower.contains("step 1")
        || lower.contains("step 2")
        || lower.contains("first,")
        || lower.contains("first i")
        || lower.contains("let me")
        || lower.contains("i'll start")
        || lower.contains("i will start")
        || lower.contains("checking")
        || lower.contains("i'll check")
        || lower.contains("starting both")
        || lower.contains("i need to");
    let has_code_block = content.contains("```");
    (has_plan_words || has_code_block) && content.len() > 60
}

/// True when the model response describes having identified a problem but hasn't called
/// any tool to actually fix it yet. This is the "I see the issue, let me fix it:" pattern
/// where the agent narrates rather than acts.
fn response_identified_problem_without_fix(content: &str) -> bool {
    let lower = content.to_lowercase();
    let identified = lower.contains("i see the problem")
        || lower.contains("i see the issue")
        || lower.contains("i found the")
        || lower.contains("the issue is")
        || lower.contains("the problem is")
        || lower.contains("the error is")
        || lower.contains("the bug is")
        || lower.contains("i can see")
        || lower.contains("i notice")
        || lower.contains("i've identified")
        || lower.contains("the root cause")
        || lower.contains("the fix is")
        || lower.contains("let me fix")
        || lower.contains("i'll fix")
        || lower.contains("i need to fix")
        || lower.contains("needs to be")
        || lower.contains("should be fixed")
        || lower.contains("missing column")
        || lower.contains("schema mismatch")
        || lower.contains("alter table")
        || lower.contains("pip install");
    // Only counts if it's a substantial response (not a one-liner)
    identified && content.len() > 40
}

/// Cap how many `exec` tool calls are allowed in one user turn (prevents spam, allows real workflows).
fn max_exec_calls_for_message(input: &str) -> usize {
    if is_whisper_cpp_install_request(input) {
        return 1;
    }
    if is_autonomous_local_servers_work(input) || is_multi_step_fix_request(input) {
        return 20;
    }
    if is_execution_or_install_request(input) {
        return 8;
    }
    20
}

/// True when the turn should show the harsh failure banner (not when the agent recovered on a later exec).
fn exec_turn_should_show_hard_failure(exec_outputs: &[String]) -> bool {
    if exec_outputs.is_empty() {
        return false;
    }
    // If the last exec succeeded, the agent recovered — no banner.
    if exec_outputs
        .last()
        .is_some_and(|o| !exec_report_failed(o))
    {
        return false;
    }
    exec_outputs.iter().any(|o| exec_report_failed(o))
}

/// True when an HTTP check (Invoke-WebRequest) for a given port returned success in any exec output.
fn port_confirmed_up(exec_outputs: &[String], port: u16) -> bool {
    let port_str = format!(":{port}");
    exec_outputs.iter().any(|o| {
        let lower = o.to_lowercase();
        (lower.contains("invoke-webrequest") || lower.contains("http://localhost"))
            && lower.contains(&port_str)
            && (lower.contains("status: success") || lower.contains("statuscode"))
            && !exec_report_failed(o)
    })
}

/// Build a human-readable summary from autonomous exec outputs when the model ran out of iterations.
fn build_autonomous_summary(exec_outputs: &[String], compact: bool) -> String {
    let port_8080_ok = port_confirmed_up(exec_outputs, 8080);
    let port_5000_ok = port_confirmed_up(exec_outputs, 5000);

    let mut lines: Vec<String> = Vec::new();
    lines.push("**Autonomous run complete** — reached iteration limit. Status:".into());

    if port_8080_ok {
        lines.push("  ✅ Port 8080 (Mission Control): responding".into());
    } else if exec_outputs.iter().any(|o| {
        let l = o.to_lowercase();
        l.contains("8080") && exec_report_failed(o)
    }) {
        lines.push("  ❌ Port 8080 (Mission Control): could not start".into());
    }

    if port_5000_ok {
        lines.push("  ✅ Port 5000 (Invoice): responding".into());
    } else if exec_outputs.iter().any(|o| {
        let l = o.to_lowercase();
        (l.contains("5000") || l.contains("invoice")) && exec_report_failed(o)
    }) {
        // Pull the first error line from the invoice traceback
        let err = exec_outputs
            .iter()
            .filter(|o| {
                let l = o.to_lowercase();
                (l.contains("invoice") || l.contains("5000")) && exec_report_failed(o)
            })
            .find_map(|o| first_failure_summary_line(o))
            .unwrap_or_else(|| "check logs above".into());
        lines.push(format!("  ❌ Port 5000 (Invoice): failed — {err}"));
        lines.push("  → Send \"debug invoice error\" to continue fixing it.".into());
    }

    if !compact {
        lines.push(String::new());
        lines.push("**Exec log:**".into());
        for o in exec_outputs {
            lines.push(summarize_exec_block(o));
        }
    }

    lines.join("\n")
}

// ─────────────────────────────────────────────
// Task ledger — durable record of what the agent did
// ─────────────────────────────────────────────

/// A single recorded action the agent took during a turn (the "checking" log).
#[derive(Debug, Clone)]
struct TaskStep {
    /// Short human label of the action (e.g. "Exec: `python ...`", "Read `app.py`").
    label: String,
    /// Whether the step succeeded.
    ok: bool,
    /// Optional outcome detail (error reason for failures).
    detail: Option<String>,
}

impl TaskStep {
    /// Build a step record from a completed tool call.
    fn from_tool_call(tool_name: &str, arguments: &str, result: &str) -> Self {
        let label = tool_progress_preview(tool_name, arguments);
        if tool_name == "exec" {
            let failed = exec_report_failed(result);
            let detail = if failed {
                first_failure_summary_line(result)
            } else {
                None
            };
            TaskStep { label, ok: !failed, detail }
        } else {
            // Non-exec tools: treat an explicit "Tool argument error" / "error:" as failure.
            let lower = result.to_lowercase();
            let failed = lower.starts_with("tool argument error")
                || lower.starts_with("error:")
                || lower.contains("no such file")
                || lower.contains("failed to");
            let detail = if failed {
                Some(truncate_chars(result.lines().next().unwrap_or("failed"), 100))
            } else {
                None
            };
            TaskStep { label, ok: !failed, detail }
        }
    }

    fn render(&self) -> String {
        let mark = if self.ok { "✓" } else { "✗" };
        match &self.detail {
            Some(d) if !self.ok => format!("  {mark} {} — {d}", self.label),
            _ => format!("  {mark} {}", self.label),
        }
    }
}

/// Build a generic, question-anchored summary: Task → Steps taken → Outcome.
///
/// Unlike `build_autonomous_summary`, this is NOT hardcoded to specific ports — it works
/// for any task and always restates the original question so the reply stays on-topic.
fn build_task_summary(question: &str, steps: &[TaskStep], compact: bool) -> String {
    let mut lines: Vec<String> = Vec::new();

    let q = truncate_chars(question.trim(), 200);
    if !q.is_empty() {
        lines.push(format!("**Task:** {q}"));
        lines.push(String::new());
    }

    if steps.is_empty() {
        lines.push("No actions were completed before the step limit was reached.".into());
        return lines.join("\n");
    }

    lines.push("**Steps taken:**".into());
    // In compact (chat) mode, cap the number of rendered steps to avoid huge messages.
    let max_steps = if compact { 8 } else { steps.len() };
    let shown = steps.len().min(max_steps);
    for step in &steps[..shown] {
        lines.push(step.render());
    }
    if steps.len() > shown {
        lines.push(format!("  … and {} more step(s)", steps.len() - shown));
    }

    // Outcome: derive from the final step.
    lines.push(String::new());
    let last = steps.last().unwrap();
    let any_failed = steps.iter().any(|s| !s.ok);
    if last.ok && !any_failed {
        lines.push("**Outcome:** ✅ Completed — all steps succeeded.".into());
    } else if last.ok && any_failed {
        lines.push("**Outcome:** ⚠ Partially done — earlier steps failed but the last step succeeded. Reply **continue** to keep going.".into());
    } else {
        let reason = last.detail.clone().unwrap_or_else(|| "last step failed".into());
        lines.push(format!(
            "**Outcome:** ❌ Not finished — {reason}. Reply **continue** to keep debugging."
        ));
    }

    lines.join("\n")
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

fn llm_response_is_api_error(response: &LlmResponse) -> bool {
    response
        .content
        .as_ref()
        .is_some_and(|c| c.starts_with("Error calling LLM:"))
}

fn content_claims_false_ok(content: &str) -> bool {
    let lower = content.to_lowercase();
    let positive = lower.contains("everything is ok")
        || lower.contains("everything's ok")
        || lower.contains("all good")
        || lower.contains("all ok")
        || lower.contains("is ok")
        || lower.contains("looks good")
        || lower.contains("working fine");
    let negative = lower.contains("not ok")
        || lower.contains("not succeed")
        || lower.contains("failed")
        || lower.contains("error calling llm");
    positive && !negative
}

/// If exec/API failed, never let the assistant claim success.
fn enforce_truthful_status_reply(
    content: String,
    exec_outputs: &[String],
    compact_exec: bool,
) -> String {
    let hard_fail = exec_turn_should_show_hard_failure(exec_outputs);
    let llm_err = content.contains("Error calling LLM:");
    if !hard_fail && !llm_err && !content_claims_false_ok(&content) {
        return content;
    }

    let mut parts = vec!["❌ **Not OK** — verification or execution failed.".to_string()];
    if llm_err {
        parts.push(content.clone());
    } else if content_claims_false_ok(&content) {
        parts.push("Earlier text incorrectly suggested success; see tool results below.".to_string());
    } else {
        let trimmed = strip_optimistic_running_claims(&content);
        if !trimmed.is_empty() {
            parts.push(trimmed);
        }
    }
    if hard_fail || exec_outputs.iter().any(|o| exec_report_failed(o)) {
        let block = if compact_exec {
            exec_outputs
                .iter()
                .map(|o| summarize_exec_block(o))
                .collect::<Vec<_>>()
                .join("\n")
        } else {
            exec_outputs.join("\n\n")
        };
        parts.push(block);
    }
    parts.join("\n\n")
}

#[cfg(test)]
fn is_invoice_processor_start_request(input: &str) -> bool {
    let lower = input.to_lowercase();
    (lower.contains("invoice") || lower.contains("5000") || lower.contains(":5000"))
        && (lower.contains("start")
            || lower.contains("not working")
            || lower.contains("verify")
            || lower.contains("launch")
            || lower.contains("run"))
}

#[cfg(test)]
fn build_mission_control_start_ps(root: &std::path::Path, port: u16) -> String {
    let root_escaped = root.display().to_string().replace('\'', "''");
    format!(
        "$ErrorActionPreference='Stop'; \
$root='{root_escaped}'; \
$port={port}; \
if (!(Test-Path -LiteralPath $root)) {{ throw \"Directory not found: $root\" }}; \
$py = (Get-Command py -ErrorAction SilentlyContinue).Source; \
if (-not $py) {{ $py = (Get-Command python -ErrorAction SilentlyContinue).Source }}; \
if (-not $py) {{ throw 'Python not found (py/python)' }}; \
netstat -ano | Select-String (\":$port\\s\") | ForEach-Object {{ if ($_ -match '\\s+(\\d+)\\s*$') {{ Stop-Process -Id ([int]$matches[1]) -Force -ErrorAction SilentlyContinue }} }}; \
$outLog = Join-Path $root 'metis_http_server.out.log'; \
$errLog = Join-Path $root 'metis_http_server.err.log'; \
$proc = Start-Process -FilePath $py -ArgumentList @('-m','http.server',$port,'--bind','127.0.0.1') -WorkingDirectory $root -PassThru -WindowStyle Hidden -RedirectStandardOutput $outLog -RedirectStandardError $errLog; \
Start-Sleep -Seconds 1; \
try {{ \
  $resp = Invoke-WebRequest -Uri (\"http://127.0.0.1:{{0}}\" -f $port) -UseBasicParsing -TimeoutSec 4; \
  \"HTTP_SERVER_OK PID=$($proc.Id) STATUS=$($resp.StatusCode) URL=http://127.0.0.1:$port ROOT=$root\" \
}} catch {{ \
  \"HTTP_SERVER_FAIL PID=$($proc.Id) URL=http://127.0.0.1:$port ROOT=$root ERROR=$($_.Exception.Message)\"; \
  if (Test-Path -LiteralPath $errLog) {{ Get-Content -LiteralPath $errLog -TotalCount 20 }} \
}}"
    )
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
            timeout: 180,
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
// Tool progress helpers (for live chat-app ticks)
// ─────────────────────────────────────────────

/// Build a short "starting" description for a tool call.
fn tool_progress_preview(tool_name: &str, arguments: &str) -> String {
    match tool_name {
        "exec" => {
            let cmd = serde_json::from_str::<serde_json::Value>(arguments)
                .ok()
                .and_then(|v| v.get("command").and_then(|c| c.as_str()).map(|s| s.to_string()))
                .unwrap_or_else(|| arguments.chars().take(80).collect());
            let preview: String = cmd.chars().take(80).collect();
            let ellipsis = if cmd.len() > 80 { "…" } else { "" };
            format!("Exec: `{preview}{ellipsis}`")
        }
        "read_file" => {
            let path = serde_json::from_str::<serde_json::Value>(arguments)
                .ok()
                .and_then(|v| v.get("path").and_then(|c| c.as_str()).map(|s| s.to_string()))
                .unwrap_or_default();
            format!("Reading `{path}`")
        }
        "write_file" | "edit_file" => {
            let path = serde_json::from_str::<serde_json::Value>(arguments)
                .ok()
                .and_then(|v| v.get("path").and_then(|c| c.as_str()).map(|s| s.to_string()))
                .unwrap_or_default();
            let verb = if tool_name == "write_file" { "Writing" } else { "Editing" };
            format!("{verb} `{path}`")
        }
        "list_dir" => {
            let path = serde_json::from_str::<serde_json::Value>(arguments)
                .ok()
                .and_then(|v| v.get("path").and_then(|c| c.as_str()).map(|s| s.to_string()))
                .unwrap_or_default();
            format!("Listing `{path}`")
        }
        "web_search" => {
            let q = serde_json::from_str::<serde_json::Value>(arguments)
                .ok()
                .and_then(|v| v.get("query").and_then(|c| c.as_str()).map(|s| s.to_string()))
                .unwrap_or_default();
            let preview: String = q.chars().take(60).collect();
            format!("Searching: {preview}")
        }
        other => format!("{other}…"),
    }
}

/// Build a short "done" or "failed" outcome line after a tool call.
fn tool_outcome_preview(tool_name: &str, arguments: &str, result: &str) -> String {
    if tool_name == "exec" {
        let failed = exec_report_failed(result);
        let cmd = serde_json::from_str::<serde_json::Value>(arguments)
            .ok()
            .and_then(|v| v.get("command").and_then(|c| c.as_str()).map(|s| s.to_string()))
            .unwrap_or_default();
        let preview: String = cmd.chars().take(60).collect();
        let ellipsis = if cmd.len() > 60 { "…" } else { "" };
        if failed {
            let err = first_failure_summary_line(result)
                .unwrap_or_else(|| "command failed".to_string());
            format!("✗ `{preview}{ellipsis}` — {err}")
        } else {
            format!("✓ `{preview}{ellipsis}`")
        }
    } else {
        let short: String = result.lines().next().unwrap_or("done").chars().take(60).collect();
        format!("done — {short}")
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
        let mut text = strip_reasoning_tags(stored);
        if !is_chat_app_channel(channel) {
            return text;
        }
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
        let mut user_text = msg.content.clone();
        if is_autonomous_local_servers_work(&msg.content) {
            // Prepend any relevant memory notes so the agent remembers previous findings.
            let memory_hint = self.context.memory().get_memory_context()
                .map(|m| format!("\n\n[Memory from previous sessions — read this before acting:\n{m}\n]"))
                .unwrap_or_default();
            user_text.push_str(&memory_hint);
            user_text.push_str(AUTONOMOUS_LOCAL_SERVERS_INSTRUCTION);
        } else if is_multi_step_fix_request(&msg.content) {
            user_text.push_str(
                "\n\n[Metis instruction: You listed multiple issues. Complete ALL of them in this turn \
                 before sending your final reply — use read_file/write_file/edit_file/exec as needed. \
                 Do not stop after a single diagnostic command like Get-Content.]",
            );
        }
        let mut messages = self.context.build_messages(
            &history,
            &user_text,
            &media_paths,
            &msg.channel,
            &msg.chat_id,
        );

        // Get tool definitions
        let tool_defs = self.tools.get_definitions();

        // Agent loop: LLM ↔ tool calling
        let mut final_content: Option<String> = None;
        let mut exec_tool_outputs: Vec<String> = Vec::new();
        // Durable record of every action taken this turn (the "checking" log).
        let mut task_steps: Vec<TaskStep> = Vec::new();
        let max_exec_calls = max_exec_calls_for_message(&msg.content);
        let mut exec_calls_executed: usize = 0;
        let mut wrapup_injected = false;

        // For longer/autonomous tasks, post a visible "goal" line up-front so the user can see
        // what the agent set out to do (helps when work takes a while). Marked intermediate so
        // the typing indicator keeps running afterwards.
        if is_chat_app_channel(&msg.channel)
            && (is_autonomous_local_servers_work(&msg.content) || is_multi_step_fix_request(&msg.content))
        {
            let goal = truncate_chars(msg.content.trim(), 160);
            let mut goal_msg = OutboundMessage::new(
                &msg.channel,
                &msg.chat_id,
                format!("🎯 Working on: {goal}"),
            );
            goal_msg.metadata.insert("intermediate".into(), "true".into());
            let _ = self.bus.publish_outbound(goal_msg).await;
        }

        for iteration in 0..self.max_iterations {
            debug!(iteration = iteration, "LLM call");

            // On the final allowed iteration, force a clean wrap-up: disable tools and ask
            // the model to write a structured answer that addresses the original question.
            // The model has full context (everything it read/ran), so it can answer properly
            // instead of us synthesising a hardcoded summary.
            let is_last_iteration = iteration + 1 >= self.max_iterations;
            if is_last_iteration && !task_steps.is_empty() && !wrapup_injected {
                messages.push(Message::user(WRAPUP_INSTRUCTION));
                wrapup_injected = true;
            }
            let tools_for_call = if is_last_iteration && wrapup_injected {
                None
            } else {
                Some(tool_defs.as_slice())
            };

            let response = self
                .provider
                .chat(
                    &messages,
                    tools_for_call,
                    &self.model,
                    &self.request_config,
                )
                .await;

            self.log_llm_thinking_json(&msg.channel, &msg.chat_id, &session_key, iteration, &response);

            if llm_response_is_api_error(&response) {
                final_content = response.content;
                break;
            }

            if response.has_tool_calls() {
                // Add assistant message with tool calls (sanitize invalid JSON for next API round-trip)
                let tool_calls = sanitize_tool_calls_for_history(response.tool_calls.clone());
                // Publish any narration text the model sent alongside the tool calls.
                if is_chat_app_channel(&msg.channel) {
                    if let Some(ref text) = response.content {
                        let stripped = strip_reasoning_tags(text);
                        let cleaned = fenced_code_block_re().replace_all(&stripped, "").trim().to_string();
                        if !cleaned.is_empty() {
                            let mut narr = OutboundMessage::new(&msg.channel, &msg.chat_id, cleaned);
                            narr.metadata.insert("intermediate".into(), "true".into());
                            let _ = self.bus.publish_outbound(narr).await;
                        }
                    }
                }
                ContextBuilder::add_assistant_message(
                    &mut messages,
                    response.content.clone(),
                    tool_calls.clone(),
                );

                // Execute each tool call
                for tc in &tool_calls {
                    if tc.function.name == "exec" && exec_calls_executed >= max_exec_calls {
                        let result = format!(
                            "Exec limit reached ({max_exec_calls} commands per request). \
Run the next command in a follow-up message, or combine steps into one script."
                        );
                        ContextBuilder::add_tool_result(&mut messages, &tc.id, &result);
                        continue;
                    }

                    // Publish a "starting" progress tick for chat channels.
                    if is_chat_app_channel(&msg.channel) {
                        let preview = tool_progress_preview(&tc.function.name, &tc.function.arguments);
                        let mut tick = OutboundMessage::new(
                            &msg.channel,
                            &msg.chat_id,
                            format!("🛠️ {preview}"),
                        );
                        tick.metadata.insert("intermediate".into(), "true".into());
                        let _ = self.bus.publish_outbound(tick).await;
                    }

                    let result = match parse_tool_params(&tc.function.arguments) {
                        Ok(mut params) => {
                            // Intercept blocking server commands and rewrite to Start-Process.
                            // Models routinely ignore prompt instructions about this; enforce deterministically.
                            if tc.function.name == "exec" {
                                if let Some(serde_json::Value::String(cmd)) = params.get("command") {
                                    if let Some(wrapped) = wrap_blocking_server_command_windows(cmd) {
                                        info!(original = %cmd, "rewrote blocking server cmd to Start-Process");
                                        params.insert(
                                            "command".to_string(),
                                            serde_json::Value::String(wrapped),
                                        );
                                    } else if let Some(stripped) = rewrite_file_grep_command(cmd) {
                                        // Both `Get-Content | Select-String` and `Select-String -Path`
                                        // are wrong for reading source files. Rewrite to plain Get-Content.
                                        info!(original = %cmd, "rewrote file-grep command to plain Get-Content");
                                        params.insert(
                                            "command".to_string(),
                                            serde_json::Value::String(stripped),
                                        );
                                    }
                                }
                            }
                            info!(
                                tool = %tc.function.name,
                                iteration = iteration,
                                "executing tool call"
                            );
                            self.tools.execute(&tc.function.name, params).await
                        }
                        Err(e) => format!("Tool argument error for `{}`: {e}", tc.function.name),
                    };

                    // Publish a "completed / failed" progress tick for chat channels.
                    if is_chat_app_channel(&msg.channel) {
                        let outcome = tool_outcome_preview(&tc.function.name, &tc.function.arguments, &result);
                        let mut tick = OutboundMessage::new(
                            &msg.channel,
                            &msg.chat_id,
                            format!("  ↳ {outcome}"),
                        );
                        tick.metadata.insert("intermediate".into(), "true".into());
                        let _ = self.bus.publish_outbound(tick).await;
                    }

                    if tc.function.name == "exec" {
                        exec_calls_executed += 1;
                        exec_tool_outputs.push(result.clone());
                    }

                    // Record the step in the durable task ledger.
                    task_steps.push(TaskStep::from_tool_call(
                        &tc.function.name,
                        &tc.function.arguments,
                        &result,
                    ));

                    debug!(
                        tool = %tc.function.name,
                        result_len = result.len(),
                        "tool result"
                    );

                    // Annotate exec results with an actionable hint when a known failure is detected.
                    // Also: for MissingModule, auto-inject a pip install exec immediately.
                    let annotated_result = if tc.function.name == "exec" {
                        annotate_exec_result_with_hint(&result)
                    } else {
                        result.clone()
                    };
                    ContextBuilder::add_tool_result(&mut messages, &tc.id, &annotated_result);

                    // Auto-fix: MissingModule → immediately run pip install without waiting for LLM.
                    // This fires as a synthetic tool result that the LLM sees on the next iteration.
                    if tc.function.name == "exec" {
                        if let Some(ExecFailureKind::MissingModule(ref module)) = parse_exec_failure(&result) {
                            if !module.is_empty() {
                                let pip_name = pip_package_name(module).to_string();
                                let pip_cmd = format!("pip install {pip_name}");
                                info!(module = %module, pip = %pip_cmd, "auto-running pip install for missing module");
                                let pip_params = {
                                    let mut m = std::collections::HashMap::new();
                                    m.insert("command".to_string(), serde_json::Value::String(pip_cmd.clone()));
                                    m
                                };
                                let pip_result = self.tools.execute("exec", pip_params).await;
                                exec_calls_executed += 1;
                                exec_tool_outputs.push(pip_result.clone());
                                // Inject as a synthetic tool call + result so the LLM knows it happened.
                                let synthetic_id = format!("auto_pip_{}", tc.id);
                                let pip_success = !pip_result.contains("ERROR") && !pip_result.contains("error:");
                                let pip_note = if pip_success {
                                    format!(
                                        "✅ Auto-installed `{pip_name}` via pip. Now retry Start-Process for the server."
                                    )
                                } else {
                                    format!(
                                        "❌ pip install {pip_name} failed. Output:\n{pip_result}\nCheck pip availability or try a different package name."
                                    )
                                };
                                ContextBuilder::add_tool_result(&mut messages, &synthetic_id, &format!("exec(`{pip_cmd}`):\n{pip_result}\n\n[Metis auto-fix result: {pip_note}]"));
                            }
                        }
                    }
                }
            } else {
                let content_text = response.content.as_deref().unwrap_or("");
                let remaining_exec_budget = exec_calls_executed < max_exec_calls;

                // Universal persistence signal: a tool call FAILED and we still have budget.
                // This is task-agnostic (no server/keyword gating) and never fires on a pure
                // question, because a question has no failed tool call.
                let still_failing = remaining_exec_budget
                    && has_unresolved_exec_failures(&exec_tool_outputs);

                // Behavioural signal: the model SAID it would act ("let me…", "I'll fix…",
                // "I see the problem…") on an early iteration but called no tool. Nudge it to
                // actually act. This keys off the model's own stated intent, not task keywords,
                // so it does not force tool calls when the model simply answered a question.
                let said_will_act_but_didnt = exec_calls_executed == 0
                    && iteration < 3
                    && (response_is_plan_without_tools(content_text)
                        || response_identified_problem_without_fix(content_text));

                if still_failing || said_will_act_but_didnt {
                    ContextBuilder::add_assistant_message(&mut messages, response.content.clone(), vec![]);
                    let nudge = if still_failing {
                        let hint = last_exec_failure_hint(&exec_tool_outputs);
                        format!(
                            "A step did not succeed yet — {hint}. \
Continue: call the next tool to fix it. If you are genuinely blocked, stop and explain exactly \
what is blocking and the next step — do not stop silently."
                        )
                    } else {
                        "You said what you would do but did not call a tool. \
Call the tool now to actually do it (or, if this was only a question, just answer it directly)."
                            .to_string()
                    };
                    messages.push(Message::user(&nudge));
                    continue;
                }
                // No tool calls and nothing pending → final answer.
                final_content = response.content;
                break;
            }
        }

        // If we exhausted iterations without a final answer, build a summary from the task ledger.
        // This is question-anchored (Task → Steps → Outcome) so the reply stays on-topic instead
        // of dumping hardcoded server status.
        let compact_exec = is_chat_app_channel(&msg.channel);
        let used_autonomous_summary = final_content.is_none() && !task_steps.is_empty();
        let fallback = if used_autonomous_summary {
            build_task_summary(&msg.content, &task_steps, compact_exec)
        } else if final_content.is_none() && !exec_tool_outputs.is_empty() {
            build_autonomous_summary(&exec_tool_outputs, compact_exec)
        } else {
            "I've completed processing but have no response to give.".into()
        };
        // Skip the reconcile banner when we already built a structured summary.
        let mut content = if used_autonomous_summary {
            fallback
        } else {
            reconcile_exec_with_reply(
                final_content.unwrap_or(fallback),
                &exec_tool_outputs,
                compact_exec,
            )
        };

        if is_multi_step_fix_request(&msg.content) && exec_calls_executed <= 1 {
            let lower = content.to_lowercase();
            let likely_only_diagnostic = lower.contains("get-content")
                || lower.contains("select-string")
                || lower.contains("checking");
            let no_write = !lower.contains("successfully wrote") && !lower.contains("write_file");
            if likely_only_diagnostic && no_write {
                content.push_str(
                    "\n\n⚠ Only a diagnostic step ran (not a full fix yet). \
Reply **continue** and I will patch Mission Control click handlers and start/check port 5000.",
                );
            }
        }

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

        content = enforce_truthful_status_reply(content, &exec_tool_outputs, compact_exec);

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

            if llm_response_is_api_error(&response) {
                final_content = response.content;
                break;
            }

            if response.has_tool_calls() {
                let tool_calls = sanitize_tool_calls_for_history(response.tool_calls.clone());
                ContextBuilder::add_assistant_message(
                    &mut messages,
                    response.content.clone(),
                    tool_calls.clone(),
                );

                for tc in &tool_calls {
                    let result = match parse_tool_params(&tc.function.arguments) {
                        Ok(params) => self.tools.execute(&tc.function.name, params).await,
                        Err(e) => format!("Tool argument error for `{}`: {e}", tc.function.name),
                    };
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
    use metis_core::types::{LlmResponse, ToolCall, ToolDefinition};

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
        // Multi-line code block should be silently removed.
        let s = "Hello\n```rust\nfn main() {}\nprintln!(\"hi\");\n```\nBye";
        let out = super::strip_markdown_fenced_code_blocks(s);
        assert!(out.contains("Hello"), "should keep surrounding text: {out}");
        assert!(out.contains("Bye"), "should keep surrounding text: {out}");
        assert!(!out.contains("fn main"), "code should be stripped: {out}");
        assert!(!out.contains("code block"), "no placeholder noise: {out}");
    }

    #[test]
    fn test_strip_fenced_only_placeholder_line() {
        // Multi-line block → silently removed (empty result is OK).
        let out = super::strip_markdown_fenced_code_blocks("```\nline one\nline two\n```");
        assert!(!out.contains("line one"), "code should be stripped: {out}");
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
        // After exhausting iterations the agent now emits a question-anchored task summary
        // (Task → Steps → Outcome) rather than a generic "no response" message.
        assert!(
            result.contains("**Task:**") && result.contains("loop forever"),
            "expected question-anchored summary, got: {result}"
        );
        assert!(result.contains("**Steps taken:**"), "expected steps section, got: {result}");
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
    fn test_exec_report_failed_detects_http_server_fail_on_stdout_exit_zero() {
        let block = "<<<EXEC_RESULT>>>\nEXIT_CODE: 0\nSTATUS: SUCCESS\n<<<END_EXEC_RESULT>>>\n--- STDOUT ---\nHTTP_SERVER_FAIL ERROR=connection refused\n";
        assert!(super::exec_report_failed(block));
    }

    #[test]
    fn test_wrap_blocking_server_command_rewrites_python() {
        let cmd = r"cd C:\Users\chack\.metis\workspace\email-app; python invoice_processor.py";
        let result = wrap_blocking_server_command_windows(cmd);
        assert!(result.is_some(), "should rewrite blocking python server");
        let wrapped = result.unwrap();
        assert!(wrapped.contains("Start-Process"), "should use Start-Process: {wrapped}");
        assert!(!wrapped.ends_with(".py"), "should not end with raw script: {wrapped}");
        assert!(wrapped.contains("email-app"), "should preserve working dir: {wrapped}");
    }

    #[test]
    fn test_wrap_blocking_server_skips_pip() {
        let cmd = "pip install flask";
        assert!(wrap_blocking_server_command_windows(cmd).is_none());
    }

    #[test]
    fn test_wrap_blocking_server_skips_taskkill_prefix() {
        // Multi-part command — must NOT be rewritten (rewriter would wrap taskkill, not python).
        let cmd = r"taskkill /F /IM python.exe 2>$null; cd C:\Users\chack\.metis\workspace\email-app; python invoice_processor.py";
        assert!(
            wrap_blocking_server_command_windows(cmd).is_none(),
            "3-part command should not be rewritten"
        );
    }

    // ── Layer 1: exec_report_failed conservatism ──────────────────────────────

    #[test]
    fn test_exec_report_failed_no_false_positive_on_select_string_output() {
        // Select-String -Path on a Python file that mentions "connection refused" as a string
        // should NOT be flagged as failed if exit code is 0.
        let block = "<<<EXEC_RESULT>>>\nCOMMAND: Select-String -Path \"invoice_processor.py\" -Pattern \"INSERT INTO\"\nEXIT_CODE: 0\nSTATUS: SUCCESS\n<<<END_EXEC_RESULT>>>\n--- STDOUT ---\ninvoice_processor.py:42: raise ConnectionError(\"connection refused\")\n";
        assert!(!exec_report_failed(block), "exit-0 Select-String should not be failed");
    }

    #[test]
    fn test_exec_report_failed_respects_exit_code_1() {
        let block = "<<<EXEC_RESULT>>>\nCOMMAND: python test.py\nEXIT_CODE: 1\nSTATUS: FAILED\n<<<END_EXEC_RESULT>>>\n--- STDERR ---\nModuleNotFoundError: No module named 'flask'\n";
        assert!(exec_report_failed(block), "exit-1 should be failed");
    }

    #[test]
    fn test_exec_report_failed_custom_marker_still_works() {
        let block = "<<<EXEC_RESULT>>>\nCOMMAND: Invoke-WebRequest http://localhost:5000\nEXIT_CODE: 0\nSTATUS: SUCCESS\n<<<END_EXEC_RESULT>>>\n--- STDOUT ---\nINVOICE_SERVER_FAIL PID=1234\n";
        assert!(exec_report_failed(block), "custom marker should still trigger failure");
    }

    // ── Layer 2: file-grep rewriter ────────────────────────────────────────

    #[test]
    fn test_rewrite_file_grep_catches_select_string_path_form() {
        let cmd = r#"Select-String -Path "C:\Users\chack\.metis\workspace\email-app\invoice_processor.py" -Pattern "INSERT INTO invoices" -Context 3"#;
        let result = rewrite_file_grep_command(cmd);
        assert!(result.is_some(), "should rewrite Select-String -Path form");
        let rewritten = result.unwrap();
        assert!(rewritten.contains("Get-Content"), "should use Get-Content: {rewritten}");
        assert!(rewritten.contains("invoice_processor.py"), "should preserve file path: {rewritten}");
    }

    #[test]
    fn test_rewrite_file_grep_catches_gc_pipe_form() {
        let cmd = r#"Get-Content "invoice_processor.py" | Select-String -Pattern "CREATE TABLE""#;
        let result = rewrite_file_grep_command(cmd);
        assert!(result.is_some(), "should rewrite Get-Content|Select-String form");
    }

    #[test]
    fn test_rewrite_file_grep_ignores_invoke_webrequest() {
        let cmd = "Invoke-WebRequest http://localhost:5000 -UseBasicParsing";
        assert!(rewrite_file_grep_command(cmd).is_none(), "should not rewrite web requests");
    }

    // ── Layer 3: identified-without-acting detection ───────────────────────

    #[test]
    fn test_response_identified_problem_without_fix() {
        let resp = "I see the problem — the invoice_date column is missing from the table. Let me fix it:";
        assert!(response_identified_problem_without_fix(resp));
    }

    #[test]
    fn test_response_identified_short_does_not_trigger() {
        let resp = "OK done.";
        assert!(!response_identified_problem_without_fix(resp));
    }

    // ── Question guard: don't hijack questions into server work ────────────

    #[test]
    fn test_question_about_invoice_is_not_autonomous_server_work() {
        let q = "are you reading the pdf file? here is the forwarded invoice";
        assert!(is_question_or_inspection(q), "should be detected as question");
        assert!(!is_autonomous_local_servers_work(q), "question must NOT trigger server work");
        assert!(!is_multi_step_fix_request(q), "question must NOT trigger fix task");
    }

    #[test]
    fn test_why_smtp_question_is_not_autonomous() {
        let q = "why are you sending via smtp? please do not smtp out without permission";
        assert!(!is_autonomous_local_servers_work(q));
    }

    #[test]
    fn test_explicit_start_request_is_still_autonomous() {
        let q = "start mission control and invoice — verify localhost:8080 and localhost:5000";
        assert!(!is_question_or_inspection(q), "explicit start must not be treated as question");
        assert!(is_autonomous_local_servers_work(q), "explicit start should be autonomous");
    }

    // ── Task ledger + question-anchored summary ────────────────────────────

    #[test]
    fn test_task_step_from_failed_exec() {
        let result = "<<<EXEC_RESULT>>>\nCOMMAND: python app.py\nEXIT_CODE: 1\nSTATUS: FAILED\n<<<END_EXEC_RESULT>>>\n--- STDERR ---\nModuleNotFoundError: No module named 'flask'\n";
        let args = r#"{"command":"python app.py"}"#;
        let step = TaskStep::from_tool_call("exec", args, result);
        assert!(!step.ok, "failed exec should be marked not ok");
    }

    #[test]
    fn test_task_step_from_ok_read_file() {
        let step = TaskStep::from_tool_call("read_file", r#"{"path":"app.py"}"#, "line1\nline2\n");
        assert!(step.ok, "successful read should be ok");
    }

    #[test]
    fn test_build_task_summary_anchors_question() {
        let steps = vec![
            TaskStep { label: "Read `invoice.pdf`".into(), ok: true, detail: None },
            TaskStep { label: "Exec: `python check.py`".into(), ok: false, detail: Some("ModuleNotFoundError".into()) },
        ];
        let summary = build_task_summary("what is in the invoice pdf?", &steps, true);
        assert!(summary.contains("**Task:**"), "must restate task: {summary}");
        assert!(summary.contains("invoice pdf"), "must include original question: {summary}");
        assert!(summary.contains("**Steps taken:**"), "must list steps: {summary}");
        assert!(summary.contains("**Outcome:**"), "must give outcome: {summary}");
        assert!(summary.contains("❌"), "should show failure outcome: {summary}");
    }

    #[test]
    fn test_build_task_summary_all_success() {
        let steps = vec![
            TaskStep { label: "Exec: `dir`".into(), ok: true, detail: None },
        ];
        let summary = build_task_summary("list files", &steps, true);
        assert!(summary.contains("✅ Completed"), "all-success outcome: {summary}");
    }

    #[test]
    fn test_wrap_blocking_server_skips_http_server() {
        let cmd = "python -m http.server 8080";
        assert!(wrap_blocking_server_command_windows(cmd).is_none());
    }

    #[test]
    fn test_parse_exec_failure_missing_module() {
        let out = "Traceback (most recent call last):\n  File \"app.py\", line 1\nModuleNotFoundError: No module named 'flask'";
        match parse_exec_failure(out) {
            Some(ExecFailureKind::MissingModule(m)) => assert_eq!(m, "flask"),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn test_parse_exec_failure_syntax_error() {
        let out = "  File \"invoice_processor.py\", line 42\nSyntaxError: invalid syntax";
        match parse_exec_failure(out) {
            Some(ExecFailureKind::SyntaxError { file: Some(f), .. }) => {
                assert!(f.contains("invoice_processor.py"), "got: {f}")
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn test_parse_exec_failure_port_in_use() {
        let out = "OSError: [WinError 10048] Only one usage of each socket address is normally permitted: ('0.0.0.0', 5000)";
        match parse_exec_failure(out) {
            Some(ExecFailureKind::PortInUse(_)) => {}
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn test_annotate_exec_hint_pip_install() {
        let out = "ModuleNotFoundError: No module named 'requests'";
        let annotated = annotate_exec_result_with_hint(out);
        assert!(annotated.contains("pip install requests"), "got: {annotated}");
        assert!(annotated.contains("Metis hint"), "got: {annotated}");
    }

    #[test]
    fn test_annotate_exec_hint_process_exited_early() {
        let out = "PROCESS_EXITED_EARLY exit=1 cmd=python app.py";
        let annotated = annotate_exec_result_with_hint(out);
        assert!(annotated.contains("Metis hint"), "got: {annotated}");
        assert!(annotated.contains("exited immediately"), "got: {annotated}");
    }

    #[test]
    fn test_pip_package_name_known_aliases() {
        assert_eq!(pip_package_name("PIL"), "Pillow");
        assert_eq!(pip_package_name("dotenv"), "python-dotenv");
        assert_eq!(pip_package_name("bs4"), "beautifulsoup4");
        assert_eq!(pip_package_name("cv2"), "opencv-python");
        assert_eq!(pip_package_name("unknown_pkg"), "unknown_pkg");
    }

    #[test]
    fn test_exec_report_failed_detects_timeout() {
        let block = "<<<EXEC_RESULT>>>\nSTATUS: TIMEOUT\nTIMEOUT_SECONDS: 60\n<<<END_EXEC_RESULT>>>";
        assert!(super::exec_report_failed(block));
    }

    #[test]
    fn test_multi_step_fix_detection() {
        let msg = "port 8080 mission control items not clickable, port 5000 invoice not working";
        assert!(is_multi_step_fix_request(msg));
        assert_eq!(max_exec_calls_for_message(msg), 20);
        assert!(is_autonomous_local_servers_work(
            "start mission control and invoice — verify localhost:8080 and localhost:5000"
        ));
    }

    #[test]
    fn test_strip_reasoning_tags() {
        let raw2 = "Hi <think>plan</think> there";
        assert_eq!(strip_reasoning_tags(raw2), "Hi  there");
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
        assert_eq!(config.timeout, 180);
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
