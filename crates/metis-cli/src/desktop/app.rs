//! egui mission-control desktop application.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;
use eframe::egui;
use metis_agent::AgentLoop;
use metis_core::config::{load_config, save_config};
use metis_core::session::{SessionManager, SessionSummary};
use metis_providers::registry::match_provider;
use metis_providers::DiscoveredModel;
use metis_core::types::{Message, MessageContent};
use metis_cron::types::{CronJob, CronStore};
use metis_core::utils::get_data_path;
use tokio::runtime::Runtime;
use tokio::sync::oneshot;

use super::config::{desktop_config_path, load_desktop_config, save_desktop_config, DesktopConfig};
use crate::agent_builder::build_agent_loop;

const ACCENT: egui::Color32 = egui::Color32::from_rgb(0, 102, 204);
const SIDEBAR_BG: egui::Color32 = egui::Color32::from_rgb(248, 249, 251);
const MAIN_BG: egui::Color32 = egui::Color32::from_rgb(255, 255, 255);

#[derive(Clone, Copy, PartialEq, Eq)]
enum NavPanel {
    Chat,
    Settings,
    Approvals,
    Models,
    SkillsTools,
    Messaging,
    Artifacts,
    CronJobs,
}

#[derive(Default)]
struct ChatLine {
    role: &'static str,
    text: String,
    /// What the model reasoned during this turn (all agents), for
    /// troubleshooting. Shown collapsed under the reply.
    thinking: Option<String>,
    /// One line per tool call / hand-off, from the event log.
    steps: Vec<String>,
    /// Metis's own account of the turn (the "What actually happened" lines),
    /// split off the reply so it can be shown apart from the agent's words.
    record: Option<Vec<String>>,
    /// What the person asked for in this turn.
    asked: Option<String>,
}

/// Result of a background model-discovery sweep across configured providers.
struct ModelSweep {
    models: Vec<DiscoveredModel>,
    /// One line per provider that could not be queried (bad key, offline, …).
    errors: Vec<String>,
}

struct PendingReply {
    session_key: String,
    rx: oneshot::Receiver<Result<String, String>>,
}

pub struct MetisDesktopApp {
    config: DesktopConfig,
    agent: Arc<AgentLoop>,
    sessions: Arc<SessionManager>,
    runtime: Runtime,
    nav: NavPanel,
    session_search: String,
    active_session: String,
    chat_lines: Vec<ChatLine>,
    input: String,
    /// Files queued for the next message (picked with "+" or dropped on the window).
    attachments: Vec<std::path::PathBuf>,
    /// Steps of the turn in progress, read live from the event log.
    live_steps: Vec<String>,
    /// The model's latest reasoning in the turn in progress.
    live_thinking: Option<String>,
    live_last_load: f64,
    /// Ctrl+V edge detection for image-only clipboards (see `paste_shortcut`).
    paste_key_down: bool,
    pending: Option<PendingReply>,
    status_line: String,
    cron_jobs: Vec<CronJob>,
    sessions_cache: Vec<SessionSummary>,
    last_refresh: f64,
    // ── Models panel ──
    /// Main model field (editable, applied on "Apply & save").
    model_main: String,
    /// Subagent model field (empty = same as main).
    model_sub: String,
    /// Status/result line for the Models panel.
    model_status: String,
    /// Models offered by every configured provider (from `config.json`).
    available_models: Vec<DiscoveredModel>,
    /// Providers that could not be queried in the last sweep.
    model_errors: Vec<String>,
    /// In-flight model discovery sweep.
    models_rx: Option<oneshot::Receiver<ModelSweep>>,
    /// In-flight model health test: Ok((reply, seconds)) or Err(reason).
    model_test_rx: Option<oneshot::Receiver<Result<(String, f64), String>>>,
    // ── Settings panel ──
    /// Editable mirror of config.json.
    settings: super::settings::SettingsForm,
    /// Which secret fields are currently revealed (ids, not values).
    settings_reveal: std::collections::HashSet<String>,
    /// Result of the last save/reload.
    settings_status: String,
    /// Shown once when the install has never been configured.
    show_first_run: bool,
    // ── Approvals panel ──
    /// Replies held for approval, refreshed from disk.
    approvals: Vec<metis_core::approvals::PendingReply>,
    /// Draft being edited, keyed by entry id.
    approval_edits: std::collections::HashMap<String, String>,
    approval_status: String,
    approvals_last_load: f64,
    /// In-flight email diagnostic (connection test / test send).
    email_diag_rx: Option<oneshot::Receiver<Result<String, String>>>,
    /// Destructive-change requests from the agent, waiting for the person.
    tool_approval_rx: tokio::sync::mpsc::UnboundedReceiver<super::approval_ui::ApprovalAsk>,
    /// The request currently on screen.
    tool_approval: Option<super::approval_ui::ApprovalAsk>,
}


const MAX_ATTACHMENTS: usize = 10;
/// Text files up to this size are pasted into the message so even a
/// tool-less model can read them; larger ones are passed by path.
const INLINE_TEXT_MAX_BYTES: u64 = 24 * 1024;

/// Write a clipboard image to `<data>/media/paste-<time>.png`.
fn save_clipboard_png(image: &arboard::ImageData) -> Result<std::path::PathBuf> {
    let dir = get_data_path().join("media");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!("paste-{}.png", chrono::Local::now().format("%Y%m%d-%H%M%S-%3f")));
    let file = std::io::BufWriter::new(std::fs::File::create(&path)?);
    let mut enc = png::Encoder::new(file, image.width as u32, image.height as u32);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    enc.write_header()?.write_image_data(&image.bytes)?;
    Ok(path)
}

fn is_image_file(p: &std::path::Path) -> bool {
    matches!(
        p.extension().and_then(|e| e.to_str()).map(|e| e.to_ascii_lowercase()).as_deref(),
        Some("png" | "jpg" | "jpeg" | "gif" | "webp")
    )
}

/// Build the message text for a turn with attachments. Returns the text and
/// the image paths (those travel as image parts; everything else is described
/// in the text so the agent opens it with its file tools).
fn compose_with_attachments(typed: &str, files: &[std::path::PathBuf]) -> (String, Vec<String>) {
    let mut text = if typed.is_empty() && !files.is_empty() {
        "Please look at the attached file(s).".to_string()
    } else {
        typed.to_string()
    };
    let mut images = Vec::new();
    for p in files {
        let path = p.display().to_string();
        if is_image_file(p) {
            text.push_str(&format!("\n\n[image: {path}]"));
            images.push(path);
            continue;
        }
        let small = std::fs::metadata(p).map(|m| m.len() <= INLINE_TEXT_MAX_BYTES).unwrap_or(false);
        let inline = if small { std::fs::read_to_string(p).ok() } else { None };
        match inline {
            Some(body) if !body.contains('\0') => {
                text.push_str(&format!("\n\n[file: {path}]\n```\n{body}\n```"));
            }
            _ => text.push_str(&format!("\n\n[file: {path}] (open it with your file tools)")),
        }
    }
    (text, images)
}

/// True when `pwsh` (PowerShell 7+) is on PATH.
fn which_pwsh() -> bool {
    let exe = if cfg!(windows) { "pwsh.exe" } else { "pwsh" };
    std::env::var("PATH")
        .map(|path| std::env::split_paths(&path).any(|d| d.join(exe).is_file()))
        .unwrap_or(false)
}

impl MetisDesktopApp {
    fn new(
        config: DesktopConfig,
        agent: Arc<AgentLoop>,
        sessions: Arc<SessionManager>,
        runtime: Runtime,
        tool_approval_rx: tokio::sync::mpsc::UnboundedReceiver<super::approval_ui::ApprovalAsk>,
    ) -> Self {
        let active = format!(
            "desktop:{}",
            chrono::Utc::now().format("%Y%m%d-%H%M%S")
        );
        let agent_config = load_config(None);
        let first_run = super::settings::is_first_run(&agent_config);
        let mut app = Self {
            settings: super::settings::SettingsForm::from_config(&agent_config),
            settings_reveal: std::collections::HashSet::new(),
            settings_status: String::new(),
            show_first_run: first_run,
            approvals: Vec::new(),
            approval_edits: std::collections::HashMap::new(),
            approval_status: String::new(),
            approvals_last_load: 0.0,
            email_diag_rx: None,
            tool_approval_rx,
            tool_approval: None,
            config,
            agent,
            sessions,
            runtime,
            nav: NavPanel::Chat,
            session_search: String::new(),
            active_session: active,
            chat_lines: Vec::new(),
            input: String::new(),
            attachments: Vec::new(),
            live_steps: Vec::new(),
            live_thinking: None,
            live_last_load: 0.0,
            paste_key_down: false,
            pending: None,
            status_line: String::new(),
            cron_jobs: load_cron_jobs(),
            sessions_cache: Vec::new(),
            last_refresh: 0.0,
            model_main: agent_config.agents.defaults.model.clone(),
            model_sub: agent_config.agents.defaults.subagent_model.clone(),
            model_status: String::new(),
            available_models: Vec::new(),
            model_errors: Vec::new(),
            models_rx: None,
            model_test_rx: None,
        };
        app.refresh_sessions();
        app.load_session_history();
        // Populate the model dropdowns from every configured provider (non-blocking).
        app.discover_models();
        app
    }

    /// Candidate models for the dropdowns as `(model id, display label)`:
    /// active model, configured main/sub, recently used, then every model
    /// offered by a configured provider — deduplicated, in that order.
    /// Models known to lack tool support are labelled "(chat-only)".
    fn model_choices(&self) -> Vec<(String, String)> {
        let mut out: Vec<(String, String)> = Vec::new();
        let add = |out: &mut Vec<(String, String)>, id: &str| {
            let id = id.trim();
            if !id.is_empty() && !out.iter().any(|(x, _)| x == id) {
                out.push((id.to_string(), self.model_label(id)));
            }
        };
        add(&mut out, self.agent.model());
        add(&mut out, &self.model_main);
        add(&mut out, &self.model_sub);
        for m in &self.config.recent_models {
            add(&mut out, m);
        }
        for m in &self.available_models {
            add(&mut out, &m.id);
        }
        out
    }

    /// Display label for a model id: `<id> · <Provider>` plus a chat-only
    /// marker when the provider reported no tool support.
    fn model_label(&self, id: &str) -> String {
        match self.available_models.iter().find(|m| m.id == id) {
            Some(m) if m.is_chat_only() => {
                format!("{id} · {} (chat-only)", m.provider_display)
            }
            Some(m) => format!("{id} · {}", m.provider_display),
            None => id.to_string(),
        }
    }

    /// Render a model dropdown; returns the newly picked id, if any.
    /// The selected entry is marked with a checkmark.
    fn model_combo(
        ui: &mut egui::Ui,
        id_salt: &str,
        current: &str,
        choices: &[(String, String)],
        width: f32,
    ) -> Option<String> {
        let mut picked = None;
        let selected_text = if current.trim().is_empty() {
            "(same as main)".to_string()
        } else {
            truncate(current, 40)
        };
        egui::ComboBox::from_id_salt(id_salt)
            .selected_text(selected_text)
            .width(width)
            .show_ui(ui, |ui| {
                for (id, label) in choices {
                    let is_selected = id == current;
                    let text = if is_selected {
                        format!("✓ {label}")
                    } else {
                        format!("   {label}")
                    };
                    if ui.selectable_label(is_selected, text).clicked() && !is_selected {
                        picked = Some(id.clone());
                    }
                }
            });
        picked
    }

