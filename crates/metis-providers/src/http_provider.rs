//! Generic HTTP-based LLM provider for OpenAI-compatible APIs.
//!
//! This is the **most important component** of the migration — it replaces LiteLLM
//! by talking directly to any OpenAI-compatible `/chat/completions` endpoint.
//!
//! Covers: OpenAI, Anthropic (via OpenRouter), DeepSeek, Groq, Gemini, ZhiPu,
//!         DashScope, Moonshot, MiniMax, vLLM, AiHubMix, OpenRouter.

use async_trait::async_trait;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use tracing::{debug, error, warn};

use metis_core::types::{
    ChatCompletionRequest, ChatCompletionResponse, LlmResponse, Message, ToolDefinition,
};

use crate::registry::{
    apply_model_overrides, resolve_model_name, ProviderConfig, ProviderSpec,
};
use crate::traits::{LlmProvider, LlmRequestConfig};

/// MiniMax `/v1/chat/completions` expects **bare** OpenAI-style model names (e.g. `MiniMax-M2`).
/// Our registry uses `minimax/...` for routing; the upstream API rejects that form (and is
/// picky about casing), so strip the prefix and normalize common slug typos.
fn minimax_openai_model_id(resolved: &str) -> String {
    let s = resolved.trim();
    let rest = if s
        .get(.."minimax/".len())
        .is_some_and(|p| p.eq_ignore_ascii_case("minimax/"))
    {
        s["minimax/".len()..].trim()
    } else {
        s
    };
    match rest.to_ascii_lowercase().as_str() {
        "minimax-m2" => "MiniMax-M2".to_string(),
        "minimax-m2.7" => "MiniMax-M2.7".to_string(),
        "minimax-m2.7-highspeed" => "MiniMax-M2.7-highspeed".to_string(),
        "minimax-m2.5" => "MiniMax-M2.5".to_string(),
        "minimax-m2.5-highspeed" => "MiniMax-M2.5-highspeed".to_string(),
        "minimax-m2.1" => "MiniMax-M2.1".to_string(),
        "minimax-m2.1-highspeed" => "MiniMax-M2.1-highspeed".to_string(),
        "m2-her" => "M2-her".to_string(),
        _ => rest.to_string(),
    }
}

// ─────────────────────────────────────────────
// HttpProvider
// ─────────────────────────────────────────────

/// A generic LLM provider that talks to any OpenAI-compatible HTTP API.
///
/// Replaces nanobot's `LiteLLMProvider` — instead of routing through LiteLLM,
/// we make direct HTTP requests via `reqwest`.
pub struct HttpProvider {
    /// HTTP client (shared, connection-pooled).
    client: reqwest::Client,
    /// API base URL (e.g. `"https://api.openai.com/v1"`).
    api_base: String,
    /// API key for Bearer authentication.
    api_key: String,
    /// Default model for this provider instance.
    default_model: String,
    /// Extra headers to send with each request (e.g. AiHubMix X-App-Code).
    extra_headers: HeaderMap,
    /// Reference to the provider spec for model resolution and overrides.
    spec: &'static ProviderSpec,
    /// Models that rejected tool definitions (e.g. Ollama 400 "does not
    /// support tools"). Further requests to them are sent without tools so
    /// plain chat keeps working instead of erroring on every message.
    no_tool_models: std::sync::RwLock<std::collections::HashSet<String>>,
    /// Whether Ollama's /api/tags has been probed for tool capabilities.
    tags_probed: std::sync::atomic::AtomicBool,
}

impl std::fmt::Debug for HttpProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpProvider")
            .field("api_base", &self.api_base)
            .field("default_model", &self.default_model)
            .field("provider", &self.spec.display_name)
            .finish()
    }
}

impl HttpProvider {
    /// Create a new HttpProvider from a provider config and spec.
    ///
    /// # Arguments
    /// * `config`  — User's config (api_key, api_base, extra_headers)
    /// * `spec`    — Static provider spec from the registry
    /// * `model`   — The default model to use
    pub fn new(config: &ProviderConfig, spec: &'static ProviderSpec, model: &str) -> Self {
        // Resolve API base: config > spec default > standard OpenAI path
        let api_base = config
            .api_base
            .clone()
            .or_else(|| spec.default_api_base.map(String::from))
            .unwrap_or_else(|| "https://api.openai.com/v1".to_string());

        // Build extra headers
        let mut extra_headers = HeaderMap::new();
        if let Some(ref headers) = config.extra_headers {
            for (key, value) in headers {
                if let (Ok(name), Ok(val)) = (
                    HeaderName::from_bytes(key.as_bytes()),
                    HeaderValue::from_str(value),
                ) {
                    extra_headers.insert(name, val);
                } else {
                    warn!("Invalid header: {}={}", key, value);
                }
            }
        }

        // Local backends (Ollama, LM Studio, vLLM) run models on the user's
        // own hardware: prompt evaluation on CPU can far exceed a cloud
        // provider's response time, so give them a much longer total timeout.
        // A short connect timeout keeps a *dead* local server failing fast —
        // the long timeout only applies once the server has accepted the
        // request and is actually working.
        let is_local_backend = matches!(spec.name, "ollama" | "lmstudio" | "vllm");
        let request_timeout = if is_local_backend { 600 } else { 120 };
        let client = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(5))
            .timeout(std::time::Duration::from_secs(request_timeout))
            .build()
            .expect("Failed to build HTTP client");

