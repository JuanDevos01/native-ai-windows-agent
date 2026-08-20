//! Context builder — constructs the system prompt and conversation messages.
//!
//! Port of nanobot's `agent/context.py`.
//! Builds the system prompt from identity, bootstrap files, memory, and skills,
//! then assembles the full message list for an LLM call.

use std::path::PathBuf;

use chrono::Utc;
use metis_core::types::{ContentPart, ImageUrl, Message, MessageContent};
use tracing::debug;

use crate::memory::MemoryStore;
use crate::skills::SkillsLoader;

// ─────────────────────────────────────────────
// Bootstrap / identity files
// ─────────────────────────────────────────────

/// Files that are automatically injected into the system prompt when present
/// in the workspace root.
const BOOTSTRAP_FILES: &[&str] = &[
    "AGENTS.md",
    "SOUL.md",
    "USER.md",
    "TOOLS.md",
    "IDENTITY.md",
];

// ─────────────────────────────────────────────
// Context builder
// ─────────────────────────────────────────────

/// Builds system prompts and conversation message lists for the agent loop.
pub struct ContextBuilder {
    /// Root workspace directory.
    workspace: PathBuf,
    /// Agent identity name (for the system prompt).
    agent_name: String,
    /// The LLM model this agent is running on (for self-identification).
    model: String,
    /// Memory store for long-term + daily notes.
    memory: MemoryStore,
    /// Skills loader for discovering and loading skill files.
    skills: SkillsLoader,
}

impl ContextBuilder {
    /// Create a new context builder.
    pub fn new(workspace: impl Into<PathBuf>, agent_name: impl Into<String>) -> Self {
        let workspace = workspace.into();
        let memory = MemoryStore::new_lazy(&workspace);
        let skills = SkillsLoader::new(&workspace, None);
        Self {
            workspace,
            agent_name: agent_name.into(),
            model: String::new(),
            memory,
            skills,
        }
    }

    /// Set the built-in skills directory (builder pattern).
    pub fn with_builtin_skills(mut self, path: PathBuf) -> Self {
        self.skills = SkillsLoader::new(&self.workspace, Some(path));
        self
    }

