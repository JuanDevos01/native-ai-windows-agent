//! Message tool — lets the agent proactively send messages to channels.
//!
//! Port of nanobot's `agent/tools/message.py`.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::sync::Mutex;
use tracing::debug;

use metis_core::bus::types::OutboundMessage;

use super::base::{optional_string, require_string, Tool};

/// Callback type for sending outbound messages.
pub type SendCallback = Arc<dyn Fn(OutboundMessage) -> Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send>> + Send + Sync>;

// ─────────────────────────────────────────────
// MessageTool
// ─────────────────────────────────────────────

/// Allows the agent to send messages to channels.
///
/// The agent loop calls `set_context` before each interaction to set
/// the default channel/chat_id for the current conversation.
pub struct MessageTool {
    /// How a message actually leaves the process. `None` means no chat
    /// channel is attached here (desktop, CLI, `cron run`) and a send is
    /// impossible — which must be reported as a failure, never as success.
    send_callback: std::sync::Mutex<Option<SendCallback>>,
    /// Default channel / chat_id set per-interaction by the agent loop.
    context: Mutex<(String, String)>,
}

impl MessageTool {
    /// Create a new message tool with a send callback.
    pub fn new(send_callback: Option<SendCallback>) -> Self {
        Self {
            send_callback: std::sync::Mutex::new(send_callback),
            context: Mutex::new(("cli".into(), "direct".into())),
        }
    }

    /// Attach the real delivery path. Only a process that actually
    /// dispatches outbound messages to channels (the gateway) should do this.
    pub fn set_send_callback(&self, cb: SendCallback) {
        *self
            .send_callback
            .lock()
            .expect("message tool callback mutex poisoned") = Some(cb);
    }

    /// Set the current context (called by the agent loop per-message).
    pub async fn set_context(&self, channel: &str, chat_id: &str) {
        let mut ctx = self.context.lock().await;
        *ctx = (channel.to_string(), chat_id.to_string());
    }
}

#[async_trait]
impl Tool for MessageTool {
    fn name(&self) -> &str {
        "message"
    }

    fn description(&self) -> &str {
        "Send a message to a channel. By default sends to the current conversation. \
         Can optionally specify a different channel and chat_id to send to."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "content": {
                    "type": "string",
                    "description": "The message content to send"
                },
                "channel": {
                    "type": "string",
                    "description": "Target channel (optional, defaults to current)"
                },
                "chat_id": {
                    "type": "string",
                    "description": "Target chat ID (optional, defaults to current)"
                }
            },
            "required": ["content"]
        })
    }

    async fn execute(&self, params: HashMap<String, Value>) -> anyhow::Result<String> {
        let content = require_string(&params, "content")?;
        let param_channel = optional_string(&params, "channel");
        let param_chat_id = optional_string(&params, "chat_id");

        let ctx = self.context.lock().await;
        let channel = param_channel.unwrap_or_else(|| ctx.0.clone());
        let chat_id = param_chat_id.unwrap_or_else(|| ctx.1.clone());

        // "cron" is the scheduler's internal channel — nothing consumes it.
        // A send would report success while the message vanished, and the
        // model would then truthfully tell the user it delivered something
        // that never arrived.
        if channel == "cron" {
            anyhow::bail!(
                "This is a scheduled job with no live chat, so there is nowhere to send a \
                 message. If this job was created with --deliver, your final reply IS the \
                 delivered message — put the content in your reply instead of calling this \
                 tool. If it was not, the output is only logged."
            );
        }
        drop(ctx);

        debug!(channel = %channel, chat_id = %chat_id, "sending message via tool");

        let msg = OutboundMessage::new(&channel, &chat_id, &content);

        // Clone the callback out so the std mutex is not held across await.
        let cb = self
            .send_callback
            .lock()
            .expect("message tool callback mutex poisoned")
            .clone();

        // This branch used to discard the message and still return
        // "Message sent to telegram:...". The tool was constructed with no
        // callback in EVERY mode, so it had never delivered anything: a
        // morning news briefing was "sent" to Telegram, the model reported
        // it delivered, and the user received nothing but that report.
        let Some(cb) = cb else {
            anyhow::bail!(
                "NOT SENT: no chat channel is attached to this process (desktop, CLI, or a \
                 manual `cron run`), so a message to {channel}:{chat_id} cannot be delivered. \
                 Put the content in your reply instead. Do not tell the user it was sent."
            );
        };
        cb(msg)
            .await
            .map_err(|e| anyhow::anyhow!("NOT SENT: delivery to {channel}:{chat_id} failed: {e}"))?;

        Ok(format!("Message delivered to {channel}:{chat_id}"))
    }
}