        HttpProvider {
            client,
            api_base,
            api_key: config.api_key.clone(),
            default_model: model.to_string(),
            extra_headers,
            spec,
            no_tool_models: std::sync::RwLock::new(std::collections::HashSet::new()),
            tags_probed: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// Fetch Ollama's model list and return the names WITHOUT tool support.
    ///
    /// Names ending in `:latest` are also recorded without the tag, since
    /// users typically configure `ollama/gemma3` rather than `gemma3:latest`.
    async fn fetch_ollama_no_tool_models(&self) -> Option<Vec<String>> {
        let base = self.api_base.trim_end_matches('/').trim_end_matches("/v1");
        let url = format!("{base}/api/tags");
        let resp = self
            .client
            .get(&url)
            .timeout(std::time::Duration::from_secs(4))
            .send()
            .await
            .ok()?;
        let json: serde_json::Value = resp.json().await.ok()?;
        let models = json.get("models")?.as_array()?;
        let mut out = Vec::new();
        for m in models {
            let Some(name) = m["name"].as_str() else { continue };
            let has_tools = m["capabilities"]
                .as_array()
                .is_some_and(|caps| caps.iter().any(|c| c.as_str() == Some("tools")));
            if !has_tools {
                out.push(name.to_string());
                if let Some(untagged) = name.strip_suffix(":latest") {
                    out.push(untagged.to_string());
                }
            }
        }
        Some(out)
    }

    /// Build the full chat completions URL.
    fn completions_url(&self) -> String {
        let base = self.api_base.trim_end_matches('/');
        format!("{}/chat/completions", base)
    }

    /// Build the full embeddings URL.
    fn embeddings_url(&self) -> String {
        let base = self.api_base.trim_end_matches('/');
        format!("{}/embeddings", base)
    }

    /// Resolve the model name for this provider (apply prefix/strip logic).
    fn resolve_model(&self, model: &str) -> String {
        let resolved = resolve_model_name(model, self.spec);
        if self.spec.name == "minimax" {
            minimax_openai_model_id(&resolved)
        } else {
            resolved
        }
    }

    /// Base URL of Ollama's native API (the OpenAI-compatible base minus `/v1`).
    fn ollama_native_base(&self) -> String {
        self.api_base
            .trim_end_matches('/')
            .trim_end_matches("/v1")
            .to_string()
    }

    /// Make sure Ollama's runner for `model` matches the requested runtime
    /// options (`num_ctx`, `keep_alive`) before the chat request goes out.
    ///
    /// Ollama's OpenAI-compatible endpoint silently ignores both fields, but a
    /// native `/api/chat` call with empty `messages` loads the model with them
    /// and returns immediately — and subsequent OpenAI-endpoint requests reuse
    /// that runner, keeping its context size and keep-alive. Without this, the
    /// runner stays at Ollama's default 4096-token context; once a growing
    /// conversation overflows that, the prefix cache misses on every turn and
    /// the whole history is re-evaluated per message (minutes on CPU).
    ///
    /// Best-effort: any failure is logged and the chat proceeds normally.
    async fn ensure_ollama_runtime(&self, resolved_model: &str, config: &LlmRequestConfig) {
        if self.spec.name != "ollama" || (config.num_ctx.is_none() && config.keep_alive.is_none())
        {
            return;
        }
        let base = self.ollama_native_base();

        // Skip the (potentially model-reloading) preload when the runner is
        // already up with a big-enough context. `/api/ps` is a ~1ms local call.
        if let Some(wanted) = config.num_ctx {
            let loaded_ok = async {
                let resp = self
                    .client
                    .get(format!("{base}/api/ps"))
                    .timeout(std::time::Duration::from_secs(3))
                    .send()
                    .await
                    .ok()?;
                let json: serde_json::Value = resp.json().await.ok()?;
                let with_latest = format!("{resolved_model}:latest");
                Some(json["models"].as_array()?.iter().any(|m| {
                    let name = m["name"].as_str().unwrap_or("");
                    (name == resolved_model || name == with_latest)
                        && m["context_length"].as_u64().unwrap_or(0) >= u64::from(wanted)
                }))
            }
            .await
            .unwrap_or(false);
            if loaded_ok {
                return;
            }
        }

        let mut body = serde_json::json!({
            "model": resolved_model,
            "messages": [],
        });
        if let Some(num_ctx) = config.num_ctx {
            body["options"] = serde_json::json!({ "num_ctx": num_ctx });
        }
        if let Some(ref keep_alive) = config.keep_alive {
            body["keep_alive"] = serde_json::Value::String(keep_alive.clone());
        }

        debug!(
            model = resolved_model,
            num_ctx = ?config.num_ctx,
            keep_alive = ?config.keep_alive,
            "preloading Ollama runner with requested runtime options"
        );
        // Loading a model from disk can take a while on CPU-only machines;
        // on timeout/failure the chat request itself will load it (with the
        // server-default context) so we never block chat on this.
        let result = self
            .client
            .post(format!("{base}/api/chat"))
            .json(&body)
            .timeout(std::time::Duration::from_secs(180))
            .send()
            .await;
        match result {
            Ok(resp) if !resp.status().is_success() => {
                warn!(
                    model = resolved_model,
                    status = %resp.status(),
                    "Ollama runner preload rejected; continuing with server defaults"
                );
            }
            Err(e) => {
                warn!(
                    model = resolved_model,
                    error = %e,
                    "Ollama runner preload failed; continuing with server defaults"
                );
            }
            Ok(_) => {}
        }
    }
}

#[async_trait]
impl LlmProvider for HttpProvider {
    async fn chat(
        &self,
        messages: &[Message],
        tools: Option<&[ToolDefinition]>,
        model: &str,
        config: &LlmRequestConfig,
    ) -> LlmResponse {
        let resolved_model = self.resolve_model(model);
        let temperature = apply_model_overrides(model, self.spec, config.temperature);

        debug!(
            provider = self.spec.display_name,
            model = %resolved_model,
            messages = messages.len(),
            tools = tools.map_or(0, |t| t.len()),
            "Calling LLM"
        );

        // Skip tool definitions for models that already rejected them
        // (chat-only mode) — otherwise every message would 400.
        let mut include_tools = tools.is_some()
            && !self
                .no_tool_models
                .read()
                .unwrap()
                .contains(&resolved_model);

        self.ensure_ollama_runtime(&resolved_model, config).await;

        let url = self.completions_url();

        loop {
            let request_body = ChatCompletionRequest {
                model: resolved_model.clone(),
                messages: messages.to_vec(),
                tools: if include_tools { tools.map(|t| t.to_vec()) } else { None },
                tool_choice: if include_tools { Some("auto".to_string()) } else { None },
                max_tokens: Some(config.max_tokens),
                temperature: Some(temperature),
            };

            let result = self
                .client
                .post(&url)
                .bearer_auth(&self.api_key)
                .headers(self.extra_headers.clone())
                .json(&request_body)
                .send()
                .await;

            let response = match result {
                Ok(resp) => resp,
                Err(e) => {
                    error!(provider = self.spec.display_name, error = %e, "HTTP request failed");
                    return LlmResponse::error(format!("Error calling LLM: {}", e));
                }
            };

            let status = response.status();
            if !status.is_success() {
                let error_text = response
                    .text()
                    .await
                    .unwrap_or_else(|_| "Failed to read error body".to_string());

                // Model can't take tool definitions (e.g. Ollama gemma3):
                // remember that and retry once in chat-only mode.
                if include_tools
                    && status.as_u16() == 400
                    && error_text.to_lowercase().contains("does not support tools")
                {
                    warn!(
                        provider = self.spec.display_name,
                        model = %resolved_model,
                        "model does not support tools — retrying in chat-only mode"
                    );
                    self.no_tool_models
                        .write()
                        .unwrap()
                        .insert(resolved_model.clone());
                    include_tools = false;
                    continue;
                }

                error!(
                    provider = self.spec.display_name,
                    status = %status,
                    body = %error_text,
                    "API error"
                );
                return LlmResponse::error(format!(
                    "Error calling LLM: {} — {}",
                    status, error_text
                ));
            }

            return match response.json::<ChatCompletionResponse>().await {
                Ok(chat_resp) => {
                    let llm_resp: LlmResponse = chat_resp.into();
                    debug!(
                        provider = self.spec.display_name,
                        has_content = llm_resp.content.is_some(),
                        tool_calls = llm_resp.tool_calls.len(),
                        finish_reason = llm_resp.finish_reason.as_deref().unwrap_or("?"),
                        "LLM response received"
                    );
                    llm_resp
                }
                Err(e) => {
                    error!(
                        provider = self.spec.display_name,
                        error = %e,
                        "Failed to parse LLM response"
                    );
                    LlmResponse::error(format!("Error parsing LLM response: {}", e))
                }
            };
        }
    }