    /// Set the LLM model name for self-identification (builder pattern).
    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = model.into();
        self
    }

    /// Get a reference to the memory store.
    pub fn memory(&self) -> &MemoryStore {
        &self.memory
    }

    /// Get a reference to the skills loader.
    pub fn skills(&self) -> &SkillsLoader {
        &self.skills
    }

    // ────────────── System prompt ──────────────

    /// Build the full system prompt.
    pub fn build_system_prompt(&self) -> String {
        let mut parts: Vec<String> = Vec::new();

        // 1) Identity
        parts.push(self.build_identity());

        // 2) Bootstrap files
        for filename in BOOTSTRAP_FILES {
            let path = self.workspace.join(filename);
            if path.is_file() {
                if let Ok(content) = std::fs::read_to_string(&path) {
                    debug!(file = filename, "loaded bootstrap file");
                    parts.push(format!("## {filename}\n\n{content}"));
                }
            }
        }

        // 3) Memory context (via MemoryStore)
        if let Some(memory) = self.memory.get_memory_context() {
            parts.push(memory);
        }

        // 4) Always-on skills (full body injected)
        let always_skills = self.skills.get_always_skills();
        if !always_skills.is_empty() {
            let always_content = self.skills.load_skills_for_context(&always_skills);
            if !always_content.is_empty() {
                parts.push(format!("# Active Skills\n\n{always_content}"));
            }
        }

        // 5) Skills summary (XML catalogue — agent uses read_file for on-demand loading)
        let skills_summary = self.skills.build_skills_summary();
        if !skills_summary.is_empty() {
            parts.push(format!(
                "# Skills\n\n\
                 The following skills extend your capabilities. \
                 To use a skill, read its SKILL.md file using the `read_file` tool.\n\
                 Skills with available=\"false\" need dependencies installed first.\n\n\
                 {skills_summary}"
            ));
        }

        parts.join("\n\n---\n\n")
    }

    /// Core identity block.
    fn build_identity(&self) -> String {
        let now = Utc::now().format("%Y-%m-%d %H:%M:%S UTC");
        let os = std::env::consts::OS;
        let arch = std::env::consts::ARCH;
        let workspace = self.workspace.display();
        let memory_file = self.memory.memory_file().display();
        let today = Utc::now().format("%Y-%m-%d");

        let build = metis_core::build::version_line();
        let model_line = if self.model.is_empty() {
            String::new()
        } else {
            format!("             - **Model**: `{}`\n", self.model)
        };

        format!(
            "# Identity\n\n\
             You are **{name}**, an autonomous AI assistant.\n\n\
             - **Date/time**: {now}\n\
             - **Runtime**: Rust on {os}/{arch}\n\
             - **Build**: {build}\n\
{model_line}\
             - **Workspace**: `{workspace}`\n\n\
             You have tools (read_file, write_file, edit_file, exec, web_search, and more). \
             Prefer tools over guessing, and investigate before you answer. \
             If asked which version/build or model you are running, report the Build/Model lines above — \
             do NOT guess a model name from your training.\n\n\
             ## When unsure about Metis itself\n\
             If you are unsure or doubting how Metis (you) works — your model/provider, local Ollama, \
             subagents, cron scheduling, the heartbeat, channels, or config — READ the guide at \
             `{workspace}/GUIDE.md` with read_file BEFORE answering or guessing. It is the authoritative \
             reference for your own configuration and capabilities.\n\n\
             ## Operating principles\n\
             1. **Questions vs. actions.** If the user ASKS something (why / what / how / where / is it / are you), your job is to INVESTIGATE and EXPLAIN. Do NOT modify, delete, create, or \"fix\" anything to answer a question. Only change files or run state-changing commands when the user explicitly asks you to change, fix, build, or start something. When in doubt, explain instead of acting.\n\
             2. **Never take destructive actions** (deleting code, removing functions, dropping data, killing unrelated processes) unless the user explicitly and unambiguously asked for that specific change.\n\
             3. **Understand before acting.** Read the relevant files with read_file before changing them. To inspect a source file, ALWAYS use read_file — never grep, Select-String, or Get-Content.\n\
             4. **Long-running processes.** Never run a server in the foreground (python app.py, node server.js, php artisan serve) — it blocks forever. Start it in the background, then verify it responds.\n\
             5. **Persist until done.** For a real task: form a brief plan, execute step by step, verify each step, and keep going until the task is complete or you hit a genuine blocker. If blocked, state exactly what is blocking and what the next step would be — do not silently stop.\n\
             6. **Fix bugs by editing the file.** When a script fails with an error, read the FULL error (the real message is usually the LAST line of a traceback, not the first), open the file with read_file, then use edit_file to change the file itself. Do NOT loop running the same failing command or one-off `python -c` probes without editing the file. If the same step fails twice, change your approach.\n\
             7. **Be truthful.** Report real outcomes. If a command fails (non-zero exit, error, connection refused), say it failed — never claim success or \"running\" when it is not. Never invent <<<EXEC_RESULT>>> blocks; only the exec tool emits them.\n\
             8. **Verify before answering.** For prices, specs, versions, availability, or anything else that changes over time, check a real source with the browser or web_fetch/web_search tool before answering — do not answer from training data alone, and say where the number/fact came from. If sources disagree or you could not verify, say so explicitly rather than presenting an estimate as confirmed.\n\
             9. **Searching the web.** If the `web_search` tool is not in your tool list, you have NO search engine — Google, Bing and DuckDuckGo all block automated access, and fetching them (with web_fetch OR the browser) returns a captcha or bot-block page, never results. Do not burn turns retrying different search engines. Instead go straight to a site that has the answer (a vendor's own site, docs, a retailer, Wikipedia) with the browser tool, or tell the user you cannot search and ask for a link. Say plainly that you could not search rather than presenting recalled facts as if they were looked up.
             10. **Answer the whole question.** If asked to compare or explain several things (e.g. \"A vs B vs C\"), address every one explicitly. Don't substitute the one you found the most data on for an explanation of all of them, and don't let a side task (like looking up a price) replace answering the concept actually asked about.\n\
             11. **Be concise.**\n\n\
             ## Built-in Metis capabilities\n\
             You ARE Metis — these features are built into your own binary. Use them instead of OS-specific workarounds:\n\
             - **Scheduling (cron).** To run something on a schedule or once in the future, use Metis's OWN cron via the exec tool — NOT Windows Task Scheduler / schtasks, crontab, or systemd timers. Commands (run the same `metis` binary that runs you; use its full path if `metis` is not on PATH):\n\
             &nbsp;&nbsp;• Add recurring: `metis cron add --name \"NAME\" --message \"PROMPT\" --cron \"0 9 * * *\"` (standard 5-field cron expression)\n\
             &nbsp;&nbsp;• Add interval: `metis cron add --name \"NAME\" --message \"PROMPT\" --every 3600` (seconds)\n\
             &nbsp;&nbsp;• Add one-shot: `metis cron add --name \"NAME\" --message \"PROMPT\" --at \"2026-03-01T09:00:00\"`\n\
             &nbsp;&nbsp;• Deliver result to a chat: add `--deliver --channel telegram --to <chat_id>`\n\
             &nbsp;&nbsp;• Manage: `metis cron list --all`, `metis cron run <ID>` (trigger now), `metis cron enable <ID> [--disable]`, `metis cron remove <ID>`\n\
             &nbsp;&nbsp;The built-in cron persists across restarts and runs each job as a prompt to you. Prefer it for ALL scheduling.\n\
             - **Subagents (delegation).** Use the `spawn` tool to delegate a self-contained subtask. The subagent runs its own loop and reports its result back to you. It may run on a different or local model (e.g. Ollama) when `agents.defaults.subagentModel` is set. Subagents cannot message the user directly, spawn further subagents, or edit files in place.\n\
             - **Heartbeat.** Metis wakes itself periodically (interval configurable in config) and reads `HEARTBEAT.md` in the workspace for recurring maintenance tasks.\n\n\
             ## Project notes (self-maintained)\n\
             For each project you work on, keep a markdown notes file in that project's own directory \
             named `project.md` (e.g. `{workspace}/email-app/project.md`). You discover and maintain it yourself — \
             nothing about specific projects is hardcoded. Each `project.md` should record:\n\
             - **Working directory** (absolute path)\n\
             - **How to run it** (command, port/settings, e.g. how to start the server in the background)\n\
             - **Description** (what the project does)\n\
             - **Last changes** (dated bullet list of what you changed and why)\n\
             - **TODO** (open items / next steps)\n\
             Before working on a project, read its `project.md` if it exists (use read_file). After you make changes \
             or learn something, update it with write_file/edit_file. If it does not exist yet and you are doing real \
             work on the project, create it. Keep it accurate — it is your memory of the project across sessions.\n\n\
             ## Skills (self-authored)\n\
             When you work out a non-obvious, reusable solution — a working method for a tricky site or API, \
             a multi-step CLI recipe, a fix for a flaky tool, a search engine or endpoint that actually returns \
             what a similar one blocked — call the `save_skill` tool so you don't have to rediscover it next \
             time. It writes the file and its frontmatter for you; do not hand-write skill files with \
             write_file. Record what actually worked (exact commands, URLs, selectors, gotchas), not a \
             narrative of what failed. If `save_skill` reports that the skill already exists, read that file \
             first and re-save with overwrite=true, merging in what you learned.\n\n\
             ## Memory\n\n\
             When you learn something important about the user or the project, \
             persist it by writing to `{memory_file}` using the `write_file` or `edit_file` tool.\n\
             For daily notes, write to `{workspace}/memory/{today}.md`.\n\
             If the `memory_save` tool is available, ALSO save durable facts with it — that store is \
             searchable and survives long after the conversation. Use `memory_search` when the user \
             refers to something from a past conversation that you don't see in the current context.",
            name = self.agent_name,
        )
    }

    // ────────────── Message building ──────────────

    /// Build the full message list for an LLM call.
    ///
    /// 1. System prompt
    /// 2. Session history
    /// 3. Current user message
    pub fn build_messages(
        &self,
        history: &[Message],
        user_text: &str,
        media: &[String],
        channel: &str,
        chat_id: &str,
    ) -> Vec<Message> {
        self.build_messages_with_memories(history, user_text, media, channel, chat_id, None)
    }

    /// Like [`build_messages`](Self::build_messages), but with an optional
    /// block of recalled long-term memories appended to the system prompt.
    pub fn build_messages_with_memories(
        &self,
        history: &[Message],
        user_text: &str,
        media: &[String],
        channel: &str,
        chat_id: &str,
        recalled_memories: Option<&str>,
    ) -> Vec<Message> {
        let mut messages = Vec::new();

        // System prompt + session info
        let mut system = self.build_system_prompt();
        if let Some(recalled) = recalled_memories {
            system.push_str("\n\n---\n\n");
            system.push_str(recalled);
        }
        system.push_str(&format!(
            "\n\n## Current Session\nChannel: {channel}\nChat ID: {chat_id}"
        ));
        messages.push(Message::system(system));

        // History
        messages.extend_from_slice(history);

        // Current user message
        if media.is_empty() {
            messages.push(Message::user(user_text));
        } else if model_supports_vision(&self.model) {
            messages.push(build_multimodal_user_message(user_text, media));
        } else {
            // Sending an image_url content part to a model that can't use
            // one risks either a provider-side error (dead air / no reply)
            // or the model being left staring at the "[image: <path>]" text
            // marker the channel already embedded — which reads like a file
            // reference and invites it to hallucinate a browse/fetch attempt
            // rather than admitting it can't see the image. Say so directly.
            let images: Vec<&String> = media.iter().filter(|p| !is_audio_extension(p)).collect();
            let note = if images.is_empty() {
                String::new()
            } else {
                let paths = images
                    .iter()
                    .map(|p| format!("  - {p}"))
                    .collect::<Vec<_>>()
                    .join("\n");
                format!(
                    "\n\n[{n} image(s) attached. You ({model}) cannot see images directly, but you \
                     CAN read them: call the `analyze_image` tool with the path below, then answer \
                     the user's question from what it returns. Do not open the path with read_file \
                     or the browser, and never guess the contents. If analyze_image fails, say what \
                     failed.]\n{paths}",
                    n = images.len(),
                    model = self.model,
                )
            };
            messages.push(Message::user(format!("{user_text}{note}")));
        }

        messages
    }

    /// Build a minimal message list for chat-only models (direct chat mode).
    ///
    /// Small local models pay minutes of CPU prompt-evaluation for the full
    /// agent briefing, and can't use any of it (no tool support). This sends
    /// a ~200-token prompt instead: short identity, memory, recalled facts.
    pub fn build_messages_lite(
        &self,
        history: &[Message],
        user_text: &str,
        media: &[String],
        channel: &str,
        chat_id: &str,
        recalled_memories: Option<&str>,
        // Context window the local model is loaded with, and the response
        // budget reserved out of it. When set, history is trimmed so
        // system + history + current message + response fit inside the
        // window — overflowing it breaks the server's prefix cache on every
        // turn and re-evaluates the whole conversation per message.
        num_ctx: Option<u32>,
        max_response_tokens: u32,
    ) -> Vec<Message> {
        // The system prompt must stay byte-identical across turns: local
        // servers (Ollama/llama.cpp) reuse their KV/prefix cache only for an
        // unchanged prompt prefix. Anything per-turn (like recalled memories)
        // goes into the CURRENT user message instead, so each turn only
        // evaluates the new tokens rather than the whole conversation.
        let mut system = format!(
            "You are {name}, a helpful personal AI assistant. Reply directly, in the user's \
             language.\n\n\
             You are running in DIRECT CHAT MODE: tool integrations are disabled, so you \
             cannot execute commands, read or write files, browse the web, or schedule \
             tasks. This does not limit what you can write — you can still produce and \
             explain code in any programming language, translate, summarize, and give \
             detailed answers from your own knowledge. Only when a request requires \
             actually executing something, say that tools are disabled and suggest \
             switching to a tool-capable model.",
            name = self.agent_name,
        );
        if let Some(memory) = self.memory.get_memory_context() {
            system.push_str("\n\n");
            system.push_str(&memory);
        }
        system.push_str(&format!(
            "\n\n## Current Session\nChannel: {channel}\nChat ID: {chat_id}"
        ));

        let user_text_final = lite_user_text(user_text, recalled_memories);

        let history = match num_ctx {
            Some(ctx) => {
                // Reserve room for everything that isn't history, plus a
                // safety margin for the chat template and estimation error.
                let fixed = estimate_text_tokens(&system)
                    + estimate_text_tokens(&user_text_final)
                    + media.len() * IMAGE_TOKEN_ESTIMATE
                    + max_response_tokens as usize
                    + 256;
                let budget = (ctx as usize).saturating_sub(fixed);
                trim_history_to_token_budget(history, budget)
            }
            None => history,
        };

        let mut messages = vec![Message::system(system)];
        messages.extend_from_slice(history);
        if media.is_empty() {
            messages.push(Message::user(user_text_final));
        } else if model_supports_vision(&self.model) {
            messages.push(build_multimodal_user_message(&user_text_final, media));
        } else {
            let image_count = media.iter().filter(|p| !is_audio_extension(p)).count();
            let note = if image_count > 0 {
                format!(
                    "\n\n[{image_count} image(s) were attached, but the current model ({model}) \
                     does not support vision — you cannot see their contents. Say so directly \
                     rather than guessing what the image shows.]",
                    model = self.model,
                )
            } else {
                String::new()
            };
            messages.push(Message::user(format!("{user_text_final}{note}")));
        }
        messages
    }

    /// Add a tool result to the message list (convenience wrapper).
    pub fn add_tool_result(messages: &mut Vec<Message>, tool_call_id: &str, result: &str) {
        messages.push(Message::tool_result(tool_call_id, result));
    }

    /// Add an assistant message (with optional tool calls) to the message list.
    pub fn add_assistant_message(
        messages: &mut Vec<Message>,
        content: Option<String>,
        tool_calls: Vec<metis_core::types::ToolCall>,
    ) {
        if tool_calls.is_empty() {
            if let Some(text) = content {
                messages.push(Message::assistant(text));
            }
        } else {
            messages.push(Message::assistant_tool_calls(tool_calls));
        }
    }
}