// ─────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// A callback that records what was sent, so tests prove delivery
    /// instead of trusting the return string.
    fn capturing_callback() -> (SendCallback, std::sync::Arc<std::sync::Mutex<Vec<OutboundMessage>>>) {
        let sent = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = sent.clone();
        let cb: SendCallback = Arc::new(move |m: OutboundMessage| {
            let sink = sink.clone();
            Box::pin(async move {
                sink.lock().unwrap().push(m);
                Ok(())
            })
        });
        (cb, sent)
    }

    #[tokio::test]
    async fn with_no_delivery_path_a_send_fails_instead_of_pretending() {
        // The regression that ate every news briefing: no callback, yet the
        // tool said "Message sent". It must now refuse, loudly.
        let tool = MessageTool::new(None);
        tool.set_context("telegram", "8582973375").await;
        let mut params = HashMap::new();
        params.insert("content".into(), json!("five headlines"));
        let err = tool.execute(params).await.unwrap_err().to_string();
        assert!(err.contains("NOT SENT"), "{err}");
        assert!(err.contains("Do not tell the user it was sent"), "{err}");
    }

    #[tokio::test]
    async fn with_a_delivery_path_the_message_really_goes_out() {
        let (cb, sent) = capturing_callback();
        let tool = MessageTool::new(None);
        tool.set_send_callback(cb);
        tool.set_context("telegram", "8582973375").await;
        let mut params = HashMap::new();
        params.insert("content".into(), json!("five headlines"));
        let out = tool.execute(params).await.unwrap();
        assert_eq!(out, "Message delivered to telegram:8582973375");
        let sent = sent.lock().unwrap();
        assert_eq!(sent.len(), 1, "exactly one message must actually leave");
        assert_eq!(sent[0].content, "five headlines");
        assert_eq!(sent[0].chat_id, "8582973375");
    }


    #[tokio::test]
    async fn sending_from_the_scheduler_channel_is_refused_with_guidance() {
        // A send into "cron" reports success while the message vanishes; the
        // model then truthfully tells the user it delivered something that
        // never arrived. Refusing with instructions is the honest failure.
        let tool = MessageTool::new(None);
        tool.set_context("cron", "77008abf").await;
        let mut params = HashMap::new();
        params.insert("content".into(), json!("5 headlines"));
        let err = tool.execute(params).await.unwrap_err().to_string();
        assert!(err.contains("final reply IS the"), "{err}");
    }

    #[tokio::test]
    async fn an_explicit_channel_still_works_from_a_scheduled_session() {
        let (cb, _sent) = capturing_callback();
        let tool = MessageTool::new(Some(cb));
        tool.set_context("cron", "77008abf").await;
        let mut params = HashMap::new();
        params.insert("content".into(), json!("hello"));
        params.insert("channel".into(), json!("telegram"));
        params.insert("chat_id".into(), json!("8582973375"));
        assert!(tool.execute(params).await.is_ok());
    }

    #[test]
    fn test_tool_definition() {
        let tool = MessageTool::new(None);
        let def = tool.to_definition();
        assert_eq!(def.function.name, "message");
        assert_eq!(def.tool_type, "function");
    }

    #[tokio::test]
    async fn test_set_context() {
        let tool = MessageTool::new(None);
        tool.set_context("telegram", "chat_42").await;
        let ctx = tool.context.lock().await;
        assert_eq!(ctx.0, "telegram");
        assert_eq!(ctx.1, "chat_42");
    }

    #[tokio::test]
    async fn test_execute_no_callback() {
        let (cb, _sent) = capturing_callback();
        let tool = MessageTool::new(Some(cb));
        tool.set_context("discord", "guild_1").await;
        let mut params = HashMap::new();
        params.insert("content".into(), json!("Hello from agent"));
        let result = tool.execute(params).await.unwrap();
        assert_eq!(result, "Message delivered to discord:guild_1");
    }

    #[tokio::test]
    async fn test_execute_with_channel_override() {
        let (cb, _sent) = capturing_callback();
        let tool = MessageTool::new(Some(cb));
        tool.set_context("cli", "direct").await;
        let mut params = HashMap::new();
        params.insert("content".into(), json!("Hello"));
        params.insert("channel".into(), json!("slack"));
        params.insert("chat_id".into(), json!("C12345"));
        let result = tool.execute(params).await.unwrap();
        assert_eq!(result, "Message delivered to slack:C12345");
    }

    #[tokio::test]
    async fn test_execute_missing_content() {
        let tool = MessageTool::new(None);
        let params = HashMap::new();
        let result = tool.execute(params).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_execute_with_callback() {
        use std::sync::atomic::{AtomicBool, Ordering};

        let called = Arc::new(AtomicBool::new(false));
        let called_clone = called.clone();

        let callback: SendCallback = Arc::new(move |_msg| {
            let called = called_clone.clone();
            Box::pin(async move {
                called.store(true, Ordering::SeqCst);
                Ok(())
            })
        });

        let tool = MessageTool::new(Some(callback));
        let mut params = HashMap::new();
        params.insert("content".into(), json!("ping"));
        let result = tool.execute(params).await.unwrap();
        assert!(result.contains("Message delivered"));
        assert!(called.load(Ordering::SeqCst));
    }
}
