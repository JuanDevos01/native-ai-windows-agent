//! Structured settings editor for the desktop app.
//!
//! Everything here edits `~/.metis/config.json`. It is deliberately a form of
//! typed fields rather than a JSON text box: the config has enough
//! interdependent settings (an email backend that changes which fields even
//! apply, a permission mode with three valid values) that free-text editing
//! mostly produces invalid files.
//!
//! Secrets are masked by default with a per-field reveal toggle, so the panel
//! can be opened — or screen-shared — without exposing API keys and tokens.
//!
//! Values are held as strings while editing and parsed on save. Editing
//! numbers directly as `u32` fights the user (you cannot transiently clear a
//! field), so parsing is deferred and bad input keeps the previous value
//! rather than resetting to zero.

use std::collections::HashSet;

use metis_core::config::Config as MetisConfig;

const ACCENT: egui::Color32 = egui::Color32::from_rgb(0, 102, 204);
const WARN: egui::Color32 = egui::Color32::from_rgb(180, 95, 0);

/// What the panel wants the app to do after this frame.
#[derive(PartialEq)]
pub enum SettingsAction {
    None,
    Save,
    Reload,
}

/// Editable mirror of the parts of `MetisConfig` worth exposing in a GUI.
pub struct SettingsForm {
    // ── Agent ──
    pub workspace: String,
    pub model: String,
    pub subagent_model: String,
    pub max_tokens: String,
    pub temperature: String,
    pub max_tool_iterations: String,
    pub chat_context_length: String,
    pub show_token_usage: bool,
    pub log_thinking_json: bool,
    pub include_fenced_code: bool,
    pub include_exec_output: bool,

    // ── Providers (api keys) ──
    pub providers: Vec<(String, String)>,

    // ── Channels ──
    pub telegram_token: String,
    pub telegram_allowed: String,
    pub discord_token: String,
    pub discord_allowed: String,
    pub whatsapp_bridge_url: String,
    pub slack_bot_token: String,
    pub slack_app_token: String,

    // Email — provider decides which block below applies.
    pub email_provider: String,
    pub email_imap_host: String,
    pub email_imap_port: String,
    pub email_imap_username: String,
    pub email_imap_password: String,
    pub email_imap_mailbox: String,
    pub email_smtp_host: String,
    pub email_smtp_port: String,
    pub email_smtp_username: String,
    pub email_smtp_password: String,
    pub email_from_address: String,
    pub email_graph_tenant_id: String,
    pub email_graph_client_id: String,
    pub email_graph_client_secret: String,
    pub email_graph_user_id: String,
    pub email_allowed: String,

    // ── Tools ──
    pub exec_timeout: String,
    pub exec_shell: String,
    pub exec_permission_mode: String,
    pub restrict_to_workspace: bool,
    pub web_search_key: String,

    // ── Memory / heartbeat ──
    pub memory_enabled: bool,
    pub memory_compaction_threshold: String,
    pub memory_keep_recent: String,
    pub heartbeat_enabled: bool,
    pub heartbeat_interval_minutes: String,
}

fn join(list: &[String]) -> String {
    list.join(", ")
}

fn split(s: &str) -> Vec<String> {
    s.split(',')
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .collect()
}