// ─────────────────────────────────────────────
// Multimodal helpers
// ─────────────────────────────────────────────

/// The exact user-message text sent to the model in lite mode (recall block
/// prepended, when present). Public so the agent loop can persist these same
/// bytes in the session: the next turn's history must be byte-identical to
/// the prompt the local server cached, or its prefix cache misses and the
/// whole conversation is re-evaluated.
pub fn lite_user_text(user_text: &str, recalled_memories: Option<&str>) -> String {
    match recalled_memories {
        Some(recalled) => format!("{recalled}\n---\n{user_text}"),
        None => user_text.to_string(),
    }
}

// ─────────────────────────────────────────────
// Token budgeting (local models)
// ─────────────────────────────────────────────

/// Rough per-image token cost (vision models encode an image as a few hundred
/// tokens; overestimating is the safe direction for budget math).
const IMAGE_TOKEN_ESTIMATE: usize = 512;

/// Per-message overhead of the chat template (role markers, separators).
const MESSAGE_TOKEN_OVERHEAD: usize = 8;

/// Estimate token count from text length (≈4 chars/token for mixed content).
fn estimate_text_tokens(text: &str) -> usize {
    text.len() / 4
}

/// Estimate the token cost of one message in a chat prompt.
pub fn estimate_message_tokens(msg: &Message) -> usize {
    let text_tokens = match msg {
        Message::System { content } => estimate_text_tokens(content),
        Message::User { content } => match content {
            MessageContent::Text(t) => estimate_text_tokens(t),
            MessageContent::Parts(parts) => parts
                .iter()
                .map(|p| match p {
                    ContentPart::Text { text } => estimate_text_tokens(text),
                    ContentPart::ImageUrl { .. } => IMAGE_TOKEN_ESTIMATE,
                })
                .sum(),
        },
        Message::Assistant {
            content,
            reasoning_content,
            ..
        } => {
            estimate_text_tokens(content.as_deref().unwrap_or(""))
                + estimate_text_tokens(reasoning_content.as_deref().unwrap_or(""))
        }
        Message::Tool { content, .. } => estimate_text_tokens(content),
    };
    MESSAGE_TOKEN_OVERHEAD + text_tokens
}

