//! Config loader — reads `~/.metis/config.json`, merges env vars, and
//! applies legacy migrations.
//!
//! Replaces nanobot's `config/loader.py`.
//!
//! # Loading precedence
//! 1. Defaults (from `Config::default()`)
//! 2. JSON file at `~/.metis/config.json`
//! 3. Environment variables `METIS_<SECTION>__<FIELD>` (override JSON)

use std::path::{Path, PathBuf};
use tracing::{debug, info, warn};

use super::schema::Config;

/// Default config file path.
pub fn get_config_path() -> PathBuf {
    crate::utils::get_data_path().join("config.json")
}

/// Load configuration from the default path + env vars.
///
/// Falls back to `Config::default()` if the file doesn't exist or can't be parsed.
pub fn load_config(path: Option<&Path>) -> Config {
    let config_path = path
        .map(PathBuf::from)
        .unwrap_or_else(get_config_path);

    load_config_from_path(&config_path)
}

/// Load config from a specific file path.
fn load_config_from_path(path: &Path) -> Config {
    if !path.exists() {
        info!("No config file found at {}, using defaults", path.display());
        return apply_env_overrides(Config::default());
    }

    debug!("Loading config from {}", path.display());

    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) => {
            warn!("Failed to read config file {}: {}", path.display(), e);
            return apply_env_overrides(Config::default());
        }
    };

    // Parse JSON → Value first for migration
    let mut raw: serde_json::Value = match serde_json::from_str(&content) {
        Ok(v) => v,
        Err(e) => {
            warn!("Failed to parse config JSON: {}", e);
            return apply_env_overrides(Config::default());
        }
    };

    // Apply legacy migrations
    migrate_config(&mut raw);

    // Deserialize into typed Config
    let config: Config = match serde_json::from_value(raw) {
        Ok(c) => c,
        Err(e) => {
            warn!("Failed to deserialize config: {}", e);
            return apply_env_overrides(Config::default());
        }
    };

    apply_env_overrides(config)
}

/// Save configuration to disk (pretty-printed JSON with camelCase keys).
pub fn save_config(config: &Config, path: Option<&Path>) -> std::io::Result<()> {
    let config_path = path
        .map(PathBuf::from)
        .unwrap_or_else(get_config_path);

    // Ensure parent directory exists
    if let Some(parent) = config_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let json = serde_json::to_string_pretty(config)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;

    std::fs::write(&config_path, json)?;
    debug!("Config saved to {}", config_path.display());
    Ok(())
}

/// Apply legacy config migrations.
///
/// Moves `tools.exec.restrictToWorkspace` → `tools.restrictToWorkspace`.
fn migrate_config(raw: &mut serde_json::Value) {
    // Migration: tools.exec.restrictToWorkspace → tools.restrictToWorkspace
    if let Some(tools) = raw.get_mut("tools") {
        if let Some(exec) = tools.get("exec") {
            if let Some(restrict) = exec.get("restrictToWorkspace") {
                if tools.get("restrictToWorkspace").is_none() {
                    let val = restrict.clone();
                    tools["restrictToWorkspace"] = val;
                    debug!("Migrated tools.exec.restrictToWorkspace → tools.restrictToWorkspace");
                }
            }
        }
    }

    // Migration: legacy OxiBot data directory → Metis (~/.oxibot/workspace → ~/.metis/workspace, etc.)
    if let Some(agents) = raw.get_mut("agents") {
        if let Some(defaults) = agents.get_mut("defaults") {
            if let Some(ws) = defaults.get_mut("workspace") {
                if let Some(s) = ws.as_str() {
                    if s.contains(".oxibot") {
                        let new_ws = s.replace(".oxibot", ".metis");
                        warn!(
                            old = %s,
                            new = %new_ws,
                            "migrated workspace path: .oxibot → .metis (update ~/.metis/config.json to remove .oxibot)"
                        );
                        *ws = serde_json::json!(new_ws);
                    }
                }
            }
        }
    }
}