impl SettingsForm {
    pub fn from_config(c: &MetisConfig) -> Self {
        let d = &c.agents.defaults;
        let e = &c.channels.email;
        let p = &c.providers;
        Self {
            workspace: d.workspace.clone(),
            model: d.model.clone(),
            subagent_model: d.subagent_model.clone(),
            max_tokens: d.max_tokens.to_string(),
            temperature: d.temperature.to_string(),
            max_tool_iterations: d.max_tool_iterations.to_string(),
            chat_context_length: d.chat_context_length.to_string(),
            show_token_usage: d.show_token_usage,
            log_thinking_json: d.log_thinking_json,
            include_fenced_code: d.include_fenced_code_in_chat_apps,
            include_exec_output: d.include_exec_output_in_chat_apps,

            providers: vec![
                ("anthropic".into(), p.anthropic.api_key.clone()),
                ("openai".into(), p.openai.api_key.clone()),
                ("openrouter".into(), p.openrouter.api_key.clone()),
                ("deepseek".into(), p.deepseek.api_key.clone()),
                ("groq".into(), p.groq.api_key.clone()),
                ("gemini".into(), p.gemini.api_key.clone()),
                ("moonshot".into(), p.moonshot.api_key.clone()),
                ("minimax".into(), p.minimax.api_key.clone()),
                ("zhipu".into(), p.zhipu.api_key.clone()),
                ("dashscope".into(), p.dashscope.api_key.clone()),
                ("aihubmix".into(), p.aihubmix.api_key.clone()),
                ("vllm".into(), p.vllm.api_key.clone()),
                ("ollama".into(), p.ollama.api_key.clone()),
                ("lmstudio".into(), p.lmstudio.api_key.clone()),
            ],

            telegram_token: c.channels.telegram.token.clone(),
            telegram_allowed: join(&c.channels.telegram.allowed_users),
            discord_token: c.channels.discord.token.clone(),
            discord_allowed: join(&c.channels.discord.allowed_users),
            whatsapp_bridge_url: c.channels.whatsapp.bridge_url.clone(),
            slack_bot_token: c.channels.slack.bot_token.clone(),
            slack_app_token: c.channels.slack.app_token.clone(),

            email_provider: if e.provider.trim().is_empty() {
                "imap".into()
            } else {
                e.provider.clone()
            },
            email_imap_host: e.imap_host.clone(),
            email_imap_port: e.imap_port.to_string(),
            email_imap_username: e.imap_username.clone(),
            email_imap_password: e.imap_password.clone(),
            email_imap_mailbox: e.imap_mailbox.clone(),
            email_smtp_host: e.smtp_host.clone(),
            email_smtp_port: e.smtp_port.to_string(),
            email_smtp_username: e.smtp_username.clone(),
            email_smtp_password: e.smtp_password.clone(),
            email_from_address: e.from_address.clone(),
            email_graph_tenant_id: e.graph_tenant_id.clone(),
            email_graph_client_id: e.graph_client_id.clone(),
            email_graph_client_secret: e.graph_client_secret.clone(),
            email_graph_user_id: e.graph_user_id.clone(),
            email_allowed: join(&e.allowed_users),

            exec_timeout: c.tools.exec.timeout.to_string(),
            exec_shell: c.tools.exec.shell.clone(),
            exec_permission_mode: c.tools.exec.permission_mode.clone(),
            restrict_to_workspace: c.tools.restrict_to_workspace,
            web_search_key: c.tools.web.search.api_key.clone(),

            memory_enabled: c.memory.enabled,
            memory_compaction_threshold: c.memory.compaction_threshold.to_string(),
            memory_keep_recent: c.memory.keep_recent.to_string(),
            heartbeat_enabled: c.heartbeat.enabled,
            heartbeat_interval_minutes: c.heartbeat.interval_minutes.to_string(),
        }
    }