/// Keep the newest messages that fit in `budget_tokens`, trimming whole
/// messages from the oldest end.
///
/// Trims with stateless hysteresis: the history overflows, so it is cut to
/// ~70% of the budget, and the number of dropped tokens is rounded up to a
/// multiple of a fixed chunk. Re-running on a slightly longer history then
/// yields the SAME cut point, so the window start stays put until roughly a
/// chunk's worth of new conversation arrives. A window that slid every turn
/// would invalidate the local server's prefix cache on every message; one
/// that jumps occasionally pays the re-evaluation once per jump.
fn trim_history_to_token_budget(history: &[Message], budget_tokens: usize) -> &[Message] {
    let budget_tokens = budget_tokens.max(1);
    let total: usize = history.iter().map(estimate_message_tokens).sum();
    if total <= budget_tokens {
        return history;
    }
    let target = budget_tokens * 7 / 10;
    let chunk = (budget_tokens * 3 / 10).max(1);
    let excess = total - target;
    let drop_tokens = excess.div_ceil(chunk) * chunk;

    let mut dropped_tokens = 0usize;
    let mut start = 0usize;
    while start < history.len() && dropped_tokens < drop_tokens {
        dropped_tokens += estimate_message_tokens(&history[start]);
        start += 1;
    }
    // Don't open the window on an assistant reply whose question was dropped.
    while start < history.len() && !matches!(history[start], Message::User { .. }) {
        start += 1;
    }
    debug!(
        dropped = start,
        kept = history.len() - start,
        kept_tokens = total.saturating_sub(dropped_tokens),
        budget_tokens,
        "trimmed direct-chat history to context budget"
    );
    &history[start..]
}

