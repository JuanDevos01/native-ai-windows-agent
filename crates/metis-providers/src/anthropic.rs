//! Native Anthropic Messages API provider.
//!
//! Talks to `POST /v1/messages` directly — Anthropic's own wire format, not
//! the OpenAI shape `HttpProvider` speaks. The two differ in every place
//! that matters: auth header (`x-api-key`, not `Authorization: Bearer`),
//! system prompt (top-level field, not a message), tool definitions
//! (`input_schema`, not `function.parameters`), and tool calls/results
//! (typed content blocks, not `tool_calls` arrays).
//!
//! Design notes:
//! - **No `temperature` is ever sent.** Current Claude models (Opus 5,
//!   Sonnet 5, the 4.7/4.8 family) reject sampling parameters with a 400;
//!   the server default is correct for the rest.
//! - **No `thinking` parameter either** — current models run adaptive
//!   thinking by default, which is the recommended configuration.
//! - **Top-level `cache_control` is always on.** The agent loop re-sends the
//!   whole history plus a large tool list every turn; automatic prefix
//!   caching cuts the cost of that by up to 90% and costs nothing when it
//!   misses.
//! - Errors come back as `LlmResponse::error(...)`, matching `HttpProvider`,
//!   so the agent loop's error handling is provider-agnostic.

use async_trait::async_trait;
use serde_json::{json, Value};
use tracing::{debug, warn};

use metis_core::types::{
    ContentPart, LlmResponse, Message, MessageContent, ToolCall, ToolDefinition, UsageInfo,
};

use crate::registry::ProviderConfig;
use crate::traits::{LlmProvider, LlmRequestConfig};

const DEFAULT_API_BASE: &str = "https://api.anthropic.com";
const API_VERSION: &str = "2023-06-01";

/// Native Anthropic Messages API client.
#[derive(Debug)]
pub struct AnthropicProvider {
    api_key: String,
    api_base: String,
    model: String,
    http: reqwest::Client,
}

impl AnthropicProvider {
    pub fn new(config: &ProviderConfig, model: &str) -> Self {
        let api_base = config
            .api_base
            .clone()
            .unwrap_or_else(|| DEFAULT_API_BASE.to_string())
            .trim_end_matches('/')
            .to_string();
        Self {
            api_key: config.api_key.clone(),
            api_base,
            model: model.to_string(),
            http: reqwest::Client::new(),
        }
    }

    fn endpoint(&self) -> String {
        format!("{}/v1/messages", self.api_base)
    }
}

// ─────────────────────────────────────────────
// OpenAI-shaped history → Anthropic request
// ─────────────────────────────────────────────

/// Convert one OpenAI-style user content into Anthropic content blocks.
fn user_content_blocks(content: &MessageContent) -> Vec<Value> {
    match content {
        MessageContent::Text(t) => vec![json!({"type": "text", "text": t})],
        MessageContent::Parts(parts) => parts
            .iter()
            .map(|p| match p {
                ContentPart::Text { text } => json!({"type": "text", "text": text}),
                ContentPart::ImageUrl { image_url } => {
                    // Metis stores images as data URIs or plain URLs; Anthropic
                    // wants them as typed sources.
                    if let Some(rest) = image_url.url.strip_prefix("data:") {
                        let media_type = rest.split(';').next().unwrap_or("image/png");
                        let data = rest.split("base64,").nth(1).unwrap_or_default();
                        json!({
                            "type": "image",
                            "source": {
                                "type": "base64",
                                "media_type": media_type,
                                "data": data
                            }
                        })
                    } else {
                        json!({
                            "type": "image",
                            "source": {"type": "url", "url": image_url.url}
                        })
                    }
                }
            })
            .collect(),
    }
}