    /// Switch the model the chat talks to, for this app session only — no
    /// config.json write, so the saved default is untouched. Changing the
    /// default lives in the Models panel (`apply_models`).
    fn switch_chat_model(&mut self, model: String) {
        if self.pending.is_some() {
            self.status_line = "⏳ Wait for the current reply to finish first.".into();
            return;
        }
        let model = model.trim().to_string();
        if model.is_empty() {
            return;
        }
        let mut agent_config = load_config(None);
        let providers = agent_config.providers.to_map();
        if match_provider(&model, &providers).is_none() {
            self.status_line = format!(
                "✗ No configured provider for '{model}'. Add its API key via `metis onboard`."
            );
            return;
        }
        // In-memory override only: the agent is rebuilt with this model, but
        // the config on disk keeps its saved default.
        agent_config.agents.defaults.model = model.clone();
        match build_agent_loop(&agent_config) {
            Ok(new_agent) => {
                self.agent = Arc::new(new_agent);
                self.status_line =
                    format!("✓ Chatting with {model} (this session; default unchanged)");
                self.config.recent_models.retain(|x| x != &model);
                self.config.recent_models.insert(0, model);
                self.config.recent_models.truncate(8);
                let _ = save_desktop_config(&self.config);
            }
            Err(e) => {
                self.status_line = format!("✗ Failed to switch model: {e}");
            }
        }
    }

    fn refresh_sessions(&mut self) {
        self.sessions_cache = self.sessions.list_sessions();
    }

    fn load_session_history(&mut self) {
        self.chat_lines.clear();
        // The event log records what the person actually received, the
        // model's reasoning, and every step - drawn from it, the chat shows
        // the real reply (including the "what actually happened" summary)
        // rather than the trimmed copy kept as model history.
        let log = metis_core::events::EventLog::new(metis_core::events::EventLog::default_dir());
        let events = log.read(&self.active_session);
        if !events.is_empty() {
            self.chat_lines = chat_from_events(&events);
            return;
        }
        // Disk-fresh read: the agent writes sessions through its own manager
        // instance, so a cached read here would show stale (often empty)
        // history and blank the chat right after a reply arrives.
        for msg in self.sessions.get_history_fresh(&self.active_session, 200) {
            if let Some((role, text)) = message_display(&msg) {
                if !text.trim().is_empty() {
                    self.chat_lines.push(ChatLine { role, text, ..Default::default() });
                }
            }
        }
    }

    fn new_session(&mut self) {
        self.active_session = format!(
            "desktop:{}",
            chrono::Utc::now().format("%Y%m%d-%H%M%S")
        );
        self.chat_lines.clear();
        self.input.clear();
        self.attachments.clear();
        self.nav = NavPanel::Chat;
        self.status_line = "New session".into();
    }

    /// Queue files for the next message. Folders and duplicates are skipped.
    fn add_attachments(&mut self, paths: impl IntoIterator<Item = std::path::PathBuf>) {
        let mut skipped = 0;
        for p in paths {
            if !p.is_file() || self.attachments.contains(&p) || self.attachments.len() >= MAX_ATTACHMENTS {
                skipped += 1;
                continue;
            }
            self.attachments.push(p);
        }
        if skipped > 0 {
            self.status_line = format!(
                "Skipped {skipped} item(s): only new files are attached (max {MAX_ATTACHMENTS})"
            );
        }
    }

    /// Refresh the live view of the turn in progress from the event log.
    fn poll_live_progress(&mut self, now: f64) {
        let Some(pending) = &self.pending else {
            self.live_steps.clear();
            self.live_thinking = None;
            return;
        };
        if now - self.live_last_load < 0.7 || pending.session_key != self.active_session {
            return;
        }
        self.live_last_load = now;
        let log = metis_core::events::EventLog::new(metis_core::events::EventLog::default_dir());
        let (steps, thinking) = live_turn(&log.read(&pending.session_key));
        self.live_steps = steps;
        self.live_thinking = thinking;
    }

    /// True on the frame Ctrl+V goes down. egui reports Ctrl+V only when the
    /// clipboard holds text, so with just a screenshot there it never sees
    /// the key at all; read the key state directly instead.
    fn paste_shortcut(&mut self, ctx: &egui::Context) -> bool {
        let (command, focused) = ctx.input(|i| (i.modifiers.command, i.focused));
        #[cfg(windows)]
        let v_down = unsafe {
            windows_sys::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState(0x56) as u16 & 0x8000 != 0
        };
        #[cfg(not(windows))]
        let v_down = ctx.input(|i| i.key_down(egui::Key::V));
        let down = command && focused && v_down;
        let pressed = down && !self.paste_key_down;
        self.paste_key_down = down;
        pressed
    }

    /// Save the clipboard image as a PNG under the media folder and attach
    /// it. `explicit`: the button was used, so say so when there is no image.
    fn paste_image_from_clipboard(&mut self, explicit: bool) {
        let image = arboard::Clipboard::new().and_then(|mut c| c.get_image());
        let image = match image {
            Ok(img) => img,
            Err(_) => {
                if explicit {
                    self.status_line = "The clipboard has no image to paste".into();
                }
                return;
            }
        };
        match save_clipboard_png(&image) {
            Ok(path) => {
                self.add_attachments([path]);
                self.status_line = "Screenshot attached".into();
            }
            Err(e) => self.status_line = format!("Could not paste the image: {e}"),
        }
    }

    fn pick_attachments(&mut self) {
        if let Some(files) = rfd::FileDialog::new().set_title("Attach files").pick_files() {
            self.add_attachments(files);
        }
    }

    fn select_session(&mut self, key: &str) {
        self.active_session = key.to_string();
        self.load_session_history();
        self.nav = NavPanel::Chat;
    }

    fn toggle_pin(&mut self, key: &str) {
        if let Some(pos) = self.config.pinned_sessions.iter().position(|k| k == key) {
            self.config.pinned_sessions.remove(pos);
        } else {
            self.config.pinned_sessions.push(key.to_string());
        }
        let _ = save_desktop_config(&self.config);
    }

    fn send_message(&mut self) {
        let typed = self.input.trim().to_string();
        if (typed.is_empty() && self.attachments.is_empty()) || self.pending.is_some() {
            return;
        }
        let files = std::mem::take(&mut self.attachments);
        let (text, images) = compose_with_attachments(&typed, &files);
        let shown = if files.is_empty() {
            typed
        } else {
            let names: Vec<String> = files
                .iter()
                .map(|p| format!("📎 {}", p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()))
                .collect();
            format!("{typed}\n{}", names.join("  ")).trim().to_string()
        };
        self.chat_lines.push(ChatLine {
            role: "You",
            text: shown,
            ..Default::default()
        });
        self.input.clear();
        self.status_line = "Thinking…".into();

        let agent = Arc::clone(&self.agent);
        let session_key = self.active_session.clone();
        let (tx, rx) = oneshot::channel();
        let key_for_pending = session_key.clone();

        self.runtime.spawn(async move {
            let (channel, chat_id) = split_session_key(&session_key);
            let result = agent
                .process_direct_session_with_media(&channel, &chat_id, &text, &images)
                .await
                .map_err(|e| e.to_string());
            let _ = tx.send(result);
        });

        self.pending = Some(PendingReply {
            session_key: key_for_pending,
            rx,
        });
    }

    fn poll_pending(&mut self, ctx: &egui::Context) {
        let mut finished: Option<Result<String, String>> = None;
        if let Some(pending) = self.pending.as_mut() {
            if let Ok(result) = pending.rx.try_recv() {
                finished = Some(result);
            }
        }
        if let Some(result) = finished {
            let pending = self.pending.take().unwrap();
            match result {
                Ok(reply) => {
                    // The agent appends a token-usage footer to the wire reply
                    // but never persists it, so the history reload below would
                    // silently drop it — show it in the status line instead.
                    let (text, footer) = split_usage_footer(&reply);
                    self.chat_lines.push(ChatLine {
                        role: "Agent",
                        text,
                        ..Default::default()
                    });
                    self.status_line = footer.unwrap_or_default();
                }
                Err(err) => {
                    self.status_line = format!("Error: {err}");
                }
            }
            if pending.session_key == self.active_session {
                self.load_session_history();
            }
            self.refresh_sessions();
            ctx.request_repaint();
        } else if self.pending.is_some() {
            ctx.request_repaint_after(std::time::Duration::from_millis(200));
        }
    }

    /// Apply the Models panel fields: validate, persist to config.json, and
    /// swap in a freshly built agent loop (same construction path as startup).
    fn apply_models(&mut self) {
        if self.pending.is_some() {
            self.model_status = "⏳ Wait for the current reply to finish first.".into();
            return;
        }
        let main = self.model_main.trim().to_string();
        if main.is_empty() {
            self.model_status = "✗ Main model cannot be empty.".into();
            return;
        }
        let sub = self.model_sub.trim().to_string();

        let mut agent_config = load_config(None);
        let providers = agent_config.providers.to_map();
        if match_provider(&main, &providers).is_none() {
            self.model_status = format!(
                "✗ No configured provider for '{main}'. Add its API key via `metis onboard`, \
                 or use a local model (ollama/…, lmstudio/…)."
            );
            return;
        }
        let sub_warning = if !sub.is_empty() && match_provider(&sub, &providers).is_none() {
            " ⚠ subagent model has no provider; subagents will fall back to the main model."
        } else {
            ""
        };

        agent_config.agents.defaults.model = main.clone();
        agent_config.agents.defaults.subagent_model = sub;
        if let Err(e) = save_config(&agent_config, None) {
            self.model_status = format!("✗ Failed to save config: {e}");
            return;
        }

        match build_agent_loop(&agent_config) {
            Ok(new_agent) => {
                self.agent = Arc::new(new_agent);
                self.model_status = format!("✓ Switched to {main} (saved to config).{sub_warning}");
            }
            Err(e) => {
                self.model_status = format!("✗ Saved config, but rebuilding the agent failed: {e}");
            }
        }
    }

    /// Kick off a background sweep of every provider configured in
    /// `config.json`, listing the models each one actually offers.
    fn discover_models(&mut self) {
        if self.models_rx.is_some() {
            return;
        }
        let providers = load_config(None).providers.to_map();

        let (tx, rx) = oneshot::channel();
        self.models_rx = Some(rx);
        self.model_status = "Loading models from configured providers…".into();
        self.runtime.spawn(async move {
            let (models, errors) = metis_providers::discover_models(&providers).await;
            let _ = tx.send(ModelSweep { models, errors });
        });
    }