/// Apply environment variable overrides on top of a loaded config.
///
/// Env var format: `METIS_<SECTION>__<FIELD>` (double underscore as delimiter).
///
/// Supported overrides:
/// - `metis_agentS__DEFAULTS__MODEL` → `agents.defaults.model`
/// - `metis_agentS__DEFAULTS__MAX_TOKENS` → `agents.defaults.max_tokens`
/// - `metis_agentS__DEFAULTS__TEMPERATURE` → `agents.defaults.temperature`
/// - `metis_providers__<NAME>__API_KEY` → `providers.<name>.api_key`
/// - `metis_providers__<NAME>__API_BASE` → `providers.<name>.api_base`
/// - `METIS_GATEWAY__HOST` → `gateway.host`
/// - `METIS_GATEWAY__PORT` → `gateway.port`
/// - `METIS_TOOLS__RESTRICT_TO_WORKSPACE` → `tools.restrict_to_workspace`
/// - `METIS_TOOLS__EXEC__TIMEOUT` → `tools.exec.timeout`
/// - `METIS_TOOLS__EXEC__SHELL` → `tools.exec.shell`
/// - `METIS_TOOLS__EXEC__PERMISSION_MODE` → `tools.exec.permission_mode`
/// - `METIS_TRANSCRIPTION__PROVIDER` → `transcription.provider`
/// - `METIS_TRANSCRIPTION__API_BASE` → `transcription.api_base`
/// - `METIS_TRANSCRIPTION__API_KEY` → `transcription.api_key`
/// - `METIS_TRANSCRIPTION__MODEL` → `transcription.model`
/// - `METIS_TRANSCRIPTION__ENABLED` → `transcription.enabled` (`true` / `1`)
/// - `METIS_TRANSCRIPTION__WHISPER_CPP__EXE_PATH` → `transcription.whisper_cpp.exe_path`
/// - `METIS_TRANSCRIPTION__WHISPER_CPP__MODEL_PATH` → `transcription.whisper_cpp.model_path`
fn apply_env_overrides(mut config: Config) -> Config {
    // Agent defaults
    if let Ok(val) = std::env::var("metis_agentS__DEFAULTS__MODEL") {
        config.agents.defaults.model = val;
    }
    if let Ok(val) = std::env::var("metis_agentS__DEFAULTS__MAX_TOKENS") {
        if let Ok(n) = val.parse::<u32>() {
            config.agents.defaults.max_tokens = n;
        }
    }
    if let Ok(val) = std::env::var("metis_agentS__DEFAULTS__TEMPERATURE") {
        if let Ok(t) = val.parse::<f64>() {
            config.agents.defaults.temperature = t;
        }
    }
    if let Ok(val) = std::env::var("metis_agentS__DEFAULTS__MAX_TOOL_ITERATIONS") {
        if let Ok(n) = val.parse::<u32>() {
            config.agents.defaults.max_tool_iterations = n;
        }
    }
    if let Ok(val) = std::env::var("metis_agentS__DEFAULTS__WORKSPACE") {
        config.agents.defaults.workspace = val;
    }

    // Provider API keys (by provider name)
    apply_provider_env(&mut config.providers.anthropic, "ANTHROPIC");
    apply_provider_env(&mut config.providers.openai, "OPENAI");
    apply_provider_env(&mut config.providers.openrouter, "OPENROUTER");
    apply_provider_env(&mut config.providers.deepseek, "DEEPSEEK");
    apply_provider_env(&mut config.providers.groq, "GROQ");
    apply_provider_env(&mut config.providers.zhipu, "ZHIPU");
    apply_provider_env(&mut config.providers.dashscope, "DASHSCOPE");
    apply_provider_env(&mut config.providers.vllm, "VLLM");
    apply_provider_env(&mut config.providers.gemini, "GEMINI");
    apply_provider_env(&mut config.providers.moonshot, "MOONSHOT");
    apply_provider_env(&mut config.providers.minimax, "MINIMAX");
    apply_provider_env(&mut config.providers.aihubmix, "AIHUBMIX");

    // Gateway
    if let Ok(val) = std::env::var("METIS_GATEWAY__HOST") {
        config.gateway.host = val;
    }
    if let Ok(val) = std::env::var("METIS_GATEWAY__PORT") {
        if let Ok(p) = val.parse::<u16>() {
            config.gateway.port = p;
        }
    }

    // Tools
    if let Ok(val) = std::env::var("METIS_TOOLS__RESTRICT_TO_WORKSPACE") {
        config.tools.restrict_to_workspace = val == "true" || val == "1";
    }
    if let Ok(val) = std::env::var("METIS_TOOLS__EXEC__TIMEOUT") {
        if let Ok(n) = val.parse::<u64>() {
            config.tools.exec.timeout = n;
        }
    }
    if let Ok(val) = std::env::var("METIS_TOOLS__EXEC__SHELL") {
        config.tools.exec.shell = val;
    }
    if let Ok(val) = std::env::var("METIS_TOOLS__EXEC__PERMISSION_MODE") {
        config.tools.exec.permission_mode = val;
    }

    // Transcription (Telegram voice, etc.)
    if let Ok(val) = std::env::var("METIS_TRANSCRIPTION__ENABLED") {
        config.transcription.enabled = val == "true" || val == "1";
    }
    if let Ok(val) = std::env::var("METIS_TRANSCRIPTION__PROVIDER") {
        config.transcription.provider = val;
    }
    if let Ok(val) = std::env::var("METIS_TRANSCRIPTION__API_BASE") {
        if !val.trim().is_empty() {
            config.transcription.api_base = Some(val);
        }
    }
    if let Ok(val) = std::env::var("METIS_TRANSCRIPTION__API_KEY") {
        config.transcription.api_key = val;
    }
    if let Ok(val) = std::env::var("METIS_TRANSCRIPTION__MODEL") {
        config.transcription.model = val;
    }
    if let Ok(val) = std::env::var("METIS_TRANSCRIPTION__WHISPER_CPP__EXE_PATH") {
        if !val.trim().is_empty() {
            let wc = config.transcription.whisper_cpp.get_or_insert_default();
            wc.exe_path = Some(val);
        }
    }
    if let Ok(val) = std::env::var("METIS_TRANSCRIPTION__WHISPER_CPP__MODEL_PATH") {
        if !val.trim().is_empty() {
            let wc = config.transcription.whisper_cpp.get_or_insert_default();
            wc.model_path = val;
        }
    }

    config
}