/// Split an OpenAI-shaped history into Anthropic's `(system, messages)`.
///
/// Rules that differ from a naive per-message mapping:
/// - System messages anywhere in the history are folded into the top-level
///   `system` string (the agent loop injects ledger/system turns mid-history).
/// - Assistant tool calls become `tool_use` content blocks.
/// - Tool results become `tool_result` blocks inside a USER message, and
///   consecutive results are merged into one user message — Anthropic expects
///   all results for a parallel tool round in a single turn.
fn convert_messages(messages: &[Message]) -> (String, Vec<Value>) {
    let mut system = String::new();
    let mut out: Vec<Value> = Vec::new();
    // Collects consecutive tool results so they land in one user turn.
    let mut pending_tool_results: Vec<Value> = Vec::new();

    let flush_tools = |pending: &mut Vec<Value>, out: &mut Vec<Value>| {
        if !pending.is_empty() {
            out.push(json!({"role": "user", "content": std::mem::take(pending)}));
        }
    };

    for msg in messages {
        match msg {
            Message::System { content } => {
                if !system.is_empty() {
                    system.push_str("\n\n");
                }
                system.push_str(content);
            }
            Message::User { content } => {
                flush_tools(&mut pending_tool_results, &mut out);
                out.push(json!({"role": "user", "content": user_content_blocks(content)}));
            }
            Message::Assistant {
                content,
                tool_calls,
                ..
            } => {
                flush_tools(&mut pending_tool_results, &mut out);
                let mut blocks: Vec<Value> = Vec::new();
                if let Some(text) = content {
                    if !text.trim().is_empty() {
                        blocks.push(json!({"type": "text", "text": text}));
                    }
                }
                if let Some(calls) = tool_calls {
                    for call in calls {
                        // Arguments arrive as a JSON string; Anthropic wants
                        // the object itself. Unparseable input degrades to an
                        // empty object rather than poisoning the request.
                        let input: Value = serde_json::from_str(&call.function.arguments)
                            .unwrap_or_else(|_| json!({}));
                        blocks.push(json!({
                            "type": "tool_use",
                            "id": call.id,
                            "name": call.function.name,
                            "input": input
                        }));
                    }
                }
                if !blocks.is_empty() {
                    out.push(json!({"role": "assistant", "content": blocks}));
                }
            }
            Message::Tool {
                content,
                tool_call_id,
            } => {
                pending_tool_results.push(json!({
                    "type": "tool_result",
                    "tool_use_id": tool_call_id,
                    "content": content
                }));
            }
        }
    }
    flush_tools(&mut pending_tool_results, &mut out);
    (system, out)
}

/// Convert Metis tool definitions into Anthropic's schema shape.
fn convert_tools(tools: &[ToolDefinition]) -> Vec<Value> {
    tools
        .iter()
        .map(|t| {
            json!({
                "name": t.function.name,
                "description": t.function.description,
                "input_schema": t.function.parameters
            })
        })
        .collect()
}

// ─────────────────────────────────────────────
// Anthropic response → LlmResponse
// ─────────────────────────────────────────────

fn parse_response(body: &Value) -> LlmResponse {
    let mut text = String::new();
    let mut reasoning = String::new();
    let mut tool_calls: Vec<ToolCall> = Vec::new();

    if let Some(blocks) = body.get("content").and_then(|c| c.as_array()) {
        for block in blocks {
            match block.get("type").and_then(|t| t.as_str()) {
                Some("text") => {
                    if let Some(t) = block.get("text").and_then(|t| t.as_str()) {
                        text.push_str(t);
                    }
                }
                Some("thinking") => {
                    if let Some(t) = block.get("thinking").and_then(|t| t.as_str()) {
                        reasoning.push_str(t);
                    }
                }
                Some("tool_use") => {
                    let id = block.get("id").and_then(|v| v.as_str()).unwrap_or_default();
                    let name = block
                        .get("name")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default();
                    let args = block
                        .get("input")
                        .map(|i| i.to_string())
                        .unwrap_or_else(|| "{}".to_string());
                    tool_calls.push(ToolCall::new(id, name, args));
                }
                _ => {}
            }
        }
    }

    let stop_reason = body.get("stop_reason").and_then(|s| s.as_str());
    // Map to the OpenAI-style values the agent loop already understands.
    let finish_reason = match stop_reason {
        Some("end_turn") | Some("stop_sequence") => Some("stop".to_string()),
        Some("tool_use") => Some("tool_calls".to_string()),
        Some("max_tokens") => Some("length".to_string()),
        Some(other) => Some(other.to_string()),
        None => None,
    };

    // A refusal arrives as HTTP 200 with empty-ish content; surface the
    // explanation instead of handing the agent loop a blank reply.
    if stop_reason == Some("refusal") && text.trim().is_empty() {
        let explanation = body
            .get("stop_details")
            .and_then(|d| d.get("explanation"))
            .and_then(|e| e.as_str())
            .unwrap_or("the request was declined by the model's safety system");
        text = format!("I can't help with that ({explanation}).");
    }

    let usage = body.get("usage").map(|u| {
        let read = |k: &str| u.get(k).and_then(|v| v.as_u64()).unwrap_or(0) as u32;
        // Cache reads/writes are still input the model saw — fold them in so
        // token accounting stays meaningful.
        let prompt =
            read("input_tokens") + read("cache_read_input_tokens") + read("cache_creation_input_tokens");
        let completion = read("output_tokens");
        UsageInfo {
            prompt_tokens: prompt,
            completion_tokens: completion,
            total_tokens: prompt + completion,
        }
    });

    LlmResponse {
        content: if text.is_empty() { None } else { Some(text) },
        tool_calls,
        finish_reason,
        usage,
        reasoning_content: if reasoning.is_empty() {
            None
        } else {
            Some(reasoning)
        },
    }
}

