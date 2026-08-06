//! Model discovery — enumerate the models each configured provider offers.
//!
//! Every provider Metis supports is OpenAI-compatible, so `GET {base}/models`
//! lists what a given API key can actually reach. Ollama additionally exposes
//! `/api/tags`, which reports per-model tool support — the one capability the
//! agent loop must know about up front.
//!
//! Used by the desktop model picker so the dropdown reflects the user's real
//! `config.json` rather than a hardcoded list.

use std::collections::HashMap;
use std::time::Duration;

use tracing::debug;

use crate::registry::{qualified_model_id, ProviderConfig, ProviderSpec, PROVIDERS};

/// How long to wait per provider. Discovery runs them concurrently, so this
/// is also roughly the worst-case wall time for the whole sweep.
const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(8);

/// One model offered by one configured provider.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiscoveredModel {
    /// Id to store in `config.json` — routes back to `provider` and resolves
    /// to the provider's own id on the wire.
    pub id: String,
    /// The provider's own id for this model (e.g. `gpt-4o`, `MiniMax-M2`).
    pub raw_id: String,
    /// Internal provider name (e.g. `"openai"`).
    pub provider: &'static str,
    /// Human-readable provider name (e.g. `"OpenAI"`).
    pub provider_display: &'static str,
    /// Tool-calling support, when the provider reports it (Ollama only).
    /// `None` means unknown — assume supported, as the agent loop does.
    pub supports_tools: Option<bool>,
}

impl DiscoveredModel {
    /// `true` when the model is known to lack tool support.
    pub fn is_chat_only(&self) -> bool {
        self.supports_tools == Some(false)
    }
}

/// Model ids that are not chat models. `/models` returns a provider's whole
/// catalogue — embeddings, speech, image, and moderation endpoints included —
/// and none of those can back an agent.
const NON_CHAT_MARKERS: &[&str] = &[
    "embed",
    "whisper",
    "tts",
    "dall-e",
    "moderation",
    "realtime",
    "audio",
    "transcribe",
    "image",
    "sora",
    "rerank",
    "guard",
    "speech",
    "video",
];

/// Whether `id` looks like a chat-capable model.
fn is_chat_model(id: &str) -> bool {
    let lower = id.to_lowercase();
    !NON_CHAT_MARKERS.iter().any(|m| lower.contains(m))
}

/// Resolve a provider's API base the same way `HttpProvider` does.
fn api_base(config: &ProviderConfig, spec: &ProviderSpec) -> String {
    config
        .api_base
        .clone()
        .or_else(|| spec.default_api_base.map(String::from))
        .unwrap_or_else(|| "https://api.openai.com/v1".to_string())
}

/// Whether this provider is usable: an API key, or a local server that needs none.
fn is_usable(config: &ProviderConfig, spec: &ProviderSpec) -> bool {
    config.is_configured()
        || (spec.is_local && (config.api_base.is_some() || spec.default_api_base.is_some()))
}

