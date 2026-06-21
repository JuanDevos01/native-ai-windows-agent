//! Ollama local model helpers — detect tool/vision capabilities via `/api/show`.

use std::collections::HashMap;
use std::sync::Mutex;

use tracing::debug;

static CAP_CACHE: Mutex<Option<HashMap<String, bool>>> = Mutex::new(None);

fn cache_get(model: &str) -> Option<bool> {
    CAP_CACHE
        .lock()
        .ok()
        .and_then(|c| c.as_ref().and_then(|m| m.get(model).copied()))
}

fn cache_set(model: &str, supports_tools: bool) {
    if let Ok(mut guard) = CAP_CACHE.lock() {
        guard
            .get_or_insert_with(HashMap::new)
            .insert(model.to_string(), supports_tools);
    }
}

/// Strip `/v1` from an OpenAI-compatible Ollama base URL.
pub fn ollama_root_from_api_base(api_base: &str) -> String {
    let base = api_base.trim_end_matches('/');
    base.strip_suffix("/v1")
        .unwrap_or(base)
        .to_string()
}

#[derive(serde::Deserialize)]
struct ShowResponse {
    #[serde(default)]
    capabilities: Vec<String>,
}

fn parse_supports_tools(show: &ShowResponse) -> bool {
    if show.capabilities.is_empty() {
        // Older Ollama builds omit capabilities — caller may retry without tools on 400.
        return true;
    }
    show.capabilities.iter().any(|c| c == "tools")
}

/// Async: does this Ollama model accept OpenAI-style tool definitions?
pub async fn ollama_model_supports_tools(
    client: &reqwest::Client,
    api_base: &str,
    model: &str,
) -> bool {
    if let Some(cached) = cache_get(model) {
        return cached;
    }

    let root = ollama_root_from_api_base(api_base);
    let url = format!("{root}/api/show");
    let body = serde_json::json!({ "model": model });

    let Ok(resp) = client.post(&url).json(&body).send().await else {
        return true;
    };
    if !resp.status().is_success() {
        return true;
    }
    let Ok(show) = resp.json::<ShowResponse>().await else {
        return true;
    };

    let supports = parse_supports_tools(&show);
    cache_set(model, supports);
    debug!(
        model = %model,
        supports,
        capabilities = ?show.capabilities,
        "ollama model capabilities"
    );
    supports
}

/// Sync variant for desktop UI (model labels).
pub fn ollama_model_supports_tools_sync(api_base: &str, model: &str) -> Option<bool> {
    if let Some(cached) = cache_get(model) {
        return Some(cached);
    }

    let root = ollama_root_from_api_base(api_base);
    let url = format!("{root}/api/show");
    let body = serde_json::json!({ "model": model });

    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(3))
        .build()
        .ok()?;
    let resp = client.post(&url).json(&body).send().ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let show: ShowResponse = resp.json().ok()?;
    let supports = parse_supports_tools(&show);
    cache_set(model, supports);
    Some(supports)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_url_strips_v1() {
        assert_eq!(
            ollama_root_from_api_base("http://localhost:11434/v1"),
            "http://localhost:11434"
        );
    }

    #[test]
    fn parse_tools_capability() {
        assert!(parse_supports_tools(&ShowResponse {
            capabilities: vec!["completion".into(), "tools".into()],
        }));
        assert!(!parse_supports_tools(&ShowResponse {
            capabilities: vec!["completion".into(), "vision".into()],
        }));
    }
}