    /// Collect a finished discovery sweep, if any.
    fn poll_model_discovery(&mut self, ctx: &egui::Context) {
        let Some(rx) = self.models_rx.as_mut() else {
            return;
        };
        let sweep = match rx.try_recv() {
            Ok(sweep) => sweep,
            Err(oneshot::error::TryRecvError::Empty) => {
                ctx.request_repaint_after(std::time::Duration::from_millis(200));
                return;
            }
            Err(oneshot::error::TryRecvError::Closed) => {
                self.models_rx = None;
                self.model_status = "✗ Model discovery task failed.".into();
                return;
            }
        };
        self.models_rx = None;

        let count = sweep.models.len();
        let providers: std::collections::BTreeSet<&str> =
            sweep.models.iter().map(|m| m.provider_display).collect();
        self.available_models = sweep.models;
        self.model_errors = sweep.errors;

        self.model_status = if count == 0 {
            "No models found. Add a provider API key via `metis onboard`, or start Ollama.".into()
        } else {
            format!(
                "✓ {count} models from {}",
                providers.into_iter().collect::<Vec<_>>().join(", ")
            )
        };
    }

    /// Health-test the model in the main-model field: one tiny real chat
    /// completion (no tools), reporting round-trip time or the exact failure.
    /// Catches dead servers, missing keys, and models too slow to be usable —
    /// BEFORE you switch to them.
    fn test_model(&mut self) {
        if self.model_test_rx.is_some() {
            return;
        }
        let model = self.model_main.trim().to_string();
        if model.is_empty() {
            self.model_status = "✗ Enter a model to test.".into();
            return;
        }

        let agent_config = load_config(None);
        let providers = agent_config.providers.to_map();
        let provider = match metis_providers::http_provider::create_provider(&model, &providers) {
            Ok(p) => p,
            Err(e) => {
                self.model_status = format!("✗ {e}");
                return;
            }
        };

        let (tx, rx) = oneshot::channel();
        self.model_test_rx = Some(rx);
        self.model_status = format!("🧪 Testing {model} (local models may take a while to load)…");
        self.runtime.spawn(async move {
            use metis_core::types::Message;
            use metis_providers::traits::{LlmProvider, LlmRequestConfig};

            let started = std::time::Instant::now();
            let req = LlmRequestConfig {
                max_tokens: 16,
                temperature: 0.0,
                ..Default::default()
            };
            let messages = vec![Message::user("Reply with exactly: OK")];
            let resp = provider.chat(&messages, None, &model, &req).await;
            let secs = started.elapsed().as_secs_f64();

            let result = match resp.content {
                Some(text) if text.starts_with("Error calling LLM") => Err(text),
                Some(text) => Ok((text.trim().chars().take(40).collect::<String>(), secs)),
                None => Err("Model returned an empty response".into()),
            };
            let _ = tx.send(result);
        });
    }

    /// Surface the result of a mailbox check or test send once it lands.
    fn poll_email_diag(&mut self, ctx: &egui::Context) {
        let Some(rx) = self.email_diag_rx.as_mut() else {
            return;
        };
        match rx.try_recv() {
            Ok(Ok(msg)) => {
                self.email_diag_rx = None;
                self.settings_status = format!("✓ {msg}");
            }
            Ok(Err(e)) => {
                self.email_diag_rx = None;
                self.settings_status = format!("✗ {e}");
            }
            Err(oneshot::error::TryRecvError::Empty) => {
                // Still running - keep the UI ticking so the result appears
                // without the user having to move the mouse.
                ctx.request_repaint_after(std::time::Duration::from_millis(300));
            }
            Err(_) => self.email_diag_rx = None,
        }
    }

    fn poll_model_test(&mut self, ctx: &egui::Context) {
        let Some(rx) = self.model_test_rx.as_mut() else {
            return;
        };
        match rx.try_recv() {
            Ok(Ok((reply, secs))) => {
                self.model_test_rx = None;
                let speed = if secs > 30.0 {
                    " ⚠ very slow — expect multi-minute replies with the full agent prompt"
                } else if secs > 10.0 {
                    " ⚠ slow"
                } else {
                    ""
                };
                self.model_status =
                    format!("✓ Model responded in {secs:.1}s: \"{reply}\"{speed}");
            }
            Ok(Err(e)) => {
                self.model_test_rx = None;
                self.model_status = format!("✗ Test failed: {e}");
            }
            Err(oneshot::error::TryRecvError::Empty) => {
                ctx.request_repaint_after(std::time::Duration::from_millis(250));
            }
            Err(oneshot::error::TryRecvError::Closed) => {
                self.model_test_rx = None;
                self.model_status = "✗ Model test task failed.".into();
            }
        }
    }

    fn filtered_sessions(&self) -> Vec<&SessionSummary> {
        let q = self.session_search.to_lowercase();
        self.sessions_cache
            .iter()
            .filter(|s| q.is_empty() || s.key.to_lowercase().contains(&q))
            .collect()
    }

    fn project_groups(&self) -> HashMap<String, Vec<&SessionSummary>> {
        let mut groups: HashMap<String, Vec<&SessionSummary>> = HashMap::new();
        for s in &self.sessions_cache {
            let project = s
                .key
                .split_once(':')
                .map(|(p, _)| p.to_string())
                .unwrap_or_else(|| "other".into());
            groups.entry(project).or_default().push(s);
        }
        groups
    }
}