/// List models from an OpenAI-compatible `GET {base}/models` endpoint.
async fn list_openai_compatible(
    client: &reqwest::Client,
    base: &str,
    api_key: &str,
) -> Result<Vec<String>, String> {
    let url = format!("{}/models", base.trim_end_matches('/'));
    let resp = client
        .get(&url)
        .bearer_auth(api_key)
        .send()
        .await
        .map_err(|e| format!("{url}: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("{url}: HTTP {}", resp.status()));
    }
    let json: serde_json::Value = resp.json().await.map_err(|e| format!("{url}: {e}"))?;
    let items = json["data"]
        .as_array()
        .ok_or_else(|| format!("{url}: response has no `data` array"))?;
    Ok(items
        .iter()
        .filter_map(|m| m["id"].as_str().map(str::to_string))
        .collect())
}

/// List Ollama models via `/api/tags`, which also reports tool support.
async fn list_ollama(
    client: &reqwest::Client,
    base: &str,
) -> Result<Vec<(String, Option<bool>)>, String> {
    let root = base.trim_end_matches('/').trim_end_matches("/v1");
    let url = format!("{root}/api/tags");
    let resp = client
        .get(&url)
        .send()
        .await
        .map_err(|e| format!("{url}: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("{url}: HTTP {}", resp.status()));
    }
    let json: serde_json::Value = resp.json().await.map_err(|e| format!("{url}: {e}"))?;
    let items = json["models"]
        .as_array()
        .ok_or_else(|| format!("{url}: response has no `models` array"))?;
    Ok(items
        .iter()
        .filter_map(|m| {
            let name = m["name"].as_str()?.to_string();
            // Absent `capabilities` means an older Ollama that doesn't report
            // them — unknown, not "no tools".
            let tools = m["capabilities"]
                .as_array()
                .map(|caps| caps.iter().any(|c| c.as_str() == Some("tools")));
            Some((name, tools))
        })
        .collect())
}

/// Discover every model reachable with the current configuration.
///
/// Providers are queried concurrently; one that is unreachable, unauthorized,
/// or slow is skipped rather than failing the sweep. Results are grouped by
/// provider in registry priority order, alphabetical within a provider.
///
/// Returns the models and, for diagnostics, one line per provider that failed.
pub async fn discover_models(
    providers: &HashMap<String, ProviderConfig>,
) -> (Vec<DiscoveredModel>, Vec<String>) {
    let client = match reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(3))
        .timeout(DISCOVERY_TIMEOUT)
        .build()
    {
        Ok(c) => c,
        Err(e) => return (Vec::new(), vec![format!("HTTP client: {e}")]),
    };

    let targets: Vec<(&'static ProviderSpec, ProviderConfig)> = PROVIDERS
        .iter()
        .filter_map(|spec| {
            let config = providers.get(spec.name)?;
            is_usable(config, spec).then(|| (spec, config.clone()))
        })
        .collect();

    // Query providers concurrently: a slow or unreachable one must not add its
    // timeout to every other provider's.
    let mut tasks = tokio::task::JoinSet::new();
    for (spec, config) in &targets {
        let client = client.clone();
        let base = api_base(config, spec);
        let key = config.api_key.clone();
        let spec: &'static ProviderSpec = spec;
        tasks.spawn(async move {
            let result: Result<Vec<(String, Option<bool>)>, String> = if spec.name == "ollama" {
                list_ollama(&client, &base).await
            } else {
                list_openai_compatible(&client, &base, &key)
                    .await
                    .map(|ids| ids.into_iter().map(|id| (id, None)).collect())
            };
            (spec, result)
        });
    }

    let mut results = Vec::new();
    while let Some(joined) = tasks.join_next().await {
        match joined {
            Ok(pair) => results.push(pair),
            Err(e) => debug!(error = %e, "model discovery task failed"),
        }
    }
    // Restore registry priority order; concurrent completion is arbitrary.
    results.sort_by_key(|(spec, _)| {
        PROVIDERS.iter().position(|s| s.name == spec.name).unwrap_or(usize::MAX)
    });

    let mut models = Vec::new();
    let mut errors = Vec::new();
    for (spec, result) in results {
        match result {
            Ok(ids) => {
                let mut found: Vec<DiscoveredModel> = ids
                    .into_iter()
                    .filter(|(id, _)| is_chat_model(id))
                    .map(|(raw_id, supports_tools)| DiscoveredModel {
                        id: qualified_model_id(&raw_id, spec),
                        raw_id,
                        provider: spec.name,
                        provider_display: spec.display_name,
                        supports_tools,
                    })
                    .collect();
                found.sort_by(|a, b| a.raw_id.to_lowercase().cmp(&b.raw_id.to_lowercase()));
                debug!(provider = spec.name, count = found.len(), "discovered models");
                models.extend(found);
            }
            Err(e) => {
                debug!(provider = spec.name, error = %e, "model discovery failed");
                errors.push(format!("{}: {e}", spec.display_name));
            }
        }
    }
    (models, errors)
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn config_with_base(key: &str, base: &str) -> ProviderConfig {
        ProviderConfig {
            api_key: key.to_string(),
            api_base: Some(base.to_string()),
            extra_headers: None,
        }
    }

    #[test]
    fn test_is_chat_model_filters_non_chat_endpoints() {
        assert!(is_chat_model("gpt-4o"));
        assert!(is_chat_model("MiniMax-M2.7-highspeed"));
        assert!(is_chat_model("claude-sonnet-4"));
        assert!(!is_chat_model("text-embedding-3-small"));
        assert!(!is_chat_model("whisper-large-v3"));
        assert!(!is_chat_model("gpt-realtime-2"));
        assert!(!is_chat_model("dall-e-3"));
        assert!(!is_chat_model("gpt-4o-transcribe"));
    }

    #[tokio::test]
    async fn test_discover_models_openai_compatible() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/models"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": [
                    { "id": "gpt-4o" },
                    { "id": "o3-mini" },
                    { "id": "text-embedding-3-small" }
                ]
            })))
            .mount(&server)
            .await;

        let mut providers = HashMap::new();
        providers.insert("openai".to_string(), config_with_base("k", &server.uri()));
        let (models, errors) = discover_models(&providers).await;

        assert!(errors.is_empty(), "unexpected errors: {errors:?}");
        // Embeddings filtered out; ambiguous id qualified so it routes back.
        assert_eq!(models.len(), 2);
        let ids: Vec<&str> = models.iter().map(|m| m.id.as_str()).collect();
        assert!(ids.contains(&"gpt-4o"));
        assert!(ids.contains(&"openai/o3-mini"));
        assert!(models.iter().all(|m| m.provider == "openai"));
        assert!(models.iter().all(|m| m.supports_tools.is_none()));
    }

    #[tokio::test]
    async fn test_discover_models_ollama_reports_tool_support() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/tags"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "models": [
                    { "name": "gemma3:4b", "capabilities": ["completion"] },
                    { "name": "qwen2.5:latest", "capabilities": ["completion", "tools"] }
                ]
            })))
            .mount(&server)
            .await;

        let mut providers = HashMap::new();
        providers.insert("ollama".to_string(), config_with_base("", &server.uri()));
        let (models, errors) = discover_models(&providers).await;

        assert!(errors.is_empty(), "unexpected errors: {errors:?}");
        assert_eq!(models.len(), 2);
        let gemma = models.iter().find(|m| m.raw_id == "gemma3:4b").unwrap();
        assert_eq!(gemma.id, "ollama/gemma3:4b");
        assert!(gemma.is_chat_only());
        let qwen = models.iter().find(|m| m.raw_id == "qwen2.5:latest").unwrap();
        assert_eq!(qwen.supports_tools, Some(true));
        assert!(!qwen.is_chat_only());
    }

    #[tokio::test]
    async fn test_unconfigured_providers_are_skipped_and_failures_reported() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/models"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&server)
            .await;

        let mut providers = HashMap::new();
        // Configured but rejects the key → reported, not fatal.
        providers.insert("openai".to_string(), config_with_base("bad", &server.uri()));
        // No key and not local → never queried.
        providers.insert("deepseek".to_string(), ProviderConfig::default());

        let (models, errors) = discover_models(&providers).await;
        assert!(models.is_empty());
        assert_eq!(errors.len(), 1);
        assert!(errors[0].starts_with("OpenAI:"), "{errors:?}");
    }

    #[tokio::test]
    async fn test_discover_models_empty_config() {
        let (models, errors) = discover_models(&HashMap::new()).await;
        assert!(models.is_empty());
        assert!(errors.is_empty());
    }
}