/// Known vision-capable model name fragments (checked case-insensitively as
/// substrings). No provider exposes a real capability flag for this the way
/// `supports_tools` does for tool-calling, so this is a best-effort
/// allowlist — deliberately conservative: an unrecognized model is treated
/// as text-only, since assuming vision support that isn't there is the
/// failure mode this function exists to prevent (a provider error or a
/// confused model hallucinating about a file path it can't actually see).
/// Update this list as new vision models are adopted.
const VISION_MODEL_HINTS: &[&str] = &[
    "gpt-4o",
    "gpt-4-vision",
    "gpt-4-turbo",
    "gpt-4.1",
    "gpt-5",
    "o1",
    "o3",
    "o4",
    "claude-3",
    "claude-4",
    "claude-5",
    "claude-sonnet",
    "claude-opus",
    "claude-haiku",
    "gemini",
    "qwen-vl",
    "qwen2-vl",
    "qwen2.5-vl",
    "llava",
    "pixtral",
    "grok-2",
    "grok-3",
    "grok-4",
    "glm-4v",
    "internvl",
    "yi-vl",
    "phi-3-vision",
    "phi-3.5-vision",
    "cogvlm",
    "moondream",
    // MiniMax splits by version: M2.5 is natively multimodal, M2.7 is
    // text-only. Matched at full version precision so "minimax-m2.7" cannot
    // match — a false positive here would send images to a model that
    // rejects them.
    "minimax-m2.5",
    "minimax-vl",
];

/// Best-effort check for whether `model` can accept image content parts.
fn model_supports_vision(model: &str) -> bool {
    let lower = model.to_lowercase();
    VISION_MODEL_HINTS.iter().any(|hint| lower.contains(hint))
}