    /// Write the form back into a config. Unparseable numbers keep the value
    /// already in the config rather than silently becoming 0.
    pub fn apply_to(&self, c: &mut MetisConfig) {
        let d = &mut c.agents.defaults;
        d.workspace = self.workspace.trim().to_string();
        d.model = self.model.trim().to_string();
        d.subagent_model = self.subagent_model.trim().to_string();
        d.max_tokens = self.max_tokens.trim().parse().unwrap_or(d.max_tokens);
        d.temperature = self.temperature.trim().parse().unwrap_or(d.temperature);
        d.max_tool_iterations = self
            .max_tool_iterations
            .trim()
            .parse()
            .unwrap_or(d.max_tool_iterations);
        d.chat_context_length = self
            .chat_context_length
            .trim()
            .parse()
            .unwrap_or(d.chat_context_length);
        d.show_token_usage = self.show_token_usage;
        d.log_thinking_json = self.log_thinking_json;
        d.include_fenced_code_in_chat_apps = self.include_fenced_code;
        d.include_exec_output_in_chat_apps = self.include_exec_output;

        for (name, key) in &self.providers {
            let slot = match name.as_str() {
                "anthropic" => &mut c.providers.anthropic.api_key,
                "openai" => &mut c.providers.openai.api_key,
                "openrouter" => &mut c.providers.openrouter.api_key,
                "deepseek" => &mut c.providers.deepseek.api_key,
                "groq" => &mut c.providers.groq.api_key,
                "gemini" => &mut c.providers.gemini.api_key,
                "moonshot" => &mut c.providers.moonshot.api_key,
                "minimax" => &mut c.providers.minimax.api_key,
                "zhipu" => &mut c.providers.zhipu.api_key,
                "dashscope" => &mut c.providers.dashscope.api_key,
                "aihubmix" => &mut c.providers.aihubmix.api_key,
                "vllm" => &mut c.providers.vllm.api_key,
                "ollama" => &mut c.providers.ollama.api_key,
                "lmstudio" => &mut c.providers.lmstudio.api_key,
                _ => continue,
            };
            *slot = key.trim().to_string();
        }

        c.channels.telegram.token = self.telegram_token.trim().to_string();
        c.channels.telegram.allowed_users = split(&self.telegram_allowed);
        c.channels.discord.token = self.discord_token.trim().to_string();
        c.channels.discord.allowed_users = split(&self.discord_allowed);
        c.channels.whatsapp.bridge_url = self.whatsapp_bridge_url.trim().to_string();
        c.channels.slack.bot_token = self.slack_bot_token.trim().to_string();
        c.channels.slack.app_token = self.slack_app_token.trim().to_string();

        let e = &mut c.channels.email;
        e.provider = self.email_provider.trim().to_string();
        e.imap_host = self.email_imap_host.trim().to_string();
        e.imap_port = self.email_imap_port.trim().parse().unwrap_or(e.imap_port);
        e.imap_username = self.email_imap_username.trim().to_string();
        e.imap_password = self.email_imap_password.clone();
        e.imap_mailbox = self.email_imap_mailbox.trim().to_string();
        e.smtp_host = self.email_smtp_host.trim().to_string();
        e.smtp_port = self.email_smtp_port.trim().parse().unwrap_or(e.smtp_port);
        e.smtp_username = self.email_smtp_username.trim().to_string();
        e.smtp_password = self.email_smtp_password.clone();
        e.from_address = self.email_from_address.trim().to_string();
        e.graph_tenant_id = self.email_graph_tenant_id.trim().to_string();
        e.graph_client_id = self.email_graph_client_id.trim().to_string();
        e.graph_client_secret = self.email_graph_client_secret.trim().to_string();
        e.graph_user_id = self.email_graph_user_id.trim().to_string();
        e.allowed_users = split(&self.email_allowed);

        c.tools.exec.timeout = self.exec_timeout.trim().parse().unwrap_or(c.tools.exec.timeout);
        c.tools.exec.shell = self.exec_shell.trim().to_string();
        c.tools.exec.permission_mode = self.exec_permission_mode.trim().to_string();
        c.tools.restrict_to_workspace = self.restrict_to_workspace;
        c.tools.web.search.api_key = self.web_search_key.trim().to_string();

        c.memory.enabled = self.memory_enabled;
        c.memory.compaction_threshold = self
            .memory_compaction_threshold
            .trim()
            .parse()
            .unwrap_or(c.memory.compaction_threshold);
        c.memory.keep_recent = self
            .memory_keep_recent
            .trim()
            .parse()
            .unwrap_or(c.memory.keep_recent);
        c.heartbeat.enabled = self.heartbeat_enabled;
        c.heartbeat.interval_minutes = self
            .heartbeat_interval_minutes
            .trim()
            .parse()
            .unwrap_or(c.heartbeat.interval_minutes);
    }