/// Apply env var overrides for a single provider.
fn apply_provider_env(provider: &mut super::schema::ProviderConfig, name: &str) {
    if let Ok(val) = std::env::var(format!("metis_providers__{name}__API_KEY")) {
        provider.api_key = val;
    }
    if let Ok(val) = std::env::var(format!("metis_providers__{name}__API_BASE")) {
        provider.api_base = Some(val);
    }
}

// ─────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    fn write_temp_json(content: &str) -> NamedTempFile {
        let mut file = NamedTempFile::new().unwrap();
        file.write_all(content.as_bytes()).unwrap();
        file.flush().unwrap();
        file
    }

    #[test]
    fn test_load_missing_file() {
        let config = load_config_from_path(Path::new("/nonexistent/path/config.json"));
        // Should return defaults
        assert_eq!(config.agents.defaults.max_tokens, 8192);
        assert_eq!(config.gateway.port, 18790);
    }

    #[test]
    fn test_load_valid_json() {
        let file = write_temp_json(r#"{
            "agents": {
                "defaults": {
                    "model": "gpt-4o",
                    "maxTokens": 2048
                }
            }
        }"#);

        let config = load_config_from_path(file.path());
        assert_eq!(config.agents.defaults.model, "gpt-4o");
        assert_eq!(config.agents.defaults.max_tokens, 2048);
        // Default preserved
        assert_eq!(config.agents.defaults.temperature, 0.7);
    }

    #[test]
    fn test_load_invalid_json_returns_defaults() {
        let file = write_temp_json("not valid json {{{");
        let config = load_config_from_path(file.path());
        assert_eq!(config.agents.defaults.max_tokens, 8192);
    }

    #[test]
    fn test_load_empty_json() {
        let file = write_temp_json("{}");
        let config = load_config_from_path(file.path());
        assert_eq!(config.agents.defaults.model, "anthropic/claude-sonnet-4-20250514");
    }

    #[test]
    fn test_save_and_reload() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");

        let mut config = Config::default();
        config.agents.defaults.model = "deepseek-chat".to_string();
        config.providers.anthropic.api_key = "sk-ant-test".to_string();

        save_config(&config, Some(&path)).unwrap();

        let reloaded = load_config_from_path(&path);
        assert_eq!(reloaded.agents.defaults.model, "deepseek-chat");
        assert_eq!(reloaded.providers.anthropic.api_key, "sk-ant-test");
    }

    #[test]
    fn test_migrate_restrict_to_workspace() {
        let file = write_temp_json(r#"{
            "tools": {
                "exec": {
                    "restrictToWorkspace": true,
                    "timeout": 30
                }
            }
        }"#);

        let config = load_config_from_path(file.path());
        assert!(config.tools.restrict_to_workspace);
        assert_eq!(config.tools.exec.timeout, 30);
    }

    #[test]
    fn test_migrate_no_overwrite() {
        let file = write_temp_json(r#"{
            "tools": {
                "restrictToWorkspace": false,
                "exec": {
                    "restrictToWorkspace": true
                }
            }
        }"#);

        let config = load_config_from_path(file.path());
        // Existing value should NOT be overwritten by migration
        assert!(!config.tools.restrict_to_workspace);
    }

    #[test]
    fn test_migrate_oxibot_workspace_to_metis() {
        let file = write_temp_json(r#"{
            "agents": {
                "defaults": {
                    "workspace": "~/.oxibot/workspace"
                }
            }
        }"#);

        let config = load_config_from_path(file.path());
        assert_eq!(config.agents.defaults.workspace, "~/.metis/workspace");
    }

    #[test]
    fn test_env_override_model() {
        // Set env var, apply overrides
        std::env::set_var("metis_agentS__DEFAULTS__MODEL", "test-model");
        let config = apply_env_overrides(Config::default());
        assert_eq!(config.agents.defaults.model, "test-model");
        std::env::remove_var("metis_agentS__DEFAULTS__MODEL");
    }

    #[test]
    fn test_env_override_provider_key() {
        std::env::set_var("metis_providers__ANTHROPIC__API_KEY", "sk-env-key");
        let config = apply_env_overrides(Config::default());
        assert_eq!(config.providers.anthropic.api_key, "sk-env-key");
        std::env::remove_var("metis_providers__ANTHROPIC__API_KEY");
    }

    #[test]
    fn test_env_override_gateway_port() {
        std::env::set_var("METIS_GATEWAY__PORT", "9999");
        let config = apply_env_overrides(Config::default());
        assert_eq!(config.gateway.port, 9999);
        std::env::remove_var("METIS_GATEWAY__PORT");
    }

    #[test]
    fn test_saved_json_uses_camel_case() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");

        save_config(&Config::default(), Some(&path)).unwrap();

        let content = std::fs::read_to_string(&path).unwrap();
        let raw: serde_json::Value = serde_json::from_str(&content).unwrap();

        assert!(raw["agents"]["defaults"].get("maxTokens").is_some());
        assert!(raw["agents"]["defaults"].get("max_tokens").is_none());
    }

    #[test]
    fn test_full_config_with_providers() {
        let file = write_temp_json(r#"{
            "providers": {
                "anthropic": { "apiKey": "sk-ant-123" },
                "openrouter": { "apiKey": "sk-or-456", "apiBase": "https://custom.io/v1" },
                "deepseek": { "apiKey": "ds-789" }
            },
            "agents": {
                "defaults": {
                    "model": "claude-sonnet-4-20250514",
                    "maxTokens": 4096,
                    "temperature": 0.5
                }
            }
        }"#);

        let config = load_config_from_path(file.path());
        assert!(config.providers.anthropic.is_configured());
        assert!(config.providers.openrouter.is_configured());
        assert_eq!(
            config.providers.openrouter.api_base.as_deref(),
            Some("https://custom.io/v1")
        );
        assert!(config.providers.deepseek.is_configured());
        assert!(!config.providers.openai.is_configured());
    }
}