    async fn supports_tools(&self, model: &str) -> bool {
        let resolved = self.resolve_model(model);
        if self.no_tool_models.read().unwrap().contains(&resolved) {
            return false;
        }
        // Ollama tells us capabilities up front — probe once so the agent
        // loop can pick the lite context BEFORE the first slow request.
        if self.spec.name == "ollama"
            && !self
                .tags_probed
                .swap(true, std::sync::atomic::Ordering::SeqCst)
        {
            if let Some(no_tool) = self.fetch_ollama_no_tool_models().await {
                let mut set = self.no_tool_models.write().unwrap();
                for m in no_tool {
                    set.insert(m);
                }
            }
            return !self.no_tool_models.read().unwrap().contains(&resolved);
        }
        true
    }

    async fn embeddings(&self, texts: &[String], model: &str) -> Result<Vec<Vec<f32>>, String> {
        #[derive(serde::Deserialize)]
        struct EmbeddingsResponse {
            data: Vec<EmbeddingItem>,
        }
        #[derive(serde::Deserialize)]
        struct EmbeddingItem {
            embedding: Vec<f32>,
            #[serde(default)]
            index: usize,
        }

        if texts.is_empty() {
            return Ok(Vec::new());
        }

        let resolved_model = self.resolve_model(model);
        let body = serde_json::json!({
            "model": resolved_model,
            "input": texts,
        });

        let response = self
            .client
            .post(self.embeddings_url())
            .bearer_auth(&self.api_key)
            .headers(self.extra_headers.clone())
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("Embeddings request failed: {e}"))?;