#[async_trait]
impl LlmProvider for AnthropicProvider {
    async fn chat(
        &self,
        messages: &[Message],
        tools: Option<&[ToolDefinition]>,
        model: &str,
        config: &LlmRequestConfig,
    ) -> LlmResponse {
        let (system, converted) = convert_messages(messages);

        let mut request = json!({
            "model": model,
            "max_tokens": config.max_tokens,
            "messages": converted,
            // Automatic prefix caching: the agent loop re-sends history and a
            // large tool list every turn, which is exactly what this is for.
            "cache_control": {"type": "ephemeral"},
        });
        if !system.is_empty() {
            request["system"] = json!(system);
        }
        if let Some(tools) = tools {
            if !tools.is_empty() {
                request["tools"] = json!(convert_tools(tools));
            }
        }
        // Deliberately no `temperature`/`top_p` (rejected by current models)
        // and no `thinking` (adaptive by default).

        debug!(model = model, endpoint = %self.endpoint(), "anthropic request");

        let resp = self
            .http
            .post(self.endpoint())
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", API_VERSION)
            .header("content-type", "application/json")
            .json(&request)
            .send()
            .await;

        let resp = match resp {
            Ok(r) => r,
            Err(e) => return LlmResponse::error(format!("Anthropic request failed: {e}")),
        };
        let status = resp.status();
        let body_text = resp.text().await.unwrap_or_default();

        if !status.is_success() {
            warn!(status = %status, "anthropic API error");
            let detail: String = serde_json::from_str::<Value>(&body_text)
                .ok()
                .and_then(|v| {
                    v.get("error")
                        .and_then(|e| e.get("message"))
                        .and_then(|m| m.as_str())
                        .map(str::to_string)
                })
                .unwrap_or_else(|| body_text.chars().take(300).collect());
            return LlmResponse::error(format!("Anthropic API error ({status}): {detail}"));
        }

        match serde_json::from_str::<Value>(&body_text) {
            Ok(v) => parse_response(&v),
            Err(e) => LlmResponse::error(format!("Anthropic returned unparseable JSON: {e}")),
        }
    }

    fn default_model(&self) -> &str {
        &self.model
    }

    fn display_name(&self) -> &str {
        "Anthropic"
    }
}

