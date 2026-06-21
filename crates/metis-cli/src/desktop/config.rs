//! Desktop GUI preferences — `~/.metis/desktop.json`.

use std::path::PathBuf;

use metis_core::utils::get_data_path;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DesktopConfig {
    #[serde(default)]
    pub window: WindowConfig,
    #[serde(default)]
    pub sidebar_width: f32,
    #[serde(default)]
    pub agent_title: String,
    #[serde(default)]
    pub pinned_sessions: Vec<String>,
    #[serde(default = "default_true")]
    pub save_window_geometry: bool,
    /// Custom model ids the user added via the desktop UI (kept across sessions).
    #[serde(default)]
    pub extra_models: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WindowConfig {
    pub width: f32,
    pub height: f32,
}

impl Default for WindowConfig {
    fn default() -> Self {
        Self {
            width: 1280.0,
            height: 800.0,
        }
    }
}

impl Default for DesktopConfig {
    fn default() -> Self {
        Self {
            window: WindowConfig::default(),
            sidebar_width: 260.0,
            agent_title: "METIS AGENT".into(),
            pinned_sessions: Vec::new(),
            save_window_geometry: true,
            extra_models: Vec::new(),
        }
    }
}

fn default_true() -> bool {
    true
}

pub fn desktop_config_path() -> PathBuf {
    get_data_path().join("desktop.json")
}

pub fn load_desktop_config() -> DesktopConfig {
    let path = desktop_config_path();
    if !path.exists() {
        return DesktopConfig::default();
    }
    match std::fs::read_to_string(&path) {
        Ok(text) => serde_json::from_str(&text).unwrap_or_default(),
        Err(_) => DesktopConfig::default(),
    }
}

pub fn save_desktop_config(config: &DesktopConfig) -> std::io::Result<()> {
    let path = desktop_config_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let text = serde_json::to_string_pretty(config)?;
    std::fs::write(path, text)
}