impl eframe::App for MetisDesktopApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // A waiting approval comes first: the agent is blocked on it.
        if self.tool_approval.is_none() {
            if let Ok(ask) = self.tool_approval_rx.try_recv() {
                self.tool_approval = Some(ask);
            }
        }
        if let Some((req, _)) = &self.tool_approval {
            if let Some(decision) = super::approval_ui::draw_approval_window(ctx, req) {
                if let Some((_, reply)) = self.tool_approval.take() {
                    let _ = reply.send(decision);
                }
                self.status_line = match decision {
                    metis_agent::approval::ApprovalDecision::Deny => "Denied - nothing was changed".into(),
                    _ => "Approved - continuing".into(),
                };
            }
            ctx.request_repaint_after(std::time::Duration::from_millis(250));
        } else if self.pending.is_some() {
            // Keep polling so a request raised mid-turn appears promptly.
            ctx.request_repaint_after(std::time::Duration::from_millis(300));
        }

        self.poll_pending(ctx);
        self.poll_model_discovery(ctx);
        self.poll_model_test(ctx);
        self.poll_email_diag(ctx);

        let now = ctx.input(|i| i.time);
        self.poll_live_progress(now);

        // Paste a screenshot: Ctrl+V with an image on the clipboard.
        if self.nav == NavPanel::Chat && self.paste_shortcut(ctx) {
            let pasted_text = ctx.input(|i| {
                i.events.iter().any(|e| matches!(e, egui::Event::Paste(t) if !t.trim().is_empty()))
            });
            if !pasted_text {
                self.paste_image_from_clipboard(false);
            }
        }
        if now - self.last_refresh > 5.0 {
            self.refresh_sessions();
            self.cron_jobs = load_cron_jobs();
            self.last_refresh = now;
            // The gateway may have queued a reply since the last tick.
            self.refresh_approvals();
        }

        // Files dropped anywhere on the window become attachments.
        let dropped: Vec<std::path::PathBuf> = ctx.input(|i| {
            i.raw.dropped_files.iter().filter_map(|f| f.path.clone()).collect()
        });
        if !dropped.is_empty() {
            self.nav = NavPanel::Chat;
            self.add_attachments(dropped);
        }
        let hovering_files = ctx.input(|i| !i.raw.hovered_files.is_empty());

        egui::TopBottomPanel::bottom("input_panel").show(ctx, |ui| {
            ui.add_space(8.0);
            if !self.attachments.is_empty() {
                let mut remove: Option<usize> = None;
                ui.horizontal_wrapped(|ui| {
                    ui.add_space(12.0);
                    for (i, p) in self.attachments.iter().enumerate() {
                        let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                        if ui
                            .small_button(format!("📎 {}  ✕", truncate(&name, 28)))
                            .on_hover_text(p.display().to_string())
                            .clicked()
                        {
                            remove = Some(i);
                        }
                    }
                });
                if let Some(i) = remove {
                    self.attachments.remove(i);
                }
                ui.add_space(4.0);
            }
            ui.horizontal(|ui| {
                ui.add_space(12.0);
                if ui
                    .add_enabled(self.pending.is_none(), egui::Button::new("＋"))
                    .on_hover_text("Attach files (or drag & drop them onto the window)")
                    .clicked()
                {
                    self.pick_attachments();
                }
                if ui
                    .add_enabled(self.pending.is_none(), egui::Button::new("📋"))
                    .on_hover_text("Paste a screenshot from the clipboard (or press Ctrl+V)")
                    .clicked()
                {
                    self.paste_image_from_clipboard(true);
                }
                let w = ui.available_width() - 300.0;
                let response = ui.add(
                    egui::TextEdit::multiline(&mut self.input)
                        .hint_text("Start with a goal…")
                        .desired_width(w)
                        .desired_rows(2),
                );

                // Model dropdown — switch who you're talking to, right from the chat.
                let mut picked: Option<String> = None;
                let mut open_models_panel = false;
                let active_model = self.agent.model().to_string();
                let choices = self.model_choices();
                ui.add_enabled_ui(self.pending.is_none(), |ui| {
                    egui::ComboBox::from_id_salt("chat_model_picker")
                        .selected_text(
                            egui::RichText::new(truncate(&active_model, 22)).small(),
                        )
                        .width(170.0)
                        .show_ui(ui, |ui| {
                            for (id, label) in &choices {
                                let is_selected = *id == active_model;
                                let text = if is_selected {
                                    format!("✓ {label}")
                                } else {
                                    format!("   {label}")
                                };
                                if ui.selectable_label(is_selected, text).clicked() && !is_selected {
                                    picked = Some(id.clone());
                                }
                            }
                            ui.separator();
                            if ui.selectable_label(false, "⚙ More models…").clicked() {
                                open_models_panel = true;
                            }
                        });
                });
                if let Some(model) = picked {
                    self.switch_chat_model(model);
                }
                if open_models_panel {
                    self.nav = NavPanel::Models;
                }

                let send = ui
                    .add_enabled(
                        self.pending.is_none()
                            && (!self.input.trim().is_empty() || !self.attachments.is_empty()),
                        egui::Button::new("Send"),
                    )
                    .clicked();
                if (response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)))
                    || send
                {
                    if !ui.input(|i| i.modifiers.shift) {
                        self.send_message();
                    }
                }
                if !self.status_line.is_empty() {
                    ui.label(
                        egui::RichText::new(&self.status_line)
                            .small()
                            .color(egui::Color32::GRAY),
                    );
                }
            });
            ui.add_space(8.0);
        });

        egui::SidePanel::left("sidebar")
            .exact_width(self.config.sidebar_width)
            .frame(egui::Frame::default().fill(SIDEBAR_BG))
            .show(ctx, |ui| {
                ui.add_space(12.0);
                ui.horizontal(|ui| {
                    ui.heading(egui::RichText::new("Metis").color(ACCENT).strong());
                });
                ui.add_space(8.0);

                if ui.button("➕  New session").clicked() {
                    self.new_session();
                }
                ui.add_space(4.0);

                sidebar_nav_button(ui, &mut self.nav, NavPanel::Chat, "💬  Chat");
                sidebar_nav_button(ui, &mut self.nav, NavPanel::Models, "🧠  Models");
                sidebar_nav_button(ui, &mut self.nav, NavPanel::SkillsTools, "🛠  Skills & Tools");
                sidebar_nav_button(ui, &mut self.nav, NavPanel::Messaging, "📨  Messaging");
                sidebar_nav_button(ui, &mut self.nav, NavPanel::Artifacts, "📁  Artifacts");
                sidebar_nav_button(ui, &mut self.nav, NavPanel::CronJobs, "⏱  Cron jobs");
                let waiting = self.approvals.len();
                let label = if waiting > 0 {
                    format!("✉  Approvals ({waiting})")
                } else {
                    "✉  Approvals".to_string()
                };
                sidebar_nav_button(ui, &mut self.nav, NavPanel::Approvals, &label);
                sidebar_nav_button(ui, &mut self.nav, NavPanel::Settings, "⚙  Settings");

                ui.add_space(12.0);
                ui.label(egui::RichText::new("Search sessions…").small().weak());
                ui.text_edit_singleline(&mut self.session_search);
                ui.add_space(8.0);

                ui.label(
                    egui::RichText::new("PINNED")
                        .small()
                        .strong()
                        .color(ACCENT),
                );
                if self.config.pinned_sessions.is_empty() {
                    ui.label(
                        egui::RichText::new("Shift-click a chat to pin")
                            .small()
                            .weak(),
                    );
                } else {
                    for key in self.config.pinned_sessions.clone() {
                        let label = session_label(&key);
                        let selected = self.active_session == key;
                        if ui.selectable_label(selected, label).clicked() {
                            self.select_session(&key);
                        }
                    }
                }

                ui.add_space(8.0);
                let session_rows: Vec<(String, bool)> = self
                    .filtered_sessions()
                    .into_iter()
                    .map(|s| (s.key.clone(), self.active_session == s.key))
                    .collect();
                let count = session_rows.len();
                ui.label(
                    egui::RichText::new(format!("SESSIONS {count}"))
                        .small()
                        .strong()
                        .color(ACCENT),
                );
                let mut select_key: Option<String> = None;
                let mut pin_key: Option<String> = None;
                egui::ScrollArea::vertical()
                    .id_salt("sessions_scroll")
                    .max_height(180.0)
                    .show(ui, |ui| {
                        for (key, selected) in &session_rows {
                            let label = session_label(key);
                            let resp = ui
                                .push_id(key, |ui| ui.selectable_label(*selected, label))
                                .inner;
                            if resp.clicked() {
                                select_key = Some(key.clone());
                            }
                            if resp.secondary_clicked() {
                                pin_key = Some(key.clone());
                            }
                        }
                    });
                if let Some(key) = select_key {
                    self.select_session(&key);
                }
                if let Some(key) = pin_key {
                    self.toggle_pin(&key);
                }

                ui.add_space(8.0);
                ui.label(
                    egui::RichText::new("PROJECTS")
                        .small()
                        .strong()
                        .color(ACCENT),
                );
                let project_rows: Vec<(String, Vec<String>)> = self
                    .project_groups()
                    .into_iter()
                    .map(|(p, sessions)| {
                        (
                            p,
                            sessions
                                .iter()
                                .take(8)
                                .map(|s| s.key.clone())
                                .collect(),
                        )
                    })
                    .collect();
                let mut project_select: Option<String> = None;
                egui::ScrollArea::vertical().id_salt("projects_scroll").show(ui, |ui| {
                    let mut projects = project_rows;
                    projects.sort_by(|a, b| a.0.cmp(&b.0));
                    for (project, keys) in &projects {
                        ui.label(
                            egui::RichText::new(format!(
                                "{} {}",
                                project.to_uppercase(),
                                keys.len()
                            ))
                            .small()
                            .strong(),
                        );
                        for key in keys {
                            let label = session_label(key);
                            if ui.push_id(key, |ui| ui.small_button(label)).inner.clicked() {
                                project_select = Some(key.clone());
                            }
                        }
                    }
                });
                if let Some(key) = project_select {
                    self.select_session(&key);
                }
            });

        egui::CentralPanel::default()
            .frame(egui::Frame::default().fill(MAIN_BG))
            .show(ctx, |ui| match self.nav {
                NavPanel::Chat => self.draw_chat(ui),
                NavPanel::Models => self.draw_models(ui),
                NavPanel::CronJobs => self.draw_cron(ui),
                NavPanel::Settings => self.draw_settings(ui),
                NavPanel::Approvals => self.draw_approvals(ui),
                NavPanel::SkillsTools => draw_placeholder(
                    ui,
                    "Skills & Tools",
                    "Agent skills live in the workspace `skills/` folder.\nUse the CLI or chat to invoke them.",
                ),
                NavPanel::Messaging => draw_placeholder(
                    ui,
                    "Messaging",
                    "Channel sessions (Telegram, Discord, WhatsApp) appear under PROJECTS.\nRun `metis gateway` to connect channels.",
                ),
                NavPanel::Artifacts => draw_placeholder(
                    ui,
                    "Artifacts",
                    "Files the agent creates appear in your workspace directory.",
                ),
            });

        if hovering_files {
            let screen = ctx.screen_rect();
            let painter = ctx.layer_painter(egui::LayerId::new(
                egui::Order::Foreground,
                egui::Id::new("drop_overlay"),
            ));
            painter.rect_filled(screen, 0.0, egui::Color32::from_black_alpha(160));
            painter.text(
                screen.center(),
                egui::Align2::CENTER_CENTER,
                "Drop files to attach",
                egui::FontId::proportional(28.0),
                egui::Color32::WHITE,
            );
        }
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        if self.config.save_window_geometry {
            let _ = save_desktop_config(&self.config);
        }
    }
}

impl MetisDesktopApp {
    fn draw_chat(&mut self, ui: &mut egui::Ui) {
        ui.vertical_centered(|ui| {
            ui.add_space(40.0);
            ui.label(
                egui::RichText::new(&self.config.agent_title)
                    .size(36.0)
                    .strong()
                    .color(ACCENT),
            );
            ui.add_space(8.0);
            ui.label(
                "Drop a file path, a traceback, or a rough idea. I'll investigate, suggest next steps, and keep things reversible.",
            );
            ui.label(
                egui::RichText::new(format!(
                    "Session: {}  ·  Model: {}",
                    self.active_session,
                    self.agent.model()
                ))
                .small()
                .weak(),
            );
        });

        ui.add_space(16.0);
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .stick_to_bottom(true)
            .show(ui, |ui| {
                for (i, line) in self.chat_lines.iter().enumerate() {
                    ui.horizontal(|ui| {
                        let color = if line.role == "You" {
                            egui::Color32::DARK_GRAY
                        } else {
                            ACCENT
                        };
                        ui.label(egui::RichText::new(format!("{}:", line.role)).strong().color(color));
                        if ui
                            .small_button("📋 Copy")
                            .on_hover_text("Copy this message")
                            .clicked()
                        {
                            ui.ctx().copy_text(line.text.clone());
                        }
                    });
                    ui.add(egui::Label::new(&line.text).selectable(true));
                    if line.role == "Agent" && (line.asked.is_some() || line.record.is_some()) {
                        draw_conclusion(ui, line);
                    }
                    if !line.steps.is_empty() {
                        egui::CollapsingHeader::new(format!("🔧 Steps ({})", line.steps.len()))
                            .id_salt(("steps", i))
                            .show(ui, |ui| {
                                for step in &line.steps {
                                    ui.add(
                                        egui::Label::new(
                                            egui::RichText::new(step).monospace().color(READABLE_TEXT),
                                        )
                                        .selectable(true),
                                    );
                                }
                            });
                    }
                    if let Some(thinking) = &line.thinking {
                        egui::CollapsingHeader::new("🧠 Thinking")
                            .id_salt(("thinking", i))
                            .show(ui, |ui| draw_thinking(ui, thinking));
                    }
                    ui.add_space(12.0);
                }
                if self.pending.is_some() {
                    self.draw_live_progress(ui);
                }
            });
    }

