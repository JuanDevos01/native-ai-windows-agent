//! egui mission-control desktop application.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;
use eframe::egui;
use metis_agent::AgentLoop;
use metis_core::config::{load_config, save_config};
use metis_core::session::{SessionManager, SessionSummary};
use metis_core::types::{Message, MessageContent};
use metis_cron::types::{CronJob, CronStore};
use metis_core::utils::get_data_path;
use tokio::runtime::Runtime;
use tokio::sync::oneshot;

use super::config::{desktop_config_path, load_desktop_config, save_desktop_config, DesktopConfig};
use super::models::discover_available_models;
use super::models::model_menu_label;
use crate::agent_builder::{build_agent_loop, provider_for_model};

const ACCENT: egui::Color32 = egui::Color32::from_rgb(0, 102, 204);
const SIDEBAR_BG: egui::Color32 = egui::Color32::from_rgb(248, 249, 251);
const MAIN_BG: egui::Color32 = egui::Color32::from_rgb(255, 255, 255);

#[derive(Clone, Copy, PartialEq, Eq)]
enum NavPanel {
    Chat,
    SkillsTools,
    Messaging,
    Artifacts,
    CronJobs,
}

struct ChatLine {
    role: &'static str,
    text: String,
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
    selected_model: String,
    model_choices: Vec<String>,
    last_model_refresh: f64,
}

impl MetisDesktopApp {
    fn new(
        config: DesktopConfig,
        selected_model: String,
        agent: Arc<AgentLoop>,
        sessions: Arc<SessionManager>,
        runtime: Runtime,
    ) -> Self {
        let active = format!(
            "desktop:{}",
            chrono::Utc::now().format("%Y%m%d-%H%M%S")
        );
        let metis_config = load_config(None);
        let model_choices = discover_available_models(&metis_config, &config);
        let mut app = Self {
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
            selected_model,
            model_choices,
            last_model_refresh: 0.0,
        };
        app.refresh_sessions();
        app.load_session_history();
        app
    }

    fn refresh_sessions(&mut self) {
        self.sessions_cache = self.sessions.list_sessions();
    }