/// Build a user message with base64-encoded images.
///
/// Audio files are skipped — their transcription is already in the text content.
fn build_multimodal_user_message(text: &str, media_paths: &[String]) -> Message {
    let mut parts = Vec::new();

    for path in media_paths {
        // Skip audio files — transcription text is already in `content`
        if is_audio_extension(path) {
            continue;
        }
        if let Ok(data) = std::fs::read(path) {
            let mime = guess_mime(path);
            let b64 = base64_encode(&data);
            parts.push(ContentPart::ImageUrl {
                image_url: ImageUrl {
                    url: format!("data:{mime};base64,{b64}"),
                    detail: None,
                },
            });
        }
    }

    parts.push(ContentPart::Text {
        text: text.to_string(),
    });

    Message::user_parts(parts)
}

/// Check if a file path has an audio extension.
fn is_audio_extension(path: &str) -> bool {
    let lower = path.to_lowercase();
    lower.ends_with(".ogg")
        || lower.ends_with(".oga")
        || lower.ends_with(".opus")
        || lower.ends_with(".mp3")
        || lower.ends_with(".m4a")
        || lower.ends_with(".wav")
        || lower.ends_with(".flac")
        || lower.ends_with(".aac")
        || lower.ends_with(".wma")
        || lower.ends_with(".webm")
}

/// Simple MIME guesser based on extension.
fn guess_mime(path: &str) -> &str {
    let lower = path.to_lowercase();
    if lower.ends_with(".png") {
        "image/png"
    } else if lower.ends_with(".gif") {
        "image/gif"
    } else if lower.ends_with(".webp") {
        "image/webp"
    } else if lower.ends_with(".svg") {
        "image/svg+xml"
    } else {
        "image/jpeg"
    }
}

