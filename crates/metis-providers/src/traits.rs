//! LLM Provider trait — the core abstraction replacing LiteLLM.
//!
//! Every LLM backend (OpenAI, Anthropic, DeepSeek, Groq, …) implements this trait.
//! The `HttpProvider` in `http_provider.rs` covers all OpenAI-compatible APIs.

use async_trait::async_trait;
use metis_core::types::{LlmResponse, Message, ToolDefinition};

/// Configuration passed to each LLM call.
///
/// Replaces nanobot's `AgentConfig` subset used by providers.
#[derive(Clone, Debug)]
pub struct LlmRequestConfig {
    /// Maximum tokens to generate.
    pub max_tokens: u32,
    /// Sampling temperature (0.0 – 2.0).
    pub temperature: f64,
    /// Context window (in tokens) to load the model with. Only meaningful for
    /// local backends (Ollama); cloud providers ignore it. Ollama's default is
    /// 4096, which a re-sent conversation overflows quickly — and once it
    /// overflows, the server's prefix cache misses on every turn and the whole
    /// history is re-evaluated on each message.
    pub num_ctx: Option<u32>,
    /// How long the local backend should keep the model loaded after a request
    /// (e.g. `"30m"`, `"-1"` for forever). Only meaningful for Ollama; avoids
    /// paying the model-load penalty on the first message after idle.
    pub keep_alive: Option<String>,
}

impl Default for LlmRequestConfig {
    fn default() -> Self {
        Self {
            max_tokens: 4096,
            temperature: 0.7,
            num_ctx: None,
            keep_alive: None,
        }
    }
}

/// Trait that all LLM providers must implement.
///
/// Replaces nanobot's `LLMProvider` ABC.
/// The main implementation is `HttpProvider` which handles any OpenAI-compatible API.
#[async_trait]
pub trait LlmProvider: Send + Sync {
    /// Send a chat completion request.
    ///
    /// # Arguments
    /// * `messages` — Conversation history in OpenAI format.
    /// * `tools`    — Optional list of tool definitions the LLM can call.
    /// * `model`    — Model identifier (e.g. `"claude-sonnet-4-20250514"`, `"gpt-4o"`).
    /// * `config`   — Temperature, max_tokens, etc.
    ///
    /// # Returns
    /// An `LlmResponse` with content and/or tool calls.
    /// On API errors, returns `LlmResponse::error(...)` instead of propagating.
    async fn chat(
        &self,
        messages: &[Message],
        tools: Option<&[ToolDefinition]>,
        model: &str,
        config: &LlmRequestConfig,
    ) -> LlmResponse;

    /// Compute embeddings for a batch of texts.
    ///
    /// Used by the semantic memory store. Providers that don't support an
    /// embeddings endpoint keep this default and return an error string;
    /// callers treat that as "no vector search available" and fall back to
    /// keyword-only matching.
    async fn embeddings(&self, texts: &[String], model: &str) -> Result<Vec<Vec<f32>>, String> {
        let _ = (texts, model);
        Err(format!(
            "{} does not support embeddings",
            self.display_name()
        ))
    }

    /// Whether `model` can accept tool definitions.
    ///
    /// Chat-only models (e.g. Ollama gemma3) get a slimmed-down "direct chat"
    /// context from the agent loop — sending the full agent briefing to a
    /// model that can't act on it wastes minutes of CPU prompt evaluation on
    /// local hardware. Defaults to `true`; providers refine it from their
    /// backend's capability info or from observed rejections.
    async fn supports_tools(&self, model: &str) -> bool {
        let _ = model;
        true
    }

    /// The default model for this provider instance.
    fn default_model(&self) -> &str;

    /// Display name for logging.
    fn display_name(&self) -> &str;
}