    /// Problems worth blocking a save for. Kept to things that are certainly
    /// wrong, so the editor never refuses a legitimate half-finished setup.
    pub fn validation_errors(&self) -> Vec<String> {
        let mut errs = Vec::new();
        if self.model.trim().is_empty() {
            errs.push("Agent model cannot be empty.".into());
        }
        if !matches!(
            self.exec_permission_mode.trim(),
            "unsafe_only" | "always" | "poweruser"
        ) {
            errs.push(
                "Exec permission mode must be unsafe_only, always, or poweruser.".into(),
            );
        }
        for (field, val) in [
            ("Max tokens", &self.max_tokens),
            ("Max tool iterations", &self.max_tool_iterations),
            ("Chat context length", &self.chat_context_length),
            ("Exec timeout", &self.exec_timeout),
        ] {
            if val.trim().parse::<u64>().is_err() {
                errs.push(format!("{field} must be a whole number."));
            }
        }
        if self.temperature.trim().parse::<f64>().is_err() {
            errs.push("Temperature must be a number.".into());
        }
        if self.email_provider == "graph" {
            let missing = [
                ("tenant id", &self.email_graph_tenant_id),
                ("client id", &self.email_graph_client_id),
                ("client secret", &self.email_graph_client_secret),
                ("mailbox", &self.email_graph_user_id),
            ]
            .iter()
            .filter(|(_, v)| v.trim().is_empty())
            .map(|(n, _)| *n)
            .collect::<Vec<_>>();
            if !missing.is_empty() && missing.len() < 4 {
                // All-empty is just "not configured yet"; a partial setup is
                // a mistake worth naming.
                errs.push(format!("Email (Graph) is missing: {}.", missing.join(", ")));
            }
        }
        errs
    }
}

/// True when the install has never really been configured — used to show the
/// first-run setup instead of dropping the user into an unusable chat.
pub fn is_first_run(c: &MetisConfig) -> bool {
    let p = &c.providers;
    let any_key = [
        &p.anthropic.api_key,
        &p.openai.api_key,
        &p.openrouter.api_key,
        &p.deepseek.api_key,
        &p.groq.api_key,
        &p.gemini.api_key,
        &p.moonshot.api_key,
        &p.minimax.api_key,
        &p.zhipu.api_key,
        &p.dashscope.api_key,
        &p.aihubmix.api_key,
        &p.vllm.api_key,
        &p.lmstudio.api_key,
    ]
    .iter()
    .any(|k| !k.trim().is_empty());
    // Ollama is local and needs no key, so a configured Ollama model counts.
    let local_model = c.agents.defaults.model.to_lowercase().contains("ollama");
    !any_key && !local_model
}

/// A masked secret field with a reveal toggle.
fn secret_field(
    ui: &mut egui::Ui,
    id: &str,
    label: &str,
    value: &mut String,
    reveal: &mut HashSet<String>,
) {
    ui.horizontal(|ui| {
        ui.label(label);
        let shown = reveal.contains(id);
        ui.add(
            egui::TextEdit::singleline(value)
                .password(!shown)
                .desired_width(320.0)
                .hint_text("not set"),
        );
        let icon = if shown { "🙈" } else { "👁" };
        if ui.small_button(icon).clicked() {
            if shown {
                reveal.remove(id);
            } else {
                reveal.insert(id.to_string());
            }
        }
        if !value.trim().is_empty() && ui.small_button("clear").clicked() {
            value.clear();
        }
    });
}

fn text_field(ui: &mut egui::Ui, label: &str, value: &mut String, width: f32, hint: &str) {
    ui.horizontal(|ui| {
        ui.label(label);
        ui.add(
            egui::TextEdit::singleline(value)
                .desired_width(width)
                .hint_text(hint),
        );
    });
}