        let status = response.status();
        if !status.is_success() {
            let error_text = response
                .text()
                .await
                .unwrap_or_else(|_| "Failed to read error body".to_string());
            return Err(format!("Embeddings API error: {status} — {error_text}"));
        }

        let parsed: EmbeddingsResponse = response
            .json()
            .await
            .map_err(|e| format!("Failed to parse embeddings response: {e}"))?;

        let mut items = parsed.data;
        items.sort_by_key(|i| i.index);
        Ok(items.into_iter().map(|i| i.embedding).collect())
    }

    fn default_model(&self) -> &str {
        &self.default_model
    }

    fn display_name(&self) -> &str {
        self.spec.display_name
    }
}

// ─────────────────────────────────────────────
// Builder (convenience)
// ─────────────────────────────────────────────

/// Build an HttpProvider from a model name and a map of provider configs.
///
/// This is the main entry point — it matches the model to a provider,
/// reads the config, and creates the HttpProvider.
///
/// Replaces nanobot's CLI instantiation logic.
/// A provider chosen at runtime by wire format.
///
/// Almost every backend speaks OpenAI's `/chat/completions`; Anthropic's
/// native Messages API is the exception, with its own auth header, request
/// shape, and content-block responses. Dispatching here keeps the rest of
/// the codebase working against `LlmProvider` without caring which.
#[derive(Debug)]
pub enum Provider {
    Http(HttpProvider),
    Anthropic(crate::anthropic::AnthropicProvider),
}

#[async_trait]
impl LlmProvider for Provider {
    async fn chat(
        &self,
        messages: &[Message],
        tools: Option<&[ToolDefinition]>,
        model: &str,
        config: &LlmRequestConfig,
    ) -> LlmResponse {
        match self {
            Provider::Http(p) => p.chat(messages, tools, model, config).await,
            Provider::Anthropic(p) => p.chat(messages, tools, model, config).await,
        }
    }

    async fn embeddings(&self, texts: &[String], model: &str) -> Result<Vec<Vec<f32>>, String> {
        match self {
            Provider::Http(p) => p.embeddings(texts, model).await,
            Provider::Anthropic(p) => p.embeddings(texts, model).await,
        }
    }

    async fn supports_tools(&self, model: &str) -> bool {
        match self {
            Provider::Http(p) => p.supports_tools(model).await,
            Provider::Anthropic(p) => p.supports_tools(model).await,
        }
    }

    fn default_model(&self) -> &str {
        match self {
            Provider::Http(p) => p.default_model(),
            Provider::Anthropic(p) => p.default_model(),
        }
    }

    fn display_name(&self) -> &str {
        match self {
            Provider::Http(p) => p.display_name(),
            Provider::Anthropic(p) => p.display_name(),
        }
    }
}

pub fn create_provider(
    model: &str,
    providers: &std::collections::HashMap<String, ProviderConfig>,
) -> Result<Provider, String> {
    let (config, spec) = crate::registry::match_provider(model, providers)
        .ok_or_else(|| {
            format!(
                "No configured provider found for model '{}'. \
                 Set the appropriate API key (e.g. ANTHROPIC_API_KEY, OPENROUTER_API_KEY).",
                model
            )
        })?;

    debug!(
        provider = spec.display_name,
        model = model,
        api_base = config.api_base.as_deref().unwrap_or("default"),
        "Creating LLM provider"
    );

    // Direct-to-Anthropic gets the native Messages API; a Claude model
    // reached through a gateway (OpenRouter etc.) matched that gateway's
    // spec above and stays on the OpenAI wire format the gateway expects.
    if spec.name == "anthropic" {
        return Ok(Provider::Anthropic(crate::anthropic::AnthropicProvider::new(
            config, model,
        )));
    }
    Ok(Provider::Http(HttpProvider::new(config, spec, model)))
}