    /// While the agent works: what it was asked, each step as it happens,
    /// and the reasoning behind the latest one.
    fn draw_live_progress(&self, ui: &mut egui::Ui) {
        egui::Frame::none()
            .fill(egui::Color32::from_rgb(238, 244, 252))
            .stroke(egui::Stroke::new(1.0, egui::Color32::from_rgb(190, 210, 235)))
            .rounding(6.0)
            .inner_margin(10.0)
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.label(egui::RichText::new("⏳ Working…").strong().color(ACCENT));
                if let Some(asked) = self.chat_lines.iter().rev().find(|l| l.role == "You") {
                    ui.label(
                        egui::RichText::new(format!("Asked: {}", first_line(&asked.text, 160)))
                            .color(READABLE_TEXT),
                    );
                }
                if self.live_steps.is_empty() {
                    ui.label(egui::RichText::new("Thinking - no tool has run yet.").color(READABLE_TEXT));
                } else {
                    ui.label(
                        egui::RichText::new(format!("Steps so far ({}):", self.live_steps.len()))
                            .color(READABLE_TEXT),
                    );
                    let skip = self.live_steps.len().saturating_sub(8);
                    for step in &self.live_steps[skip..] {
                        ui.label(egui::RichText::new(step).monospace().color(READABLE_TEXT));
                    }
                }
                if let Some(t) = &self.live_thinking {
                    let chars: Vec<char> = t.chars().collect();
                    let tail: String = chars[chars.len().saturating_sub(400)..].iter().collect();
                    ui.add_space(4.0);
                    ui.label(egui::RichText::new("Latest reasoning:").small().color(READABLE_TEXT));
                    draw_thinking(ui, &format!("…{}", tail.trim()));
                }
            });
    }

    // ── Settings panel ──

    /// Back up config.json next to itself before overwriting it. A GUI that
    /// rewrites the whole file should never be the reason a working setup is
    /// lost, and the backup is what makes an accidental "Save" recoverable.
    fn backup_config_file() -> Option<std::path::PathBuf> {
        let path = metis_core::utils::get_data_path().join("config.json");
        if !path.is_file() {
            return None;
        }
        let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
        let backup = path.with_extension(format!("json.bak-{stamp}"));
        std::fs::copy(&path, &backup).ok().map(|_| backup)
    }

    /// The setup script, compiled into the binary.
    ///
    /// Deployments are frequently just `metis.exe` copied to another machine
    /// with no repo alongside it, which left the button dead with "could not
    /// find the script". Embedding it means the feature travels with the
    /// binary; an on-disk copy still wins so local edits are respected.
    const SETUP_SCRIPT: &'static str =
        include_str!("../../../../scripts/setup-o365-graph.ps1");

    /// Path to the script to run: a real file next to the binary if there is
    /// one, otherwise the embedded copy written to a temp file.
    fn resolve_setup_script() -> std::io::Result<std::path::PathBuf> {
        if let Some(found) = Self::find_setup_script() {
            return Ok(found);
        }
        let dir = std::env::temp_dir().join("metis-setup");
        std::fs::create_dir_all(&dir)?;
        let path = dir.join("setup-o365-graph.ps1");
        std::fs::write(&path, Self::SETUP_SCRIPT)?;
        Ok(path)
    }

    /// Locate `setup-o365-graph.ps1` on disk. The binary can be run from a dev
    /// checkout (`target/debug/metis.exe`) or copied somewhere with the
    /// scripts alongside it, so check the plausible layouts rather than
    /// assuming a fixed path.
    fn find_setup_script() -> Option<std::path::PathBuf> {
        let mut roots: Vec<std::path::PathBuf> = Vec::new();
        if let Ok(exe) = std::env::current_exe() {
            let mut dir = exe.parent().map(|p| p.to_path_buf());
            // exe dir, then up a few levels for target/debug|release layouts.
            for _ in 0..4 {
                match dir {
                    Some(d) => {
                        roots.push(d.clone());
                        dir = d.parent().map(|p| p.to_path_buf());
                    }
                    None => break,
                }
            }
        }
        if let Ok(cwd) = std::env::current_dir() {
            roots.push(cwd);
        }
        for r in roots {
            for candidate in [
                r.join("scripts").join("setup-o365-graph.ps1"),
                r.join("setup-o365-graph.ps1"),
            ] {
                if candidate.is_file() {
                    return Some(candidate);
                }
            }
        }
        None
    }

    /// Launch the Azure setup script in its own console window.
    ///
    /// Deliberately a separate visible window rather than a captured child
    /// process: the script requires an interactive Microsoft sign-in (with
    /// MFA) and prints a client secret exactly once, both of which the user
    /// has to see and act on. `-NoExit` keeps the window open afterwards so
    /// the output survives long enough to read.
    fn run_graph_setup(&mut self) {
        let mailbox = self.settings.email_graph_user_id.trim().to_string();
        if mailbox.is_empty() {
            self.settings_status = "Enter the mailbox first.".to_string();
            return;
        }
        let script = match Self::resolve_setup_script() {
            Ok(p) => p,
            Err(e) => {
                self.settings_status = format!("Could not prepare the setup script: {e}");
                return;
            }
        };

        // Prefer PowerShell 7 when it is installed: Windows PowerShell 5.1
        // ships PowerShellGet 1.0.0.1, which cannot install the
        // Microsoft.Graph modules and fails silently, so the script has to
        // upgrade it and relaunch. pwsh skips that whole detour.
        let shell = if which_pwsh() {
            "pwsh"
        } else if cfg!(windows) {
            "powershell"
        } else {
            "pwsh"
        };
        let mut cmd = std::process::Command::new(shell);
        cmd.args([
            "-NoExit",
            "-NoProfile",
            "-ExecutionPolicy",
            "Bypass",
            "-File",
        ])
        .arg(&script)
        .arg("-Mailbox")
        .arg(&mailbox)
        .arg("-WriteConfig");

        // Without CREATE_NEW_CONSOLE the child inherits this process's
        // console, so when Metis was itself started from a terminal the
        // script's prompts and output land in that same window mixed with
        // Metis' logs. The script is interactive (sign-in, one-time secret),
        // so it needs a window of its own.
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NEW_CONSOLE: u32 = 0x0000_0010;
            cmd.creation_flags(CREATE_NEW_CONSOLE);
        }

        let result = cmd.spawn();

        self.settings_status = match result {
            Ok(_) => format!(
                "Setup running in a new PowerShell window for {mailbox}. Sign in there, then click                  “Reload from disk” to pull in the tenant/client/secret it saves."
            ),
            Err(e) => format!("Could not start {shell}: {e}"),
        };
    }

    // ── Approvals panel ──

    /// Reload the queue from disk. The gateway is a different process, so
    /// the file is the only shared state; poll it rather than caching.
    fn refresh_approvals(&mut self) {
        let path = metis_core::approvals::default_path();
        match metis_core::approvals::load(&path) {
            Ok(store) => {
                self.approvals = metis_core::approvals::with_status(
                    &store,
                    metis_core::approvals::ApprovalStatus::Pending,
                )
                .into_iter()
                .cloned()
                .collect();
            }
            Err(e) => {
                self.approval_status = format!("Could not read the approval queue: {e}");
            }
        }
    }

    fn draw_approvals(&mut self, ui: &mut egui::Ui) {
        use metis_core::approvals::{self, ApprovalStatus};

        ui.horizontal(|ui| {
            ui.heading("Approvals");
            ui.add_space(10.0);
            if ui.button("↻  Refresh").clicked() {
                self.refresh_approvals();
            }
        });
        ui.label(
            egui::RichText::new(
                "Replies to senders outside your allow-list wait here. Nothing is sent until you approve it.                  Enable with channels.email.replyPolicy = \"approval\".",
            )
            .small()
            .color(egui::Color32::GRAY),
        );
        if !self.approval_status.is_empty() {
            ui.label(egui::RichText::new(&self.approval_status).color(ACCENT).strong());
        }
        ui.separator();

        if self.approvals.is_empty() {
            ui.add_space(20.0);
            ui.label(
                egui::RichText::new("Nothing waiting for approval.")
                    .color(egui::Color32::GRAY),
            );
            return;
        }

        let path = approvals::default_path();
        let mut action: Option<(String, ApprovalStatus)> = None;
        let entries = self.approvals.clone();

        egui::ScrollArea::vertical().id_salt("approvals_scroll").show(ui, |ui| {
            for e in &entries {
                ui.push_id(&e.id, |ui| {
                    egui::Frame::none()
                        .fill(egui::Color32::from_rgb(250, 250, 252))
                        .inner_margin(10.0)
                        .show(ui, |ui| {
                            ui.horizontal(|ui| {
                                ui.label(egui::RichText::new(&e.to).strong().color(ACCENT));
                                ui.label(egui::RichText::new(format!("· {}", e.subject)).small());
                            });
                            if !e.incoming_excerpt.trim().is_empty() {
                                ui.add_space(4.0);
                                ui.label(egui::RichText::new("They wrote:").small().strong());
                                ui.label(
                                    egui::RichText::new(&e.incoming_excerpt)
                                        .small()
                                        .color(egui::Color32::DARK_GRAY),
                                );
                            }
                            ui.add_space(6.0);
                            ui.label(egui::RichText::new("Proposed reply (editable):").small().strong());
                            let draft = self
                                .approval_edits
                                .entry(e.id.clone())
                                .or_insert_with(|| e.body.clone());
                            ui.add(
                                egui::TextEdit::multiline(draft)
                                    .desired_width(f32::INFINITY)
                                    .desired_rows(6),
                            );
                            ui.add_space(6.0);
                            ui.horizontal(|ui| {
                                if ui.button("✔  Approve & send").clicked() {
                                    action = Some((e.id.clone(), ApprovalStatus::Approved));
                                }
                                if ui.button("✖  Reject").clicked() {
                                    action = Some((e.id.clone(), ApprovalStatus::Rejected));
                                }
                                ui.label(
                                    egui::RichText::new(format!("id {}", e.id))
                                        .small()
                                        .color(egui::Color32::GRAY),
                                );
                            });
                        });
                });
                ui.add_space(8.0);
            }
        });

        if let Some((id, status)) = action {
            // Persist any edit before approving, so the gateway sends the
            // text actually shown here rather than the original draft.
            if status == ApprovalStatus::Approved {
                if let Some(body) = self.approval_edits.get(&id) {
                    let _ = approvals::set_body(&path, &id, body);
                }
            }
            match approvals::set_status(&path, &id, status, None) {
                Ok(Some(_)) => {
                    self.approval_status = match status {
                        ApprovalStatus::Approved => {
                            format!("Approved {id} — the gateway will send it shortly.")
                        }
                        _ => format!("Rejected {id} — it will not be sent."),
                    };
                    self.approval_edits.remove(&id);
                    self.refresh_approvals();
                }
                Ok(None) => self.approval_status = format!("{id} is no longer in the queue."),
                Err(e) => self.approval_status = format!("Could not update {id}: {e}"),
            }
        }
    }

    /// Run a mailbox check (or a test send) off the UI thread.
    ///
    /// Reads config from disk rather than the form, so what is tested is what
    /// the gateway will actually use — testing unsaved edits would report a
    /// success the running system does not share.
    fn start_email_diag(&mut self, send: bool) {
        #[cfg(feature = "email")]
        {
            let cfg = load_config(None).channels.email;
            let to = self.settings.email_test_recipient.trim().to_string();
            let (tx, rx) = oneshot::channel();
            self.email_diag_rx = Some(rx);
            self.settings_status = if send {
                format!("Sending a test message to {to}…")
            } else {
                "Checking the mailbox connection…".to_string()
            };
            self.runtime.spawn(async move {
                let result = if send {
                    metis_channels::email::send_test_message(&cfg, &to).await
                } else {
                    metis_channels::email::test_connection(&cfg).await
                };
                let _ = tx.send(result);
            });
        }
        #[cfg(not(feature = "email"))]
        {
            let _ = send;
            self.settings_status =
                "This build has no email support. Rebuild with --features email.".to_string();
        }
    }

    /// Collect a troubleshooting report and write it next to the config.
    ///
    /// Deliberately leads with whether a gateway is running: the desktop app
    /// hosts no channels, so "mail never arrives" is most often simply that
    /// nothing is polling the mailbox.
    fn write_diagnostics(&mut self) {
        let cfg = load_config(None);
        let e = &cfg.channels.email;
        let mask = |v: &str| {
            if v.trim().is_empty() {
                "(not set)".to_string()
            } else {
                format!("set, {} chars", v.trim().len())
            }
        };

        let pid_path = metis_core::utils::get_data_path().join("gateway.pid");
        let gateway = match std::fs::read_to_string(&pid_path)
            .ok()
            .and_then(|p| p.trim().parse::<u32>().ok())
        {
            Some(pid) => format!("gateway.pid present (pid {pid})"),
            None => "NO gateway.pid - the gateway does not appear to be running".to_string(),
        };

        let queue = metis_core::approvals::load(&metis_core::approvals::default_path())
            .map(|st| {
                let pend = metis_core::approvals::with_status(
                    &st,
                    metis_core::approvals::ApprovalStatus::Pending,
                )
                .len();
                let appr = metis_core::approvals::with_status(
                    &st,
                    metis_core::approvals::ApprovalStatus::Approved,
                )
                .len();
                format!("{pend} pending, {appr} approved-not-yet-sent")
            })
            .unwrap_or_else(|err| format!("unreadable: {err}"));

        let provider = if e.provider.trim().is_empty() { "imap" } else { e.provider.trim() };
        let mailbox = if e.graph_user_id.trim().is_empty() {
            e.imap_username.clone()
        } else {
            e.graph_user_id.clone()
        };
        let allowed = if e.allowed_users.is_empty() {
            "(empty - everyone allowed)".to_string()
        } else {
            e.allowed_users.join(", ")
        };

        let mut report = String::new();
        report.push_str("Metis diagnostics\n");
        report.push_str(&format!("Generated       : {}\n", chrono::Local::now().format("%Y-%m-%d %H:%M:%S")));
        report.push_str(&format!("Build           : {}\n", metis_core::build::version_line()));
        report.push_str(&format!("OS              : {}/{}\n\n", std::env::consts::OS, std::env::consts::ARCH));
        report.push_str("MAIL IS ONLY RECEIVED WHILE `metis gateway` IS RUNNING.\n");
        report.push_str("The desktop app hosts no channels; it is a GUI over the same config.\n");
        report.push_str(&format!("  {gateway}\n\n"));
        report.push_str("Email\n");
        report.push_str(&format!("  backend       : {provider}\n"));
        report.push_str(&format!("  mailbox       : {mailbox}\n"));
        report.push_str(&format!("  folder        : {}\n", e.imap_mailbox));
        report.push_str(&format!("  reply policy  : {}\n", e.reply_policy));
        report.push_str(&format!("  allowed users : {allowed}\n"));
        report.push_str(&format!("  poll interval : {}s\n", e.poll_interval_seconds));
        report.push_str(&format!("  graph tenant  : {}\n", mask(&e.graph_tenant_id)));
        report.push_str(&format!("  graph client  : {}\n", mask(&e.graph_client_id)));
        report.push_str(&format!("  graph secret  : {}\n", mask(&e.graph_client_secret)));
        report.push_str(&format!("  imap host     : {}\n", if e.imap_host.trim().is_empty() { "(not set)" } else { &e.imap_host }));
        report.push_str(&format!("  imap user     : {}\n", if e.imap_username.trim().is_empty() { "(not set)" } else { &e.imap_username }));
        report.push_str(&format!("  imap password : {}\n", mask(&e.imap_password)));
        report.push_str(&format!("  smtp host     : {}\n\n", if e.smtp_host.trim().is_empty() { "(not set)" } else { &e.smtp_host }));
        report.push_str("Agent\n");
        report.push_str(&format!("  model         : {}\n", cfg.agents.defaults.model));
        report.push_str(&format!("  workspace     : {}\n\n", cfg.agents.defaults.workspace));
        report.push_str(&format!("Approval queue  : {queue}\n\n"));
        report.push_str("Notes\n");
        report.push_str("  - reply policy 'allowlist_only' ignores senders not in allowed users.\n");
        report.push_str("  - allowed users accepts addresses and whole domains (@yourdomain.com).\n");
        report.push_str("  - Office 365 cannot use the IMAP backend at all; it needs Microsoft Graph.\n");

        let path = metis_core::utils::get_data_path().join("diagnostics.txt");
        match std::fs::write(&path, &report) {
            // Secrets appear only as "set, N chars", so the file is safe to
            // hand to someone for help.
            Ok(()) => {
                self.settings_status =
                    format!("Wrote {} (secrets masked - safe to share).", path.display())
            }
            Err(err) => self.settings_status = format!("Could not write the report: {err}"),
        }
    }

    /// Open Microsoft's admin-consent page for the configured app.
    ///
    /// This is the one-click fix for "the token carries no application
    /// permissions": the permissions are requested on the app but nobody with
    /// authority has approved them yet.
    fn open_admin_consent(&mut self) {
        let cfg = load_config(None).channels.email;
        if cfg.graph_tenant_id.trim().is_empty() || cfg.graph_client_id.trim().is_empty() {
            self.settings_status =
                "Save the tenant id and client id first.".to_string();
            return;
        }
        let url = format!(
            "https://login.microsoftonline.com/{}/adminconsent?client_id={}",
            cfg.graph_tenant_id.trim(),
            cfg.graph_client_id.trim()
        );
        let opened = if cfg!(windows) {
            std::process::Command::new("cmd").args(["/C", "start", "", &url]).spawn().is_ok()
        } else {
            std::process::Command::new("xdg-open").arg(&url).spawn().is_ok()
        };
        self.settings_status = if opened {
            "Opened the consent page — sign in as an administrator and approve, then use Check connection.".to_string()
        } else {
            format!("Could not open a browser. Visit: {url}")
        };
    }

    /// Re-apply permissions, consent and mailbox scoping to the EXISTING app.
    ///
    /// Deliberately does not mint a new secret or touch config.json: the
    /// usual cause of a 403 is scoping or consent, not credentials, and
    /// re-registering would invalidate a working setup for no reason.
    fn run_graph_setup_repair(&mut self) {
        let cfg = load_config(None).channels.email;
        let mailbox = cfg.graph_user_id.trim().to_string();
        if mailbox.is_empty() {
            self.settings_status = "Save the mailbox address first.".to_string();
            return;
        }
        let script = match Self::resolve_setup_script() {
            Ok(p) => p,
            Err(e) => {
                self.settings_status = format!("Could not prepare the setup script: {e}");
                return;
            }
        };
        let shell = if which_pwsh() { "pwsh" } else if cfg!(windows) { "powershell" } else { "pwsh" };
        let mut cmd = std::process::Command::new(shell);
        cmd.args(["-NoExit", "-NoProfile", "-ExecutionPolicy", "Bypass", "-File"])
            .arg(&script)
            .arg("-Mailbox")
            .arg(&mailbox)
            .arg("-RepairAccess");
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NEW_CONSOLE: u32 = 0x0000_0010;
            cmd.creation_flags(CREATE_NEW_CONSOLE);
        }
        self.settings_status = match cmd.spawn() {
            Ok(_) => format!(
                "Repairing access for {mailbox} in a new window. Your secret and config.json are \
                 left unchanged. Scoping can take ~30 min to apply — then use Check connection."
            ),
            Err(e) => format!("Could not start {shell}: {e}"),
        };
    }

    fn draw_settings(&mut self, ui: &mut egui::Ui) {
        if self.show_first_run {
            egui::Frame::none()
                .fill(egui::Color32::from_rgb(255, 249, 230))
                .inner_margin(10.0)
                .show(ui, |ui| {
                    ui.label(
                        egui::RichText::new("First run — Metis is not configured yet")
                            .strong()
                            .color(egui::Color32::from_rgb(150, 90, 0)),
                    );
                    ui.label("Add an API key under “Provider API keys”, set the agent model, then Save. Local models (Ollama) need no key.");
                    if ui.button("Got it").clicked() {
                        self.show_first_run = false;
                    }
                });
            ui.add_space(8.0);
        }

        let status = self.settings_status.clone();
        let action = super::settings::draw(ui, &mut self.settings, &mut self.settings_reveal, &status);

        match action {
            super::settings::SettingsAction::Save => {
                let errors = self.settings.validation_errors();
                if !errors.is_empty() {
                    self.settings_status = format!("Not saved — {}", errors.join(" "));
                } else {
                    // Re-read from disk first so a field this editor does not
                    // expose (or something changed by the CLI meanwhile) is
                    // preserved rather than clobbered by a stale in-memory copy.
                    let mut cfg = load_config(None);
                    self.settings.apply_to(&mut cfg);
                    let backup = Self::backup_config_file();
                    match save_config(&cfg, None) {
                        Ok(()) => {
                            self.show_first_run = false;
                            self.settings_status = match backup {
                                Some(b) => format!(
                                    "Saved. Backup: {}. Restart the gateway to apply.",
                                    b.file_name().unwrap_or_default().to_string_lossy()
                                ),
                                None => "Saved. Restart the gateway to apply.".to_string(),
                            };
                        }
                        Err(e) => self.settings_status = format!("Save failed: {e}"),
                    }
                }
            }
            super::settings::SettingsAction::Reload => {
                let cfg = load_config(None);
                self.settings = super::settings::SettingsForm::from_config(&cfg);
                self.settings_reveal.clear();
                self.settings_status = "Reloaded from config.json.".to_string();
            }
            super::settings::SettingsAction::RunGraphSetup => self.run_graph_setup(),
            super::settings::SettingsAction::TestEmailConnection => self.start_email_diag(false),
            super::settings::SettingsAction::SendTestEmail => self.start_email_diag(true),
            super::settings::SettingsAction::WriteDiagnostics => self.write_diagnostics(),
            super::settings::SettingsAction::RepairGraphAccess => self.run_graph_setup_repair(),
            super::settings::SettingsAction::OpenAdminConsent => self.open_admin_consent(),
            super::settings::SettingsAction::None => {}
        }
    }

    fn draw_models(&mut self, ui: &mut egui::Ui) {
        ui.heading("Models");
        ui.label(
            egui::RichText::new(format!("Active main model: {}", self.agent.model()))
                .color(ACCENT)
                .strong(),
        );
        ui.add_space(12.0);

        let choices = self.model_choices();
        let busy = self.models_rx.is_some();

        ui.label("Main model");
        ui.horizontal(|ui| {
            ui.add_enabled_ui(!busy, |ui| {
                if let Some(picked) =
                    Self::model_combo(ui, "main_model_picker", &self.model_main, &choices, 380.0)
                {
                    self.model_main = picked;
                }
            });
            ui.add(
                egui::TextEdit::singleline(&mut self.model_main)
                    .hint_text("or type an id")
                    .desired_width(220.0),
            );
        });

        ui.add_space(6.0);
        ui.label("Subagent model (empty = same as main; can be a cheap local model)");
        ui.horizontal(|ui| {
            ui.add_enabled_ui(!busy, |ui| {
                if let Some(picked) =
                    Self::model_combo(ui, "sub_model_picker", &self.model_sub, &choices, 380.0)
                {
                    self.model_sub = picked;
                }
            });
            ui.add(
                egui::TextEdit::singleline(&mut self.model_sub)
                    .hint_text("or type an id")
                    .desired_width(220.0),
            );
            if ui.small_button("clear").clicked() {
                self.model_sub.clear();
            }
        });
        ui.add_space(10.0);

        ui.horizontal(|ui| {
            if ui
                .add_enabled(self.pending.is_none(), egui::Button::new("✅ Apply & save"))
                .clicked()
            {
                self.apply_models();
            }
            if ui
                .add_enabled(self.model_test_rx.is_none(), egui::Button::new("🧪 Test model"))
                .clicked()
            {
                self.test_model();
            }
            if ui
                .add_enabled(!busy, egui::Button::new("🔄 Refresh models"))
                .clicked()
            {
                self.discover_models();
            }
        });
        if !self.model_status.is_empty() {
            ui.add_space(6.0);
            ui.label(&self.model_status);
        }

        if !self.available_models.is_empty() {
            ui.add_space(12.0);
            ui.separator();
            ui.label(
                egui::RichText::new("AVAILABLE MODELS (from ~/.metis/config.json)")
                    .small()
                    .strong()
                    .color(ACCENT),
            );
            ui.add_space(4.0);
            let models = self.available_models.clone();
            egui::ScrollArea::vertical().max_height(260.0).show(ui, |ui| {
                let mut current_provider = "";
                for m in &models {
                    if m.provider != current_provider {
                        current_provider = m.provider;
                        ui.add_space(6.0);
                        ui.label(
                            egui::RichText::new(m.provider_display)
                                .small()
                                .strong()
                                .color(egui::Color32::GRAY),
                        );
                    }
                    ui.horizontal(|ui| {
                        ui.label(&m.raw_id);
                        if m.is_chat_only() {
                            ui.label(
                                egui::RichText::new("⚠ chat-only (no tool support)")
                                    .small()
                                    .color(egui::Color32::from_rgb(180, 120, 0)),
                            );
                        }
                        if ui.small_button("use as main").clicked() {
                            self.model_main = m.id.clone();
                        }
                        if ui.small_button("use as subagent").clicked() {
                            self.model_sub = m.id.clone();
                        }
                    });
                }
            });
        }

        if !self.model_errors.is_empty() {
            ui.add_space(10.0);
            ui.label(
                egui::RichText::new("Providers that could not be listed")
                    .small()
                    .strong()
                    .color(egui::Color32::from_rgb(180, 120, 0)),
            );
            for e in &self.model_errors {
                ui.label(egui::RichText::new(e).small().weak());
            }
        }

        ui.add_space(12.0);
        ui.separator();
        ui.label(
            egui::RichText::new(
                "Switching rebuilds the agent (sessions and memory are kept — they live on disk) \
                 and persists to ~/.metis/config.json, so the CLI and gateway pick it up too. \
                 Cloud models need their API key configured via `metis onboard`.",
            )
            .small()
            .weak(),
        );
    }

    fn draw_cron(&mut self, ui: &mut egui::Ui) {
        ui.heading("Cron jobs");
        ui.label("Scheduled tasks from ~/.metis/cron/jobs.json");
        ui.add_space(8.0);
        if ui.button("Refresh").clicked() {
            self.cron_jobs = load_cron_jobs();
        }
        ui.separator();
        if self.cron_jobs.is_empty() {
            ui.label("No cron jobs. Add one with: metis cron add …");
            return;
        }
        egui::ScrollArea::vertical().show(ui, |ui| {
            for job in &self.cron_jobs {
                ui.group(|ui| {
                    ui.horizontal(|ui| {
                        ui.label(egui::RichText::new(&job.name).strong());
                        let status = if job.enabled { "enabled" } else { "disabled" };
                        ui.label(
                            egui::RichText::new(status)
                                .small()
                                .color(if job.enabled {
                                    egui::Color32::from_rgb(0, 128, 0)
                                } else {
                                    egui::Color32::GRAY
                                }),
                        );
                    });
                    ui.label(format!("ID: {}", job.id));
                    ui.label(format!("Message: {}", truncate(&job.payload.message, 120)));
                    ui.label(format!("Schedule: {:?}", job.schedule.kind));
                });
                ui.add_space(4.0);
            }
        });
    }
}