// ─────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use metis_core::types::Message;
    use wiremock::matchers::{body_partial_json, header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn make_provider(base: &str) -> AnthropicProvider {
        let config = ProviderConfig {
            api_key: "sk-ant-test".to_string(),
            api_base: Some(base.to_string()),
            extra_headers: None,
        };
        AnthropicProvider::new(&config, "claude-opus-5")
    }

    fn tool_def(name: &str) -> ToolDefinition {
        ToolDefinition::new(name, "a test tool", json!({"type": "object", "properties": {}}))
    }

    // ── message conversion ──

    #[test]
    fn system_messages_fold_into_the_top_level_field() {
        let (system, msgs) = convert_messages(&[
            Message::system("You are Metis."),
            Message::user("hi"),
            Message::system("Ledger: did a thing."),
        ]);
        assert!(system.contains("You are Metis."));
        assert!(system.contains("Ledger: did a thing."));
        assert_eq!(msgs.len(), 1, "system turns must not appear as messages");
        assert_eq!(msgs[0]["role"], "user");
    }

    #[test]
    fn tool_calls_become_tool_use_blocks_and_results_merge_into_one_turn() {
        let (_, msgs) = convert_messages(&[
            Message::user("check two things"),
            Message::assistant_tool_calls(vec![
                ToolCall::new("id1", "read_file", r#"{"path":"a.txt"}"#),
                ToolCall::new("id2", "read_file", r#"{"path":"b.txt"}"#),
            ]),
            Message::tool_result("id1", "contents of a"),
            Message::tool_result("id2", "contents of b"),
        ]);
        assert_eq!(msgs.len(), 3);
        let assistant = &msgs[1];
        assert_eq!(assistant["content"][0]["type"], "tool_use");
        assert_eq!(assistant["content"][0]["input"]["path"], "a.txt");
        // Both results in ONE user message — splitting them trains the model
        // out of parallel tool use.
        let results = &msgs[2];
        assert_eq!(results["role"], "user");
        assert_eq!(results["content"].as_array().unwrap().len(), 2);
        assert_eq!(results["content"][0]["type"], "tool_result");
        assert_eq!(results["content"][1]["tool_use_id"], "id2");
    }

    #[test]
    fn unparseable_tool_arguments_degrade_to_an_empty_object() {
        let (_, msgs) = convert_messages(&[Message::assistant_tool_calls(vec![ToolCall::new(
            "id1",
            "exec",
            "not json at all",
        )])]);
        assert_eq!(msgs[0]["content"][0]["input"], json!({}));
    }

    // ── response parsing ──

    #[test]
    fn parses_text_and_tool_use_blocks() {
        let body = json!({
            "content": [
                {"type": "text", "text": "Let me check."},
                {"type": "tool_use", "id": "toolu_1", "name": "exec", "input": {"command": "dir"}}
            ],
            "stop_reason": "tool_use",
            "usage": {"input_tokens": 100, "output_tokens": 20, "cache_read_input_tokens": 900}
        });
        let r = parse_response(&body);
        assert_eq!(r.content.as_deref(), Some("Let me check."));
        assert_eq!(r.tool_calls.len(), 1);
        assert_eq!(r.tool_calls[0].function.name, "exec");
        assert_eq!(r.finish_reason.as_deref(), Some("tool_calls"));
        let usage = r.usage.unwrap();
        assert_eq!(usage.prompt_tokens, 1000, "cached tokens still count as input");
        assert_eq!(usage.completion_tokens, 20);
    }

    #[test]
    fn a_refusal_becomes_a_readable_reply_not_an_empty_one() {
        let body = json!({
            "content": [],
            "stop_reason": "refusal",
            "stop_details": {"type": "refusal", "category": "cyber", "explanation": "policy"}
        });
        let r = parse_response(&body);
        assert!(r.content.unwrap().contains("policy"));
    }

    // ── wire format ──

    #[tokio::test]
    async fn speaks_the_native_wire_format() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .and(header("x-api-key", "sk-ant-test"))
            .and(header("anthropic-version", API_VERSION))
            // System is a top-level field; caching is on; temperature absent.
            .and(body_partial_json(json!({
                "model": "claude-opus-5",
                "system": "be brief",
                "cache_control": {"type": "ephemeral"}
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "content": [{"type": "text", "text": "ok"}],
                "stop_reason": "end_turn",
                "usage": {"input_tokens": 10, "output_tokens": 1}
            })))
            .expect(1)
            .mount(&server)
            .await;

        let provider = make_provider(&server.uri());
        let response = provider
            .chat(
                &[Message::system("be brief"), Message::user("hi")],
                Some(&[tool_def("exec")]),
                "claude-opus-5",
                &LlmRequestConfig::default(),
            )
            .await;
        assert_eq!(response.content.as_deref(), Some("ok"));
    }

    #[tokio::test]
    async fn temperature_is_never_sent() {
        // Current Claude models return 400 if sampling params are present, so
        // the absence of `temperature` is a correctness requirement.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "content": [{"type": "text", "text": "ok"}],
                "stop_reason": "end_turn"
            })))
            .mount(&server)
            .await;

        let provider = make_provider(&server.uri());
        let mut config = LlmRequestConfig::default();
        config.temperature = 0.9;
        provider
            .chat(&[Message::user("hi")], None, "claude-opus-5", &config)
            .await;

        let requests = server.received_requests().await.unwrap();
        let body: Value = serde_json::from_slice(&requests[0].body).unwrap();
        assert!(body.get("temperature").is_none());
        assert!(body.get("thinking").is_none(), "adaptive is the default; do not configure it");
    }

    #[tokio::test]
    async fn api_errors_come_back_as_error_responses() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(401).set_body_json(json!({
                "error": {"type": "authentication_error", "message": "invalid x-api-key"}
            })))
            .mount(&server)
            .await;

        let provider = make_provider(&server.uri());
        let r = provider
            .chat(&[Message::user("hi")], None, "claude-opus-5", &LlmRequestConfig::default())
            .await;
        let content = r.content.unwrap_or_default();
        assert!(content.contains("invalid x-api-key"), "{content}");
    }
}