/// Draw the settings panel. Returns what the app should do next.
pub fn draw(
    ui: &mut egui::Ui,
    form: &mut SettingsForm,
    reveal: &mut HashSet<String>,
    status: &str,
) -> SettingsAction {
    let mut action = SettingsAction::None;

    ui.horizontal(|ui| {
        ui.heading("Settings");
        ui.add_space(12.0);
        if ui.button("💾  Save to config.json").clicked() {
            action = SettingsAction::Save;
        }
        if ui.button("↺  Reload from disk").clicked() {
            action = SettingsAction::Reload;
        }
    });
    ui.label(
        egui::RichText::new("Changes are written to ~/.metis/config.json (a backup is kept). Restart the gateway to apply.")
            .small()
            .color(egui::Color32::GRAY),
    );
    if !status.is_empty() {
        ui.add_space(4.0);
        ui.label(egui::RichText::new(status).color(ACCENT).strong());
    }
    ui.separator();

    egui::ScrollArea::vertical().show(ui, |ui| {
        // ── Agent ──
        egui::CollapsingHeader::new("🧠  Agent")
            .default_open(true)
            .show(ui, |ui| {
                text_field(ui, "Model", &mut form.model, 320.0, "e.g. MiniMax-M2.7");
                text_field(ui, "Subagent model", &mut form.subagent_model, 320.0, "empty = same as main");
                text_field(ui, "Workspace", &mut form.workspace, 380.0, "~/.metis/workspace");
                ui.horizontal(|ui| {
                    ui.label("Max tokens");
                    ui.add(egui::TextEdit::singleline(&mut form.max_tokens).desired_width(80.0));
                    ui.label("Temperature");
                    ui.add(egui::TextEdit::singleline(&mut form.temperature).desired_width(60.0));
                    ui.label("Max tool iterations");
                    ui.add(egui::TextEdit::singleline(&mut form.max_tool_iterations).desired_width(60.0));
                });
                ui.horizontal(|ui| {
                    ui.label("Chat context length");
                    ui.add(egui::TextEdit::singleline(&mut form.chat_context_length).desired_width(80.0));
                });
                ui.checkbox(&mut form.show_token_usage, "Show token usage in replies");
                ui.checkbox(&mut form.log_thinking_json, "Log thinking JSON");
                ui.checkbox(&mut form.include_fenced_code, "Include fenced code in chat apps");
                ui.checkbox(&mut form.include_exec_output, "Include exec output in chat apps");
            });

        // ── Providers ──
        egui::CollapsingHeader::new("🔑  Provider API keys")
            .default_open(false)
            .show(ui, |ui| {
                ui.label(
                    egui::RichText::new("Ollama and LM Studio run locally and need no key.")
                        .small()
                        .color(egui::Color32::GRAY),
                );
                for (name, key) in form.providers.iter_mut() {
                    let id = format!("prov_{name}");
                    secret_field(ui, &id, name, key, reveal);
                }
            });

        // ── Channels ──
        egui::CollapsingHeader::new("📨  Channels")
            .default_open(false)
            .show(ui, |ui| {
                ui.label(egui::RichText::new("Telegram").strong());
                secret_field(ui, "tg_token", "Bot token", &mut form.telegram_token, reveal);
                text_field(ui, "Allowed user ids", &mut form.telegram_allowed, 320.0, "comma-separated, empty = anyone");
                ui.add_space(8.0);

                ui.label(egui::RichText::new("Discord").strong());
                secret_field(ui, "dc_token", "Bot token", &mut form.discord_token, reveal);
                text_field(ui, "Allowed user ids", &mut form.discord_allowed, 320.0, "comma-separated");
                ui.add_space(8.0);

                ui.label(egui::RichText::new("WhatsApp").strong());
                text_field(ui, "Bridge URL", &mut form.whatsapp_bridge_url, 320.0, "ws://localhost:3001");
                ui.label(
                    egui::RichText::new("Needs the Node bridge running (bridge/ → npm start). Leave empty to disable the channel.")
                        .small()
                        .color(WARN),
                );
                ui.add_space(8.0);

                ui.label(egui::RichText::new("Slack").strong());
                secret_field(ui, "sl_bot", "Bot token", &mut form.slack_bot_token, reveal);
                secret_field(ui, "sl_app", "App token", &mut form.slack_app_token, reveal);
                ui.add_space(8.0);

                ui.label(egui::RichText::new("Email").strong());
                ui.horizontal(|ui| {
                    ui.label("Backend");
                    ui.selectable_value(&mut form.email_provider, "imap".to_string(), "IMAP + SMTP");
                    ui.selectable_value(&mut form.email_provider, "graph".to_string(), "Microsoft Graph");
                });
                if form.email_provider == "graph" {
                    ui.label(
                        egui::RichText::new("Office 365. IMAP cannot be used there — Microsoft disabled Basic auth for it in 2022. Run scripts/setup-o365-graph.ps1 to create the Azure app.")
                            .small()
                            .color(WARN),
                    );
                    text_field(ui, "Tenant id", &mut form.email_graph_tenant_id, 320.0, "");
                    text_field(ui, "Client id", &mut form.email_graph_client_id, 320.0, "");
                    secret_field(ui, "gr_secret", "Client secret", &mut form.email_graph_client_secret, reveal);
                    text_field(ui, "Mailbox", &mut form.email_graph_user_id, 320.0, "info@yourdomain.com");
                    text_field(ui, "Folder", &mut form.email_imap_mailbox, 200.0, "Inbox");
                } else {
                    text_field(ui, "IMAP host", &mut form.email_imap_host, 260.0, "imap.gmail.com");
                    ui.horizontal(|ui| {
                        ui.label("IMAP port");
                        ui.add(egui::TextEdit::singleline(&mut form.email_imap_port).desired_width(70.0));
                        ui.label("Folder");
                        ui.add(egui::TextEdit::singleline(&mut form.email_imap_mailbox).desired_width(120.0));
                    });
                    text_field(ui, "IMAP user", &mut form.email_imap_username, 260.0, "");
                    secret_field(ui, "im_pass", "IMAP password", &mut form.email_imap_password, reveal);
                    text_field(ui, "SMTP host", &mut form.email_smtp_host, 260.0, "smtp.gmail.com");
                    ui.horizontal(|ui| {
                        ui.label("SMTP port");
                        ui.add(egui::TextEdit::singleline(&mut form.email_smtp_port).desired_width(70.0));
                    });
                    text_field(ui, "SMTP user", &mut form.email_smtp_username, 260.0, "");
                    secret_field(ui, "sm_pass", "SMTP password", &mut form.email_smtp_password, reveal);
                    text_field(ui, "From address", &mut form.email_from_address, 260.0, "");
                }
                text_field(ui, "Allowed senders", &mut form.email_allowed, 320.0, "comma-separated, empty = anyone");
            });

        // ── Tools ──
        egui::CollapsingHeader::new("🛠  Tools")
            .default_open(false)
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label("Exec timeout (s)");
                    ui.add(egui::TextEdit::singleline(&mut form.exec_timeout).desired_width(70.0));
                    ui.label("Shell");
                    ui.add(egui::TextEdit::singleline(&mut form.exec_shell).desired_width(120.0));
                });
                ui.horizontal(|ui| {
                    ui.label("Permission mode");
                    for mode in ["unsafe_only", "always", "poweruser"] {
                        ui.selectable_value(&mut form.exec_permission_mode, mode.to_string(), mode);
                    }
                });
                ui.checkbox(&mut form.restrict_to_workspace, "Restrict file/exec access to the workspace");
                secret_field(ui, "brave_key", "Brave search API key", &mut form.web_search_key, reveal);
            });

        // ── Memory / heartbeat ──
        egui::CollapsingHeader::new("🧩  Memory & heartbeat")
            .default_open(false)
            .show(ui, |ui| {
                ui.checkbox(&mut form.memory_enabled, "Semantic memory enabled");
                ui.horizontal(|ui| {
                    ui.label("Compact after N messages");
                    ui.add(egui::TextEdit::singleline(&mut form.memory_compaction_threshold).desired_width(70.0));
                    ui.label("Keep recent");
                    ui.add(egui::TextEdit::singleline(&mut form.memory_keep_recent).desired_width(70.0));
                });
                ui.checkbox(&mut form.heartbeat_enabled, "Heartbeat enabled");
                ui.horizontal(|ui| {
                    ui.label("Interval (minutes)");
                    ui.add(egui::TextEdit::singleline(&mut form.heartbeat_interval_minutes).desired_width(70.0));
                });
            });

        ui.add_space(16.0);
    });

    action
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_through_config() {
        let mut c = MetisConfig::default();
        c.agents.defaults.model = "MiniMax-M2.7".into();
        c.channels.telegram.token = "tok".into();
        c.channels.telegram.allowed_users = vec!["1".into(), "2".into()];

        let mut form = SettingsForm::from_config(&c);
        assert_eq!(form.model, "MiniMax-M2.7");
        assert_eq!(form.telegram_allowed, "1, 2");

        form.model = "gpt-4o".into();
        form.telegram_allowed = "7, 8 , ".into();
        let mut out = MetisConfig::default();
        form.apply_to(&mut out);
        assert_eq!(out.agents.defaults.model, "gpt-4o");
        // Blank entries are dropped, whitespace trimmed.
        assert_eq!(out.channels.telegram.allowed_users, vec!["7", "8"]);
    }

    #[test]
    fn bad_numbers_keep_the_existing_value() {
        let mut c = MetisConfig::default();
        c.agents.defaults.max_tokens = 8192;
        let mut form = SettingsForm::from_config(&c);
        form.max_tokens = "not a number".into();
        form.apply_to(&mut c);
        assert_eq!(c.agents.defaults.max_tokens, 8192, "must not reset to 0");
    }

    #[test]
    fn validation_catches_real_mistakes_only() {
        let c = MetisConfig::default();
        let mut form = SettingsForm::from_config(&c);
        form.model = "m".into();
        form.exec_permission_mode = "unsafe_only".into();
        assert!(form.validation_errors().is_empty());

        form.exec_permission_mode = "yolo".into();
        assert!(form.validation_errors().iter().any(|e| e.contains("permission mode")));

        form.exec_permission_mode = "unsafe_only".into();
        form.model.clear();
        assert!(form.validation_errors().iter().any(|e| e.contains("model")));
    }

    #[test]
    fn partial_graph_config_is_flagged_but_empty_is_not() {
        let c = MetisConfig::default();
        let mut form = SettingsForm::from_config(&c);
        form.model = "m".into();
        form.email_provider = "graph".into();
        // Nothing filled in yet — that is simply "not configured".
        assert!(!form.validation_errors().iter().any(|e| e.contains("Graph")));
        // Half-filled is a mistake worth naming.
        form.email_graph_tenant_id = "tenant".into();
        assert!(form.validation_errors().iter().any(|e| e.contains("Graph")));
    }

    #[test]
    fn first_run_detection() {
        let mut c = MetisConfig::default();
        assert!(is_first_run(&c), "a fresh config has no keys");
        c.providers.openai.api_key = "sk-x".into();
        assert!(!is_first_run(&c));

        let mut local = MetisConfig::default();
        local.agents.defaults.model = "ollama/gemma3:4b".into();
        assert!(!is_first_run(&local), "local models need no API key");
    }
}