fn sidebar_nav_button(ui: &mut egui::Ui, nav: &mut NavPanel, panel: NavPanel, label: &str) {
    let selected = *nav == panel;
    if ui.selectable_label(selected, label).clicked() {
        *nav = panel;
    }
}

fn draw_placeholder(ui: &mut egui::Ui, title: &str, body: &str) {
    ui.vertical_centered(|ui| {
        ui.add_space(60.0);
        ui.heading(title);
        ui.add_space(12.0);
        ui.label(body);
    });
}

fn session_label(key: &str) -> String {
    key.split_once(':')
        .map(|(_, id)| id.to_string())
        .unwrap_or_else(|| key.to_string())
}

/// Split the agent's token-usage footer ("\n\n📊 …") off a wire reply.
/// Session history stores replies without the footer, so it must not go
/// into `chat_lines`; the caller surfaces it in the status line.
fn split_usage_footer(reply: &str) -> (String, Option<String>) {
    match reply.rfind("\n\n📊 ") {
        Some(pos) => (
            reply[..pos].to_string(),
            Some(reply[pos..].trim().to_string()),
        ),
        None => (reply.to_string(), None),
    }
}

fn split_session_key(key: &str) -> (String, String) {
    key.split_once(':')
        .map(|(c, id)| (c.to_string(), id.to_string()))
        .unwrap_or_else(|| ("desktop".into(), key.to_string()))
}