// ─────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::find_by_name;
    use std::collections::HashMap;
    use wiremock::matchers::{body_partial_json, header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn make_config(api_key: &str, api_base: Option<&str>) -> ProviderConfig {
        ProviderConfig {
            api_key: api_key.to_string(),
            api_base: api_base.map(String::from),
            extra_headers: None,
        }
    }

    // ── Unit tests ──

    #[test]
    fn test_minimax_openai_model_id_strips_prefix_and_fixes_casing() {
        assert_eq!(minimax_openai_model_id("minimax/MiniMax-M2"), "MiniMax-M2");
        assert_eq!(minimax_openai_model_id("minimax/minimax-m2"), "MiniMax-M2");
        assert_eq!(minimax_openai_model_id("MiniMax-M2"), "MiniMax-M2");
        assert_eq!(
            minimax_openai_model_id("minimax/MiniMax-M2.7-highspeed"),
            "MiniMax-M2.7-highspeed"
        );
    }

    #[test]
    fn test_completions_url_trailing_slash() {
        let spec = find_by_name("openai").unwrap();
        let config = make_config("key", Some("https://api.openai.com/v1/"));
        let provider = HttpProvider::new(&config, spec, "gpt-4o");
        assert_eq!(
            provider.completions_url(),
            "https://api.openai.com/v1/chat/completions"
        );
    }

    #[test]
    fn test_completions_url_no_trailing_slash() {
        let spec = find_by_name("openai").unwrap();
        let config = make_config("key", Some("https://api.openai.com/v1"));
        let provider = HttpProvider::new(&config, spec, "gpt-4o");
        assert_eq!(
            provider.completions_url(),
            "https://api.openai.com/v1/chat/completions"
        );
    }

    #[test]
    fn test_default_api_base_for_gateway() {
        let spec = find_by_name("openrouter").unwrap();
        let config = make_config("sk-or-abc", None);
        let provider = HttpProvider::new(&config, spec, "meta-llama/llama-3");
        assert_eq!(provider.api_base, "https://openrouter.ai/api/v1");
    }

    #[test]
    fn test_config_overrides_default_base() {
        let spec = find_by_name("openrouter").unwrap();
        let config = make_config("sk-or-abc", Some("https://custom.proxy.com/v1"));
        let provider = HttpProvider::new(&config, spec, "meta-llama/llama-3");
        assert_eq!(provider.api_base, "https://custom.proxy.com/v1");
    }

    #[test]
    fn test_model_resolution_in_provider() {
        let spec = find_by_name("deepseek").unwrap();
        let config = make_config("key", None);
        let provider = HttpProvider::new(&config, spec, "deepseek-chat");
        assert_eq!(provider.resolve_model("deepseek-chat"), "deepseek/deepseek-chat");
    }

    #[test]
    fn test_display_name() {
        let spec = find_by_name("groq").unwrap();
        let config = make_config("key", None);
        let provider = HttpProvider::new(&config, spec, "llama-3.3-70b");
        assert_eq!(provider.display_name(), "Groq");
    }

    #[test]
    fn test_extra_headers() {
        let spec = find_by_name("aihubmix").unwrap();
        let mut headers = HashMap::new();
        headers.insert("X-App-Code".to_string(), "my-app-code".to_string());
        let config = ProviderConfig {
            api_key: "key".to_string(),
            api_base: None,
            extra_headers: Some(headers),
        };
        let provider = HttpProvider::new(&config, spec, "gpt-4o");
        assert!(provider.extra_headers.contains_key("x-app-code"));
    }

    // ── Integration tests with mock server ──

    #[tokio::test]
    async fn test_chat_success() {
        let mock_server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .and(header("Authorization", "Bearer test-key-123"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "chatcmpl-test",
                "choices": [{
                    "message": {
                        "content": "Hello! I'm Metis.",
                        "tool_calls": null
                    },
                    "finish_reason": "stop"
                }],
                "usage": {
                    "prompt_tokens": 10,
                    "completion_tokens": 5,
                    "total_tokens": 15
                }
            })))
            .mount(&mock_server)
            .await;

        let spec = find_by_name("openai").unwrap();
        let config = make_config("test-key-123", Some(&mock_server.uri()));
        let provider = HttpProvider::new(&config, spec, "gpt-4o");

        let messages = vec![
            Message::system("You are Metis."),
            Message::user("Hello"),
        ];
        let req_config = LlmRequestConfig::default();

        let resp = provider.chat(&messages, None, "gpt-4o", &req_config).await;

        assert_eq!(resp.content.as_deref(), Some("Hello! I'm Metis."));
        assert!(!resp.has_tool_calls());
        assert_eq!(resp.finish_reason.as_deref(), Some("stop"));
        assert_eq!(resp.usage.as_ref().unwrap().total_tokens, 15);
    }

    #[tokio::test]
    async fn test_chat_with_tool_calls() {
        let mock_server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "chatcmpl-tools",
                "choices": [{
                    "message": {
                        "content": null,
                        "tool_calls": [{
                            "id": "call_abc123",
                            "type": "function",
                            "function": {
                                "name": "web_search",
                                "arguments": "{\"query\": \"Rust programming\"}"
                            }
                        }]
                    },
                    "finish_reason": "tool_calls"
                }],
                "usage": {
                    "prompt_tokens": 20,
                    "completion_tokens": 15,
                    "total_tokens": 35
                }
            })))
            .mount(&mock_server)
            .await;

        let spec = find_by_name("openai").unwrap();
        let config = make_config("key", Some(&mock_server.uri()));
        let provider = HttpProvider::new(&config, spec, "gpt-4o");

        let tool_def = ToolDefinition::new(
            "web_search",
            "Search the web",
            serde_json::json!({"type": "object", "properties": {"query": {"type": "string"}}}),
        );

        let messages = vec![Message::user("Search for Rust")];
        let req_config = LlmRequestConfig::default();

        let resp = provider
            .chat(&messages, Some(&[tool_def]), "gpt-4o", &req_config)
            .await;

        assert!(resp.content.is_none());
        assert!(resp.has_tool_calls());
        assert_eq!(resp.tool_calls.len(), 1);
        assert_eq!(resp.tool_calls[0].function.name, "web_search");
        assert_eq!(resp.tool_calls[0].id, "call_abc123");
    }

    #[tokio::test]
    async fn test_chat_api_error() {
        let mock_server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(
                ResponseTemplate::new(429).set_body_json(serde_json::json!({
                    "error": {
                        "message": "Rate limit exceeded",
                        "type": "rate_limit_error"
                    }
                })),
            )
            .mount(&mock_server)
            .await;

        let spec = find_by_name("openai").unwrap();
        let config = make_config("key", Some(&mock_server.uri()));
        let provider = HttpProvider::new(&config, spec, "gpt-4o");

        let messages = vec![Message::user("Hello")];
        let req_config = LlmRequestConfig::default();

        let resp = provider.chat(&messages, None, "gpt-4o", &req_config).await;

        // Should return error message, not panic
        assert!(resp.content.is_some());
        let content = resp.content.unwrap();
        assert!(content.contains("Error calling LLM"));
        assert!(content.contains("429"));
    }

    #[tokio::test]
    async fn test_chat_network_error() {
        // Point to a port that's not listening
        let spec = find_by_name("openai").unwrap();
        let config = make_config("key", Some("http://127.0.0.1:1"));
        let provider = HttpProvider::new(&config, spec, "gpt-4o");

        let messages = vec![Message::user("Hello")];
        let req_config = LlmRequestConfig::default();

        let resp = provider.chat(&messages, None, "gpt-4o", &req_config).await;

        assert!(resp.content.is_some());
        assert!(resp.content.unwrap().contains("Error calling LLM"));
    }

    #[tokio::test]
    async fn test_chat_sends_correct_body() {
        let mock_server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .and(body_partial_json(serde_json::json!({
                "model": "deepseek/deepseek-chat",
                "max_tokens": 4096
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "chatcmpl-body",
                "choices": [{
                    "message": { "content": "ok" },
                    "finish_reason": "stop"
                }],
                "usage": null
            })))
            .mount(&mock_server)
            .await;

        let spec = find_by_name("deepseek").unwrap();
        let config = make_config("ds-key", Some(&mock_server.uri()));
        let provider = HttpProvider::new(&config, spec, "deepseek-chat");

        let messages = vec![Message::user("test")];
        let req_config = LlmRequestConfig::default();

        let resp = provider
            .chat(&messages, None, "deepseek-chat", &req_config)
            .await;

        // If the body matcher fails, wiremock returns 404 → we'd get an error
        assert_eq!(resp.content.as_deref(), Some("ok"));
    }

    #[tokio::test]
    async fn test_chat_with_reasoning_content() {
        let mock_server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "chatcmpl-reasoning",
                "choices": [{
                    "message": {
                        "content": "The answer is 42.",
                        "reasoning_content": "Let me think step by step..."
                    },
                    "finish_reason": "stop"
                }],
                "usage": null
            })))
            .mount(&mock_server)
            .await;

        let spec = find_by_name("deepseek").unwrap();
        let config = make_config("key", Some(&mock_server.uri()));
        let provider = HttpProvider::new(&config, spec, "deepseek-reasoner");

        let messages = vec![Message::user("What is the meaning of life?")];
        let req_config = LlmRequestConfig::default();

        let resp = provider
            .chat(&messages, None, "deepseek-reasoner", &req_config)
            .await;

        assert_eq!(resp.content.as_deref(), Some("The answer is 42."));
        assert_eq!(
            resp.reasoning_content.as_deref(),
            Some("Let me think step by step...")
        );
    }

    #[tokio::test]
    async fn test_chat_falls_back_to_chat_only_when_tools_rejected() {
        let mock_server = MockServer::start().await;

        // Requests WITH a tools field → Ollama-style 400 (higher priority).
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .and(body_partial_json(serde_json::json!({"tool_choice": "auto"})))
            .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({
                "error": {
                    "message": "registry.ollama.ai/library/gemma3:4b does not support tools",
                    "type": "invalid_request_error"
                }
            })))
            .with_priority(1)
            .mount(&mock_server)
            .await;

        // Requests without tools → success.
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "chatcmpl-fallback",
                "choices": [{
                    "message": { "content": "Hi! (chat-only)" },
                    "finish_reason": "stop"
                }],
                "usage": null
            })))
            .with_priority(2)
            .mount(&mock_server)
            .await;

        let spec = find_by_name("ollama").unwrap();
        let config = make_config("", Some(&mock_server.uri()));
        let provider = HttpProvider::new(&config, spec, "gemma3:4b");

        let tool_def = ToolDefinition::new(
            "exec",
            "Run a command",
            serde_json::json!({"type": "object", "properties": {"command": {"type": "string"}}}),
        );
        let messages = vec![Message::user("hi")];
        let req_config = LlmRequestConfig::default();

        // First call: 400 with tools → transparent retry without tools.
        let resp = provider
            .chat(&messages, Some(&[tool_def.clone()]), "gemma3:4b", &req_config)
            .await;
        assert_eq!(resp.content.as_deref(), Some("Hi! (chat-only)"));

        // Second call: the model is remembered as chat-only → no 400 round-trip.
        let resp = provider
            .chat(&messages, Some(&[tool_def]), "gemma3:4b", &req_config)
            .await;
        assert_eq!(resp.content.as_deref(), Some("Hi! (chat-only)"));

        // Exactly one 400 was consumed: 3 requests total (400 + 2 × 200).
        let received = mock_server.received_requests().await.unwrap();
        assert_eq!(received.len(), 3);
    }

    #[tokio::test]
    async fn test_supports_tools_probes_ollama_tags() {
        let mock_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/tags"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "models": [
                    { "name": "gemma3:4b", "capabilities": ["completion"] },
                    { "name": "qwen2.5:latest", "capabilities": ["completion", "tools"] }
                ]
            })))
            .mount(&mock_server)
            .await;

        let spec = find_by_name("ollama").unwrap();
        let config = make_config("", Some(&mock_server.uri()));
        let provider = HttpProvider::new(&config, spec, "gemma3:4b");

        assert!(!provider.supports_tools("gemma3:4b").await);
        assert!(provider.supports_tools("qwen2.5:latest").await);
        // Probe happens once; results are cached.
        assert!(!provider.supports_tools("gemma3:4b").await);
        let tag_requests = mock_server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .filter(|r| r.url.path() == "/api/tags")
            .count();
        assert_eq!(tag_requests, 1);
    }

    #[tokio::test]
    async fn test_ollama_runtime_preload_fires_when_not_loaded() {
        let mock_server = MockServer::start().await;
        // Nothing loaded → the preload must fire with the runtime options.
        Mock::given(method("GET"))
            .and(path("/api/ps"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"models": []})))
            .mount(&mock_server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .and(body_partial_json(serde_json::json!({
                "keep_alive": "30m",
                "options": { "num_ctx": 8192 }
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "done": true, "done_reason": "load"
            })))
            .expect(1)
            .mount(&mock_server)
            .await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "x",
                "choices": [{ "message": { "content": "ok" }, "finish_reason": "stop" }],
                "usage": null
            })))
            .mount(&mock_server)
            .await;

        let spec = find_by_name("ollama").unwrap();
        let config = make_config("ollama", Some(&mock_server.uri()));
        let provider = HttpProvider::new(&config, spec, "gemma3:4b");
        let req = LlmRequestConfig {
            num_ctx: Some(8192),
            keep_alive: Some("30m".into()),
            ..Default::default()
        };
        let resp = provider
            .chat(&[Message::user("hi")], None, "gemma3:4b", &req)
            .await;
        assert_eq!(resp.content.as_deref(), Some("ok"));
    }

    #[tokio::test]
    async fn test_ollama_runtime_preload_skipped_when_already_loaded() {
        let mock_server = MockServer::start().await;
        // Runner already up with a big-enough context → no preload POST.
        Mock::given(method("GET"))
            .and(path("/api/ps"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "models": [{ "name": "gemma3:4b", "context_length": 8192 }]
            })))
            .mount(&mock_server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"done": true})))
            .expect(0)
            .mount(&mock_server)
            .await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "x",
                "choices": [{ "message": { "content": "ok" }, "finish_reason": "stop" }],
                "usage": null
            })))
            .mount(&mock_server)
            .await;

        let spec = find_by_name("ollama").unwrap();
        let config = make_config("ollama", Some(&mock_server.uri()));
        let provider = HttpProvider::new(&config, spec, "gemma3:4b");
        let req = LlmRequestConfig {
            num_ctx: Some(8192),
            keep_alive: Some("30m".into()),
            ..Default::default()
        };
        let resp = provider
            .chat(&[Message::user("hi")], None, "gemma3:4b", &req)
            .await;
        assert_eq!(resp.content.as_deref(), Some("ok"));
    }

    #[tokio::test]
    async fn test_no_ollama_preload_for_cloud_providers() {
        let mock_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/ps"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"models": []})))
            .expect(0)
            .mount(&mock_server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"done": true})))
            .expect(0)
            .mount(&mock_server)
            .await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "x",
                "choices": [{ "message": { "content": "ok" }, "finish_reason": "stop" }],
                "usage": null
            })))
            .mount(&mock_server)
            .await;

        let spec = find_by_name("openai").unwrap();
        let config = make_config("key", Some(&mock_server.uri()));
        let provider = HttpProvider::new(&config, spec, "gpt-4o");
        // num_ctx/keep_alive set but provider is not Ollama → ignored.
        let req = LlmRequestConfig {
            num_ctx: Some(8192),
            keep_alive: Some("30m".into()),
            ..Default::default()
        };
        let resp = provider
            .chat(&[Message::user("hi")], None, "gpt-4o", &req)
            .await;
        assert_eq!(resp.content.as_deref(), Some("ok"));
    }

    #[tokio::test]
    async fn test_supports_tools_learned_from_rejection() {
        // Non-Ollama provider: no probe, but a 400 rejection teaches it.
        let mock_server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .and(body_partial_json(serde_json::json!({"tool_choice": "auto"})))
            .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({
                "error": { "message": "model does not support tools" }
            })))
            .with_priority(1)
            .mount(&mock_server)
            .await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "x",
                "choices": [{ "message": { "content": "ok" }, "finish_reason": "stop" }],
                "usage": null
            })))
            .with_priority(2)
            .mount(&mock_server)
            .await;

        let spec = find_by_name("vllm").unwrap();
        let config = make_config("", Some(&mock_server.uri()));
        let provider = HttpProvider::new(&config, spec, "some-model");

        assert!(provider.supports_tools("some-model").await);
        let tool_def = ToolDefinition::new("t", "d", serde_json::json!({"type":"object"}));
        let _ = provider
            .chat(&[Message::user("hi")], Some(&[tool_def]), "some-model", &LlmRequestConfig::default())
            .await;
        assert!(!provider.supports_tools("some-model").await);
    }

    #[tokio::test]
    async fn test_chat_other_400_errors_not_retried() {
        let mock_server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({
                "error": { "message": "context length exceeded", "type": "invalid_request_error" }
            })))
            .mount(&mock_server)
            .await;

        let spec = find_by_name("openai").unwrap();
        let config = make_config("key", Some(&mock_server.uri()));
        let provider = HttpProvider::new(&config, spec, "gpt-4o");

        let resp = provider
            .chat(&[Message::user("hi")], None, "gpt-4o", &LlmRequestConfig::default())
            .await;
        assert!(resp.content.unwrap().contains("context length exceeded"));
        // No retry for unrelated 400s.
        assert_eq!(mock_server.received_requests().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn test_embeddings_success() {
        let mock_server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/embeddings"))
            .and(header("Authorization", "Bearer emb-key"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "object": "list",
                "data": [
                    { "object": "embedding", "index": 1, "embedding": [0.4, 0.5] },
                    { "object": "embedding", "index": 0, "embedding": [0.1, 0.2] }
                ],
                "model": "text-embedding-3-small"
            })))
            .mount(&mock_server)
            .await;

        let spec = find_by_name("openai").unwrap();
        let config = make_config("emb-key", Some(&mock_server.uri()));
        let provider = HttpProvider::new(&config, spec, "gpt-4o");

        let out = provider
            .embeddings(
                &["first".to_string(), "second".to_string()],
                "text-embedding-3-small",
            )
            .await
            .unwrap();

        // Out-of-order indices must be re-sorted
        assert_eq!(out, vec![vec![0.1, 0.2], vec![0.4, 0.5]]);
    }

    #[tokio::test]
    async fn test_embeddings_api_error() {
        let mock_server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/embeddings"))
            .respond_with(ResponseTemplate::new(404).set_body_string("no such model"))
            .mount(&mock_server)
            .await;

        let spec = find_by_name("openai").unwrap();
        let config = make_config("key", Some(&mock_server.uri()));
        let provider = HttpProvider::new(&config, spec, "gpt-4o");

        let err = provider
            .embeddings(&["text".to_string()], "bad-model")
            .await
            .unwrap_err();
        assert!(err.contains("404"));
    }

    #[tokio::test]
    async fn test_embeddings_empty_input() {
        let spec = find_by_name("openai").unwrap();
        let config = make_config("key", Some("http://127.0.0.1:1"));
        let provider = HttpProvider::new(&config, spec, "gpt-4o");
        // Must not even attempt a network call
        let out = provider.embeddings(&[], "text-embedding-3-small").await.unwrap();
        assert!(out.is_empty());
    }

    // ── create_provider ──

    #[test]
    fn test_create_provider_success() {
        let mut providers = HashMap::new();
        providers.insert(
            "anthropic".to_string(),
            make_config("sk-ant-123", None),
        );

        let provider = create_provider("claude-sonnet-4-20250514", &providers).unwrap();
        assert_eq!(provider.display_name(), "Anthropic");
        assert_eq!(provider.default_model(), "claude-sonnet-4-20250514");
    }

    #[test]
    fn test_create_provider_no_config() {
        let providers = HashMap::new();
        let err = create_provider("claude-3", &providers).unwrap_err();
        assert!(err.contains("No configured provider"));
        assert!(err.contains("claude-3"));
    }
}