/// Base64-encode bytes (no padding issues, uses standard alphabet).
fn base64_encode(data: &[u8]) -> String {
    use std::io::Write;
    // Simple base64 encoder without external dependency
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = Vec::with_capacity((data.len() + 2) / 3 * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = if chunk.len() > 1 { chunk[1] as u32 } else { 0 };
        let b2 = if chunk.len() > 2 { chunk[2] as u32 } else { 0 };
        let triple = (b0 << 16) | (b1 << 8) | b2;
        out.push(ALPHABET[((triple >> 18) & 0x3F) as usize]);
        out.push(ALPHABET[((triple >> 12) & 0x3F) as usize]);
        if chunk.len() > 1 {
            out.push(ALPHABET[((triple >> 6) & 0x3F) as usize]);
        } else {
            out.push(b'=');
        }
        if chunk.len() > 2 {
            out.push(ALPHABET[(triple & 0x3F) as usize]);
        } else {
            out.push(b'=');
        }
    }
    let _ = out.flush();
    String::from_utf8(out).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_guess_mime() {
        assert_eq!(guess_mime("photo.png"), "image/png");
        assert_eq!(guess_mime("photo.PNG"), "image/png");
        assert_eq!(guess_mime("photo.jpg"), "image/jpeg");
        assert_eq!(guess_mime("photo.gif"), "image/gif");
        assert_eq!(guess_mime("photo.webp"), "image/webp");
        assert_eq!(guess_mime("photo.unknown"), "image/jpeg");
    }

    #[test]
    fn test_base64_encode() {
        assert_eq!(base64_encode(b"Hello"), "SGVsbG8=");
        assert_eq!(base64_encode(b"Hi"), "SGk=");
        assert_eq!(base64_encode(b"ABC"), "QUJD");
        assert_eq!(base64_encode(b""), "");
    }

    #[test]
    fn test_build_identity() {
        let ctx = ContextBuilder::new("/tmp/workspace", "TestBot");
        let identity = ctx.build_identity();
        assert!(identity.contains("TestBot"));
        assert!(identity.contains("/tmp/workspace"));
        assert!(identity.contains("Rust on"));
    }

    #[test]
    fn test_build_system_prompt_no_files() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ContextBuilder::new(dir.path(), "Metis");
        let prompt = ctx.build_system_prompt();
        assert!(prompt.contains("Metis"));
        // No bootstrap files → no "---" separator for them
    }

    #[test]
    fn test_build_system_prompt_with_bootstrap() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("AGENTS.md"), "# Agent config\nBe helpful.").unwrap();
        let ctx = ContextBuilder::new(dir.path(), "Metis");
        let prompt = ctx.build_system_prompt();
        assert!(prompt.contains("Be helpful."));
        assert!(prompt.contains("## AGENTS.md"));
    }

    #[test]
    fn test_build_system_prompt_with_memory() {
        let dir = tempfile::tempdir().unwrap();
        let mem_dir = dir.path().join("memory");
        std::fs::create_dir(&mem_dir).unwrap();
        std::fs::write(mem_dir.join("MEMORY.md"), "User prefers dark mode.").unwrap();
        let ctx = ContextBuilder::new(dir.path(), "Metis");
        let prompt = ctx.build_system_prompt();
        assert!(prompt.contains("User prefers dark mode."));
        assert!(prompt.contains("Long-term Memory"));
    }

    #[test]
    fn test_build_messages_text_only() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ContextBuilder::new(dir.path(), "Metis");
        let history = vec![
            Message::user("previous question"),
            Message::assistant("previous answer"),
        ];
        let msgs = ctx.build_messages(&history, "new question", &[], "cli", "direct");
        // system + 2 history + 1 user = 4
        assert_eq!(msgs.len(), 4);
    }

    #[test]
    fn test_build_messages_with_session_info() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ContextBuilder::new(dir.path(), "Metis");
        let msgs = ctx.build_messages(&[], "hello", &[], "telegram", "chat_42");
        // The system message should contain channel/chat info
        if let Message::System { content } = &msgs[0] {
            assert!(content.contains("Channel: telegram"));
            assert!(content.contains("Chat ID: chat_42"));
        } else {
            panic!("First message should be System");
        }
    }

    #[test]
    fn test_build_messages_with_memories() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ContextBuilder::new(dir.path(), "Metis");
        let recalled = "# Recalled Memories\n- [2026-07-06] User prefers Rust.";
        let msgs = ctx.build_messages_with_memories(&[], "hi", &[], "cli", "1", Some(recalled));
        if let Message::System { content } = &msgs[0] {
            assert!(content.contains("Recalled Memories"));
            assert!(content.contains("User prefers Rust."));
            // Session info must still come after the recalled block
            assert!(content.contains("Channel: cli"));
        } else {
            panic!("First message should be System");
        }
        // No recall → no block
        let msgs = ctx.build_messages(&[], "hi", &[], "cli", "1");
        if let Message::System { content } = &msgs[0] {
            assert!(!content.contains("Recalled Memories"));
        }
    }

    #[test]
    fn test_model_supports_vision() {
        assert!(model_supports_vision("gpt-4o"));
        assert!(model_supports_vision("openai/gpt-4o-mini"));
        assert!(model_supports_vision("claude-sonnet-5"));
        assert!(model_supports_vision("gemini-1.5-pro"));
        assert!(model_supports_vision("Qwen2.5-VL-72B"));
        // MiniMax version split: M2.5 is multimodal, M2.7 is text-only.
        // Substring matching must not let one leak into the other.
        assert!(model_supports_vision("MiniMax-M2.5"));
        assert!(model_supports_vision("minimax/MiniMax-M2.5-highspeed"));
        assert!(!model_supports_vision("MiniMax-M2.7"));
        assert!(!model_supports_vision("minimax/MiniMax-M2.7"));
        assert!(!model_supports_vision("ollama/gemma3:4b"));
        assert!(!model_supports_vision(""));
        assert!(!model_supports_vision("deepseek-chat"));
    }

    #[test]
    fn test_build_messages_with_image_on_non_vision_model_points_at_analyze_image() {
        let dir = tempfile::tempdir().unwrap();
        let img_path = dir.path().join("photo.jpg");
        std::fs::write(&img_path, b"fake-jpeg-bytes").unwrap();
        let ctx = ContextBuilder::new(dir.path(), "Metis").with_model("MiniMax-M2.7");
        let media = vec![img_path.to_str().unwrap().to_string()];
        let msgs = ctx.build_messages(&[], "what does it say?", &media, "telegram", "chat_1");
        let last = msgs.last().unwrap();
        match last {
            Message::User { content: MessageContent::Text(t) } => {
                // The model must be routed to the tool, and given the path.
                assert!(t.contains("analyze_image"), "got: {t}");
                assert!(t.contains(img_path.to_str().unwrap()), "path must be included: {t}");
                assert!(t.contains("what does it say?"));
                // It must not be told to read the image as a file.
                assert!(!t.contains("switching models"));
            }
            other => panic!("expected a plain text user message for a non-vision model, got {other:?}"),
        }
    }

    #[test]
    fn test_build_messages_with_image_on_vision_model_embeds_image_part() {
        let dir = tempfile::tempdir().unwrap();
        let img_path = dir.path().join("photo.png");
        std::fs::write(&img_path, b"fake-png-bytes").unwrap();
        let ctx = ContextBuilder::new(dir.path(), "Metis").with_model("gpt-4o");
        let media = vec![img_path.to_str().unwrap().to_string()];
        let msgs = ctx.build_messages(&[], "what is this?", &media, "telegram", "chat_1");
        let last = msgs.last().unwrap();
        match last {
            Message::User { content: MessageContent::Parts(parts) } => {
                assert!(parts.iter().any(|p| matches!(p, ContentPart::ImageUrl { .. })));
            }
            other => panic!("expected a multimodal user message for a vision model, got {other:?}"),
        }
    }

    #[test]
    fn test_build_messages_lite_cache_friendly_layout() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ContextBuilder::new(dir.path(), "Metis");
        let recalled = "# Recalled Memories\n- [2026-07-06] User's cat is Miso.";
        let history = vec![Message::user("earlier"), Message::assistant("earlier reply")];
        let msgs = ctx.build_messages_lite(&history, "hi again", &[], "desktop", "1", Some(recalled), None, 1024);

        // System prompt: lite, and byte-stable — recall must NOT be in it.
        let Message::System { content: system } = &msgs[0] else {
            panic!("first message must be system");
        };
        assert!(system.contains("DIRECT CHAT MODE"));
        assert!(!system.contains("Recalled Memories"));
        assert!(!system.contains("## Operating principles"));

        // Recall rides in the final user message, after unchanged history.
        assert_eq!(msgs.len(), 4);
        use metis_core::types::MessageContent;
        let Message::User { content: MessageContent::Text(last) } = &msgs[3] else {
            panic!("last message must be user text");
        };
        assert!(last.contains("Miso"));
        assert!(last.ends_with("hi again"));

        // Identical inputs → identical system prompt (prefix-cache friendly).
        let msgs2 = ctx.build_messages_lite(&history, "different msg", &[], "desktop", "1", None, None, 1024);
        let Message::System { content: system2 } = &msgs2[0] else {
            panic!("first message must be system");
        };
        assert_eq!(system, system2);
    }

    #[test]
    fn test_build_messages_lite_token_budget_trims_history() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ContextBuilder::new(dir.path(), "Metis");
        // 20 messages ≈ 1000 tokens each ≈ 20k tokens — far over an 8k window.
        let big = "x".repeat(4000);
        let history: Vec<Message> = (0..10)
            .flat_map(|_| [Message::user(big.clone()), Message::assistant(big.clone())])
            .collect();

        let msgs = ctx.build_messages_lite(&history, "hi", &[], "cli", "1", None, Some(8192), 1024);
        let kept = msgs.len() - 2; // minus system and current user message
        assert!(kept < history.len(), "history must be trimmed");
        assert!(kept >= 2, "recent messages must survive");
        // Window must open on a user message, not an orphaned assistant reply.
        assert!(matches!(msgs[1], Message::User { .. }));
        // Newest history message is still the one right before the current text.
        assert_eq!(msgs[msgs.len() - 2], history[history.len() - 1]);

        // Without a context window, nothing is trimmed.
        let full = ctx.build_messages_lite(&history, "hi", &[], "cli", "1", None, None, 1024);
        assert_eq!(full.len(), history.len() + 2);

        // A small conversation inside the budget is untouched.
        let small = vec![Message::user("q"), Message::assistant("a")];
        let msgs = ctx.build_messages_lite(&small, "hi", &[], "cli", "1", None, Some(8192), 1024);
        assert_eq!(msgs.len(), small.len() + 2);
    }

    #[test]
    fn test_trim_hysteresis_keeps_window_start_stable() {
        // After a trim, appending a small message must NOT move the window
        // start (a start that slides every turn would break the server's
        // prefix cache on every message).
        let big = "x".repeat(2000); // ~500 tokens
        let mut history: Vec<Message> = (0..20)
            .flat_map(|_| [Message::user(big.clone()), Message::assistant(big.clone())])
            .collect();
        let budget = 4000;
        let trimmed = trim_history_to_token_budget(&history, budget);
        let dropped = history.len() - trimmed.len();
        assert!(dropped > 0);
        assert!(dropped < history.len());

        history.push(Message::user("short follow-up"));
        let trimmed2 = trim_history_to_token_budget(&history, budget);
        let dropped2 = history.len() - trimmed2.len();
        assert_eq!(
            dropped, dropped2,
            "cut point must stay put until a chunk's worth of new tokens arrives"
        );

        // The cut point never moves backwards as history grows.
        history.push(Message::assistant(big.clone()));
        history.push(Message::user(big.clone()));
        let trimmed3 = trim_history_to_token_budget(&history, budget);
        let dropped3 = history.len() - trimmed3.len();
        assert!(dropped3 >= dropped);
    }

    #[test]
    fn test_add_tool_result() {
        let mut msgs = vec![Message::user("test")];
        ContextBuilder::add_tool_result(&mut msgs, "call_1", "result data");
        assert_eq!(msgs.len(), 2);
    }

    #[test]
    fn test_add_assistant_message_text() {
        let mut msgs = Vec::new();
        ContextBuilder::add_assistant_message(&mut msgs, Some("hello".into()), vec![]);
        assert_eq!(msgs.len(), 1);
    }

    #[test]
    fn test_add_assistant_message_tool_calls() {
        use metis_core::types::ToolCall;
        let mut msgs = Vec::new();
        let tc = ToolCall::new("id1", "read_file", r#"{"path":"foo"}"#);
        ContextBuilder::add_assistant_message(&mut msgs, None, vec![tc]);
        assert_eq!(msgs.len(), 1);
    }
}