fn message_display(msg: &Message) -> Option<(&'static str, String)> {
    match msg {
        Message::User { content } => {
            let text = match content {
                MessageContent::Text(t) => t.clone(),
                MessageContent::Parts(parts) => parts
                    .iter()
                    .filter_map(|p| match p {
                        metis_core::types::ContentPart::Text { text } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n"),
            };
            Some(("You", text))
        }
        Message::Assistant {
            content,
            tool_calls,
            ..
        } => {
            if let Some(c) = content {
                Some(("Agent", c.clone()))
            } else if tool_calls.is_some() {
                Some(("Agent", "[running tools…]".into()))
            } else {
                None
            }
        }
        // System entries are internal bookkeeping for the model (the tool
        // log, role notes). Showing them put "[Internal tool log ...]" on the
        // person's screen as if it were part of the conversation.
        Message::System { .. } => None,
        Message::Tool { .. } => None,
    }
}

/// Body text colour for secondary panels - dark enough to read, unlike
/// egui's "weak" grey.
const READABLE_TEXT: egui::Color32 = egui::Color32::from_rgb(40, 44, 52);

/// The model's reasoning: full-size dark text on a tinted panel, selectable.
fn draw_thinking(ui: &mut egui::Ui, text: &str) {
    egui::Frame::none()
        .fill(egui::Color32::from_rgb(255, 250, 232))
        .stroke(egui::Stroke::new(1.0, egui::Color32::from_rgb(235, 220, 170)))
        .rounding(4.0)
        .inner_margin(8.0)
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.add(
                egui::Label::new(egui::RichText::new(text).size(14.0).color(READABLE_TEXT))
                    .selectable(true),
            );
        });
}

/// Under each reply: what was asked and what really happened, from Metis's
/// record rather than the agent's own account.
fn draw_conclusion(ui: &mut egui::Ui, line: &ChatLine) {
    egui::Frame::none()
        .fill(egui::Color32::from_rgb(236, 247, 238))
        .stroke(egui::Stroke::new(1.0, egui::Color32::from_rgb(180, 220, 188)))
        .rounding(6.0)
        .inner_margin(8.0)
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.label(
                egui::RichText::new("📌 Conclusion - checked by Metis")
                    .strong()
                    .color(egui::Color32::from_rgb(30, 110, 50)),
            )
            .on_hover_text("Built from the tools that really ran, not from the agent's reply.");
            if let Some(asked) = &line.asked {
                ui.label(egui::RichText::new(format!("Asked: {asked}")).color(READABLE_TEXT));
            }
            match &line.record {
                Some(lines) => {
                    for l in lines {
                        let failed = l.contains("FAILED") || l.contains("NOT RUN") || l.contains("did NOT");
                        let color = if failed { egui::Color32::from_rgb(180, 40, 40) } else { READABLE_TEXT };
                        ui.label(egui::RichText::new(format!("• {l}")).color(color));
                    }
                }
                None => {
                    ui.label(
                        egui::RichText::new(
                            "• No tool ran - nothing was read, run or changed. The reply is the model's own words.",
                        )
                        .color(READABLE_TEXT),
                    );
                }
            }
            if line.text.contains("⚠ Unverified") {
                ui.label(
                    egui::RichText::new("• Some numbers in the reply were not found in any tool output (see ⚠ above).")
                        .color(egui::Color32::from_rgb(180, 40, 40)),
                );
            }
        });
}

/// First non-blank line of `s`, cut to `n` characters.
fn first_line(s: &str, n: usize) -> String {
    let one = s.lines().find(|l| !l.trim().is_empty()).unwrap_or("").trim();
    truncate(one, n)
}

/// One line for a step of a turn (tool call, its result, a hand-off), or
/// None for events that are not steps.
fn event_step(e: &metis_core::events::Event) -> Option<String> {
    use metis_core::events::{EventKind, ExecStatus, ToolOutcome};
    let who = if e.agent == "main" { String::new() } else { format!("[{}] ", e.agent) };
    Some(match &e.event {
        EventKind::ToolCall { tool, args, .. } => {
            format!("{who}→ {tool} {}", first_line(&args.to_string(), 110))
        }
        EventKind::ToolResult { tool, outcome, .. } => {
            let mark = match outcome {
                ToolOutcome::Exec { status: ExecStatus::Success, exit_code, .. } => {
                    format!("✓ exit {}", exit_code.unwrap_or(0))
                }
                ToolOutcome::Exec { status: ExecStatus::NotRun, .. } => "⋯ NOT RUN".into(),
                ToolOutcome::Exec { status: ExecStatus::Timeout, .. } => "✗ timed out".into(),
                ToolOutcome::Exec { status: ExecStatus::Failed, exit_code, .. } => {
                    format!("✗ failed (exit {})", exit_code.map(|c| c.to_string()).unwrap_or("?".into()))
                }
                ToolOutcome::Ok { .. } => "✓".into(),
                ToolOutcome::Failed { error } => format!("✗ {}", first_line(error, 90)),
            };
            format!("{who}← {tool} {mark}")
        }
        EventKind::Delegation { to_agent, task } => {
            format!("⇢ handed to {to_agent}: {}", first_line(task, 100))
        }
        EventKind::DelegationResult { from_agent, text } => {
            format!("⇠ {from_agent} answered: {}", first_line(text, 100))
        }
        _ => return None,
    })
}

/// Split Metis's own record ("What actually happened (recorded by Metis…)")
/// off a delivered reply: (the agent's words, the record's lines).
fn split_record(text: &str) -> (String, Option<Vec<String>>) {
    const HEAD: &str = "What actually happened (recorded by Metis";
    let Some(at) = text.find(HEAD) else {
        return (text.to_string(), None);
    };
    let body = text[..at].trim_end().trim_end_matches('─').trim_end().to_string();
    let lines = text[at..]
        .lines()
        .skip(1)
        .map(|l| l.trim().trim_start_matches('•').trim().to_string())
        .filter(|l| !l.is_empty())
        .collect();
    (body, Some(lines))
}