    fn load_session_history(&mut self) {
        self.chat_lines.clear();
        for msg in self.sessions.get_history(&self.active_session, 200) {
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
                .process_chat_session(&channel, &chat_id, &text)
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
        let mut dropped = false;
        if let Some(pending) = self.pending.as_mut() {
            match pending.rx.try_recv() {
                Ok(result) => finished = Some(result),
                Err(tokio::sync::oneshot::error::TryRecvError::Closed) => dropped = true,
                Err(tokio::sync::oneshot::error::TryRecvError::Empty) => {}
            }
        }
        if dropped {
            self.pending = None;
            self.status_line = "Error: agent task failed unexpectedly".into();
            ctx.request_repaint();
            return;
        }
        if let Some(result) = finished {
            let pending = self.pending.take().unwrap();
            match result {
                Ok(reply) => {
                    if !reply.trim().is_empty() {
                        self.chat_lines.push(ChatLine {
                            role: "Agent",
                            text: reply,
                        });
                    }
                    self.status_line.clear();
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

    fn refresh_model_choices(&mut self) {
        let metis_config = load_config(None);
        self.model_choices = discover_available_models(&metis_config, &self.config);
        if !self.selected_model.is_empty()
            && !self.model_choices.iter().any(|m| m == &self.selected_model)
        {
            self.model_choices.insert(0, self.selected_model.clone());
        }
    }

    fn remember_custom_model(&mut self, model: &str) {
        let model = model.trim();
        if model.is_empty() {
            return;
        }
        if !self.config.extra_models.iter().any(|m| m == model) {
            self.config.extra_models.push(model.to_string());
            let _ = save_desktop_config(&self.config);
        }
    }

    fn apply_model(&mut self, model: String) {
        let model = model.trim().to_string();
        if model.is_empty() || model == self.agent.model() {
            self.selected_model = model;
            return;
        }
        let mut config = load_config(None);
        match provider_for_model(&config, &model) {
            Ok(provider) => {
                self.agent.set_active_model(model.clone(), provider);
                config.agents.defaults.model = model.clone();
                if let Err(e) = save_config(&config, None) {
                    self.status_line = format!("Model active, but config save failed: {e}");
                } else {
                    self.status_line = format!("Model: {model}");
                }
                self.selected_model = model.clone();
                self.remember_custom_model(&model);
                self.refresh_model_choices();
            }
            Err(e) => {
                self.status_line = format!("Model error: {e}");
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

        let now = ctx.input(|i| i.time);
        if now - self.last_refresh > 5.0 {
            self.refresh_sessions();
            self.cron_jobs = load_cron_jobs();
            self.last_refresh = now;
        }
        if now - self.last_model_refresh > 30.0 {
            self.refresh_model_choices();
            self.last_model_refresh = now;
        }

        egui::TopBottomPanel::bottom("input_panel").show(ctx, |ui| {
            ui.add_space(8.0);
            let metis_config = load_config(None);
            let selected_label = model_menu_label(&self.selected_model, &metis_config);
            ui.horizontal(|ui| {
                ui.add_space(12.0);
                ui.label(egui::RichText::new("Model").small().weak());
                let mut pick: Option<String> = None;
                egui::ComboBox::from_id_salt("desktop_model")
                    .selected_text(truncate(&selected_label, 56))
                    .width(320.0)
                    .show_ui(ui, |ui| {
                        for model in &self.model_choices {
                            let label = model_menu_label(model, &metis_config);
                            if ui
                                .selectable_label(self.selected_model == *model, label)
                                .clicked()
                            {
                                pick = Some(model.clone());
                            }
                        }
                    });
                if let Some(model) = pick {
                    self.apply_model(model);
                }
                if ui.small_button("↻").on_hover_text("Refresh installed models").clicked() {
                    self.refresh_model_choices();
                    self.last_model_refresh = ctx.input(|i| i.time);
                }
            });
            ui.horizontal(|ui| {
                ui.add_space(12.0);
                let w = ui.available_width() - 80.0;
                let response = ui.add(
                    egui::TextEdit::multiline(&mut self.input)
                        .hint_text("Start with a goal…")
                        .desired_width(w)
                        .desired_rows(2),
                );
                let enter_send = ui.input(|i| {
                    i.key_pressed(egui::Key::Enter) && !i.modifiers.shift && !i.modifiers.ctrl
                });
                let send = ui
                    .add_enabled(
                        self.pending.is_none() && !self.input.trim().is_empty(),
                        egui::Button::new("Send"),
                    )
                    .clicked();
                if (enter_send && response.has_focus()) || send {
                    self.send_message();
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
                sidebar_nav_button(ui, &mut self.nav, NavPanel::SkillsTools, "🛠  Skills & Tools");
                sidebar_nav_button(ui, &mut self.nav, NavPanel::Messaging, "📨  Messaging");
                sidebar_nav_button(ui, &mut self.nav, NavPanel::Artifacts, "📁  Artifacts");
                sidebar_nav_button(ui, &mut self.nav, NavPanel::CronJobs, "⏱  Cron jobs");

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
                    .max_height(180.0)
                    .show(ui, |ui| {
                        for (key, selected) in &session_rows {
                            let label = session_label(key);
                            let resp = ui.selectable_label(*selected, label);
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
                egui::ScrollArea::vertical().show(ui, |ui| {
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
                            if ui.small_button(label).clicked() {
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
                NavPanel::CronJobs => self.draw_cron(ui),
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
                egui::RichText::new(format!("Session: {} · Model: {}", self.active_session, self.selected_model))
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
    let selected_model = metis_config.agents.defaults.model.clone();
    let desktop_config = load_desktop_config();
    if !desktop_config_path().exists() {
        let _ = save_desktop_config(&desktop_config);
    }

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let sessions = Arc::new(SessionManager::new(None)?);
    let agent = Arc::new(build_agent_loop(&metis_config, Some(Arc::clone(&sessions)))?);

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
                selected_model,
                agent,
                sessions,
                runtime,
            )))
        }),
    )
    .map_err(|e| anyhow::anyhow!("desktop GUI error: {e}"))
}
