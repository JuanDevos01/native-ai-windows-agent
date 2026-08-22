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
    Models,
    SkillsTools,
    Messaging,
    Artifacts,
    CronJobs,
}

struct ChatLine {
    role: &'static str,
    text: String,
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
            config,
            agent,
            sessions,
            runtime,
            nav: NavPanel::Chat,
            session_search: String::new(),
            active_session: active,
            chat_lines: Vec::new(),
            input: String::new(),
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
        // Disk-fresh read: the agent writes sessions through its own manager
        // instance, so a cached read here would show stale (often empty)
        // history and blank the chat right after a reply arrives.
        for msg in self.sessions.get_history_fresh(&self.active_session, 200) {
            if let Some((role, text)) = message_display(&msg) {
                if !text.trim().is_empty() {
                    self.chat_lines.push(ChatLine { role, text });
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
        self.nav = NavPanel::Chat;
        self.status_line = "New session".into();
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
        let text = self.input.trim().to_string();
        if text.is_empty() || self.pending.is_some() {
            return;
        }
        self.chat_lines.push(ChatLine {
            role: "You",
            text: text.clone(),
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
                .process_direct_session(&channel, &chat_id, &text)
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
        self.poll_pending(ctx);
        self.poll_model_discovery(ctx);
        self.poll_model_test(ctx);

        let now = ctx.input(|i| i.time);
        if now - self.last_refresh > 5.0 {
            self.refresh_sessions();
            self.cron_jobs = load_cron_jobs();
            self.last_refresh = now;
        }

        egui::TopBottomPanel::bottom("input_panel").show(ctx, |ui| {
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                ui.add_space(12.0);
                let w = ui.available_width() - 270.0;
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
                        self.pending.is_none() && !self.input.trim().is_empty(),
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
                for line in &self.chat_lines {
                    ui.horizontal(|ui| {
                        let color = if line.role == "You" {
                            egui::Color32::DARK_GRAY
                        } else {
                            ACCENT
                        };
                        ui.label(egui::RichText::new(format!("{}:", line.role)).strong().color(color));
                    });
                    ui.label(&line.text);
                    ui.add_space(12.0);
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
        Message::System { content } => Some(("System", content.clone())),
        Message::Tool { .. } => None,
    }
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

    let title = desktop_config.agent_title.clone();
    let width = desktop_config.window.width;
    let height = desktop_config.window.height;

    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([width, height])
            .with_title("Metis Desktop"),
        ..Default::default()
    };

    eframe::run_native(
        &title,
        native_options,
        Box::new(move |cc| {
            setup_theme(&cc.egui_ctx);
            Ok(Box::new(MetisDesktopApp::new(
                desktop_config,
                agent,
                sessions,
                runtime,
            )))
        }),
    )
    .map_err(|e| anyhow::anyhow!("desktop GUI error: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

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