/// Build the chat view from a session's event log: the person's messages,
/// each reply as it was delivered, and - collapsed under it - the model's
/// reasoning and the steps the turn really took.
fn chat_from_events(events: &[metis_core::events::Event]) -> Vec<ChatLine> {
    use metis_core::events::EventKind;
    let mut lines: Vec<ChatLine> = Vec::new();
    let mut thinking: Vec<String> = Vec::new();
    let mut steps: Vec<String> = Vec::new();
    let mut asked: Option<String> = None;
    let mut current_turn = String::new();
    for e in events {
        if e.turn != current_turn {
            thinking.clear();
            steps.clear();
            current_turn = e.turn.clone();
        }
        if let Some(step) = event_step(e) {
            steps.push(step);
            continue;
        }
        let who = if e.agent == "main" { String::new() } else { format!("[{}] ", e.agent) };
        match &e.event {
            EventKind::UserMessage { text } => {
                asked = Some(first_line(text, 160));
                lines.push(ChatLine {
                    role: "You",
                    text: text.clone(),
                    ..Default::default()
                })
            }
            EventKind::Thinking { text } => thinking.push(format!("{who}{}", text.trim())),
            EventKind::AgentReply { text } => {
                let (body, record) = split_record(text);
                lines.push(ChatLine {
                    role: "Agent",
                    text: body,
                    thinking: (!thinking.is_empty()).then(|| thinking.join("

")),
                    steps: std::mem::take(&mut steps),
                    record,
                    asked: asked.take(),
                })
            }
            _ => {}
        }
    }
    lines
}

/// The turn still in progress: its steps and the latest reasoning, i.e.
/// everything after the last message the person sent.
fn live_turn(events: &[metis_core::events::Event]) -> (Vec<String>, Option<String>) {
    use metis_core::events::EventKind;
    let start = events
        .iter()
        .rposition(|e| matches!(e.event, EventKind::UserMessage { .. }))
        .map(|i| i + 1)
        .unwrap_or(events.len());
    let tail = &events[start..];
    let steps = tail.iter().filter_map(event_step).collect();
    let thinking = tail.iter().rev().find_map(|e| match &e.event {
        EventKind::Thinking { text } => Some(text.trim().to_string()),
        _ => None,
    });
    (steps, thinking)
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(max).collect::<String>())
    }
}

fn load_cron_jobs() -> Vec<CronJob> {
    let path = get_data_path().join("cron").join("jobs.json");
    std::fs::read_to_string(path)
        .ok()
        .and_then(|data| serde_json::from_str::<CronStore>(&data).ok())
        .map(|store| store.jobs)
        .unwrap_or_default()
}

fn setup_theme(ctx: &egui::Context) {
    let mut style = (*ctx.style()).clone();
    style.visuals.window_fill = MAIN_BG;
    style.visuals.panel_fill = MAIN_BG;
    style.spacing.item_spacing = egui::vec2(8.0, 6.0);
    ctx.set_style(style);
}

/// Launch the native desktop window (blocks until closed).
pub fn run(logs: bool) -> Result<()> {
    crate::agent_builder::init_logging(logs);

    let metis_config = load_config(None);
    let desktop_config = load_desktop_config();
    if !desktop_config_path().exists() {
        let _ = save_desktop_config(&desktop_config);
    }

    let runtime = Runtime::new()?;
    let agent = Arc::new(build_agent_loop(&metis_config)?);
    let sessions = Arc::new(SessionManager::new(None)?);

    // Destructive changes are put to the person in a dialog, not approved by
    // the agent itself.
    let (approval_tx, approval_rx) = tokio::sync::mpsc::unbounded_channel();
    agent
        .approvals()
        .set_handler(Arc::new(super::approval_ui::DesktopApprovalHandler::new(approval_tx)));

    let title = desktop_config.agent_title.clone();
    let width = desktop_config.window.width;
    let height = desktop_config.window.height;

    // The window is opened at most once, but the attempt may be made twice
    // with different renderers, so the pieces the app owns live behind a
    // take-once cell rather than being moved into a single closure.
    let parts = Arc::new(std::sync::Mutex::new(Some((
        desktop_config,
        agent,
        sessions,
        runtime,
        approval_rx,
    ))));

    let attempt = |renderer: eframe::Renderer| -> std::result::Result<(), eframe::Error> {
        let parts = parts.clone();
        eframe::run_native(
            &title,
            eframe::NativeOptions {
                viewport: egui::ViewportBuilder::default()
                    .with_inner_size([width, height])
                    .with_title("Metis Desktop"),
                renderer,
                ..Default::default()
            },
            Box::new(move |cc| {
                setup_theme(&cc.egui_ctx);
                let (config, agent, sessions, runtime, approval_rx) = parts
                    .lock()
                    .expect("desktop state mutex poisoned")
                    .take()
                    .expect("desktop app state was already consumed");
                Ok(Box::new(MetisDesktopApp::new(
                    config, agent, sessions, runtime, approval_rx,
                )))
            }),
        )
    };

    // An explicit choice wins, for when the automatic one guesses wrong.
    match std::env::var("METIS_DESKTOP_RENDERER")
        .unwrap_or_default()
        .trim()
        .to_lowercase()
        .as_str()
    {
        "wgpu" => return attempt(eframe::Renderer::Wgpu).map_err(|e| desktop_error(e, true)),
        "glow" => return attempt(eframe::Renderer::Glow).map_err(|e| desktop_error(e, false)),
        _ => {}
    }

    match attempt(eframe::Renderer::Glow) {
        Ok(()) => Ok(()),
        Err(e) if is_opengl_failure(&e) => {
            // Windows ships only a 1.1 software OpenGL when no GPU driver is
            // installed, so this is the normal case on a fresh machine, a VM,
            // or over Remote Desktop - not an exotic failure.
            tracing::warn!(error = %e, "OpenGL unavailable; retrying with the Direct3D renderer");
            eprintln!("  OpenGL is not available here; retrying with Direct3D…");
            attempt(eframe::Renderer::Wgpu).map_err(|e2| desktop_error(e2, true))
        }
        Err(e) => Err(desktop_error(e, false)),
    }
}

/// Does this failure mean "this machine has no usable OpenGL"?
fn is_opengl_failure(e: &eframe::Error) -> bool {
    let msg = e.to_string().to_lowercase();
    msg.contains("opengl") || msg.contains("glow") || msg.contains("glutin")
}

/// Turn a renderer failure into something the reader can act on.
fn desktop_error(e: eframe::Error, tried_fallback: bool) -> anyhow::Error {
    if tried_fallback {
        anyhow::anyhow!(
            "desktop GUI could not start: {e}\n\n             Neither OpenGL nor Direct3D could open a window here. That usually means a \
             Remote Desktop session or a virtual machine with no graphics driver.\n\
             Metis does not need the GUI: `metis gateway` runs the channels and `metis agent` \
             gives you a chat prompt. Settings can be edited directly in \
             ~/.metis/config.json.\n\
             To force one renderer, set METIS_DESKTOP_RENDERER=glow or =wgpu."
        )
    } else {
        anyhow::anyhow!("desktop GUI error: {e}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(turn: &str, event: metis_core::events::EventKind) -> metis_core::events::Event {
        metis_core::events::Event {
            ts: chrono::Utc::now(),
            session: "desktop:t".into(),
            turn: turn.into(),
            agent: "main".into(),
            event,
        }
    }

    #[test]
    fn the_metis_record_is_split_from_the_reply_into_the_conclusion() {
        use metis_core::events::{EventKind, ToolOutcome};
        let reply = "Edited gui.py.\n\n──────────\nWhat actually happened (recorded by Metis, not written by the agent):\n\
                     • main: read 1 file · edited gui.py\n• main: FAILED, file NOT changed: edit_file x.py: boom";
        let events = vec![
            ev("t1", EventKind::UserMessage { text: "fix the lowercase bug".into() }),
            ev("t1", EventKind::Thinking { text: "look at load_db".into() }),
            ev("t1", EventKind::ToolResult { call_id: "c".into(), tool: "read_file".into(), outcome: ToolOutcome::Ok { output: "x".into(), truncated: false } }),
            ev("t1", EventKind::AgentReply { text: reply.into() }),
        ];
        let lines = chat_from_events(&events);
        let agent = &lines[1];
        assert_eq!(agent.text, "Edited gui.py.");
        assert_eq!(agent.asked.as_deref(), Some("fix the lowercase bug"));
        let record = agent.record.as_ref().unwrap();
        assert_eq!(record[0], "main: read 1 file · edited gui.py");
        assert!(record[1].contains("FAILED"));
        assert_eq!(agent.steps, vec!["← read_file ✓".to_string()]);
    }

    #[test]
    fn live_progress_shows_only_the_turn_in_progress() {
        use metis_core::events::{EventKind, ToolOutcome};
        let events = vec![
            ev("t1", EventKind::UserMessage { text: "old".into() }),
            ev("t1", EventKind::ToolResult { call_id: "a".into(), tool: "list_dir".into(), outcome: ToolOutcome::Ok { output: String::new(), truncated: false } }),
            ev("t1", EventKind::AgentReply { text: "done".into() }),
            ev("t2", EventKind::UserMessage { text: "new".into() }),
            ev("t2", EventKind::Thinking { text: "first".into() }),
            ev("t2", EventKind::ToolResult { call_id: "b".into(), tool: "read_file".into(), outcome: ToolOutcome::Failed { error: "no such file".into() } }),
            ev("t2", EventKind::Thinking { text: "second".into() }),
        ];
        let (steps, thinking) = live_turn(&events);
        assert_eq!(steps, vec!["← read_file ✗ no such file".to_string()]);
        assert_eq!(thinking.as_deref(), Some("second"));
    }

    #[test]
    fn attachments_split_into_images_and_inlined_text() {
        let dir = tempfile::tempdir().unwrap();
        let txt = dir.path().join("notes.txt");
        let img = dir.path().join("shot.PNG");
        let bin = dir.path().join("blob.bin");
        std::fs::write(&txt, "hello world").unwrap();
        std::fs::write(&img, [1u8, 2, 3]).unwrap();
        std::fs::write(&bin, [0u8, 159, 146]).unwrap();
        let (text, images) = compose_with_attachments("", &[txt, img.clone(), bin]);
        assert!(text.starts_with("Please look at the attached"));
        assert!(text.contains("hello world"));
        assert_eq!(images, vec![img.display().to_string()]);
        assert!(text.contains("open it with your file tools"));
    }

    #[test]
    fn setup_script_is_locatable_from_the_binary() {
        // The button is useless if the script cannot be found from wherever
        // the binary sits. The test binary lives deeper (target/debug/deps),
        // so finding it from there also covers the normal exe location.
        let found = MetisDesktopApp::find_setup_script();
        assert!(
            found.is_some(),
            "setup-o365-graph.ps1 not found by walking up from the test binary"
        );
        let p = found.unwrap();
        assert!(p.ends_with("setup-o365-graph.ps1"));
        assert!(p.is_file());
    }

    #[test]
    fn embedded_script_is_the_real_script() {
        // The button must work on a machine that has only metis.exe, so the
        // script is compiled in. Guard against it being empty or truncated.
        let s = MetisDesktopApp::SETUP_SCRIPT;
        assert!(s.len() > 2000, "embedded script looks truncated: {} bytes", s.len());
        assert!(s.contains("param("), "missing param block");
        assert!(s.contains("-Mailbox") || s.contains("$Mailbox"), "missing Mailbox param");
        assert!(s.contains("Connect-MgGraph"), "missing the sign-in step");
        assert!(s.contains("Install-PackageProvider"), "missing the NuGet bootstrap");
    }

    #[test]
    fn resolve_setup_script_always_yields_a_runnable_file() {
        let p = MetisDesktopApp::resolve_setup_script().expect("must resolve");
        assert!(p.is_file());
        let body = std::fs::read_to_string(&p).unwrap();
        assert!(body.contains("Connect-MgGraph"));
    }
}
