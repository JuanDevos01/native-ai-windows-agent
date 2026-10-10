//! How a person is asked to approve a destructive change, per front end.
//!
//! - Terminal (`metis agent`): a prompt on the console. No console - output
//!   piped, run from a script - means nobody can answer, so the answer is no.
//! - Gateway: a Telegram conversation gets the question as a message and the
//!   reply 1 / 2 / 3. Every other session (scheduled jobs, email, WhatsApp)
//!   is refused: nobody is there to ask, and a 06:00 job must never decide to
//!   overwrite data on its own.

use std::io::{IsTerminal, Write};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;

use metis_agent::approval::{ApprovalDecision, ApprovalHandler, ApprovalRequest};
use metis_core::bus::queue::MessageBus;
use metis_core::bus::types::OutboundMessage;

/// How long a chat request waits for an answer before it is refused.
const CHAT_ANSWER_TIMEOUT: Duration = Duration::from_secs(600);

/// Read a person's reply. Anything not clearly a yes is a no.
pub fn parse_answer(text: &str) -> ApprovalDecision {
    let t = text.trim().to_lowercase();
    match t.as_str() {
        "1" | "y" | "yes" | "ok" | "allow" | "allow once" | "once" | "si" | "sí" | "ja" => {
            ApprovalDecision::AllowOnce
        }
        "2" | "a" | "always" | "session" | "allow session" | "allow for this session" => {
            ApprovalDecision::AllowSession
        }
        _ => ApprovalDecision::Deny,
    }
}

fn render(req: &ApprovalRequest) -> String {
    let mut out = format!("⚠ Approval needed\n\n{}\n\nThis will change:\n", req.title);
    for c in &req.changes {
        out.push_str(&format!("• {c}\n"));
    }
    out
}

/// Asks on the console.
pub struct TerminalApprovalHandler;

#[async_trait]
impl ApprovalHandler for TerminalApprovalHandler {
    async fn ask(&self, req: ApprovalRequest) -> ApprovalDecision {
        if !std::io::stdin().is_terminal() {
            return ApprovalDecision::Deny;
        }
        let prompt = format!(
            "\n{}\n[1] allow once   [2] allow for this session   [3] deny\n> ",
            render(&req)
        );
        tokio::task::spawn_blocking(move || {
            eprint!("{prompt}");
            let _ = std::io::stderr().flush();
            let mut line = String::new();
            match std::io::stdin().read_line(&mut line) {
                Ok(_) => parse_answer(&line),
                Err(_) => ApprovalDecision::Deny,
            }
        })
        .await
        .unwrap_or(ApprovalDecision::Deny)
    }
}

/// Asks in the conversation the request came from, where that is possible.
pub struct GatewayApprovalHandler {
    bus: Arc<MessageBus>,
}

impl GatewayApprovalHandler {
    pub fn new(bus: Arc<MessageBus>) -> Self {
        Self { bus }
    }
}

#[async_trait]
impl ApprovalHandler for GatewayApprovalHandler {
    async fn ask(&self, req: ApprovalRequest) -> ApprovalDecision {
        let Some(chat_id) = req.session.strip_prefix("telegram:") else {
            tracing::warn!(session = %req.session, title = %req.title,
                "approval refused: this session has nobody to ask");
            return ApprovalDecision::Deny;
        };
        let key = format!("telegram:{chat_id}");
        let answer = metis_core::live_answers::expect_answer(&key);
        let text = format!(
            "{}\nReply:\n1 = allow once\n2 = allow for this conversation\n3 = deny\n\n(No answer within 10 minutes counts as deny.)",
            render(&req)
        );
        if self
            .bus
            .publish_outbound(OutboundMessage::new("telegram", chat_id, &text))
            .await
            .is_err()
        {
            metis_core::live_answers::cancel(&key);
            return ApprovalDecision::Deny;
        }
        match tokio::time::timeout(CHAT_ANSWER_TIMEOUT, answer).await {
            Ok(Ok(reply)) => {
                let d = parse_answer(&reply);
                let note = match d {
                    ApprovalDecision::Deny => "⛔ Denied - nothing was changed.",
                    ApprovalDecision::AllowOnce => "✅ Allowed once - continuing.",
                    ApprovalDecision::AllowSession => "🔓 Allowed for this conversation - continuing.",
                };
                let _ = self
                    .bus
                    .publish_outbound(OutboundMessage::new("telegram", chat_id, note))
                    .await;
                d
            }
            _ => {
                metis_core::live_answers::cancel(&key);
                ApprovalDecision::Deny
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use metis_agent::approval::ChangeKind;

    #[test]
    fn only_a_clear_yes_allows() {
        assert_eq!(parse_answer("1"), ApprovalDecision::AllowOnce);
        assert_eq!(parse_answer(" Yes "), ApprovalDecision::AllowOnce);
        assert_eq!(parse_answer("2"), ApprovalDecision::AllowSession);
        assert_eq!(parse_answer("3"), ApprovalDecision::Deny);
        assert_eq!(parse_answer("hmm, what does it do?"), ApprovalDecision::Deny);
        assert_eq!(parse_answer(""), ApprovalDecision::Deny);
    }

    fn req(session: &str) -> ApprovalRequest {
        ApprovalRequest {
            session: session.into(),
            kind: ChangeKind::Overwrite,
            title: "Run a command that may overwrite 1 existing file".into(),
            changes: vec!["C:\\x\\blf2.nam (6,813,796 bytes)".into()],
            detail: "python x.py --output blf2.nam".into(),
        }
    }

    #[tokio::test]
    async fn a_scheduled_job_is_never_approved() {
        let h = GatewayApprovalHandler::new(Arc::new(MessageBus::new(4)));
        assert_eq!(h.ask(req("cron:d88966e6-20261009")).await, ApprovalDecision::Deny);
        assert_eq!(h.ask(req("email:someone")).await, ApprovalDecision::Deny);
    }

    #[tokio::test]
    async fn a_telegram_reply_answers_the_waiting_request() {
        let bus = Arc::new(MessageBus::new(8));
        let h = GatewayApprovalHandler::new(bus.clone());
        let ask = tokio::spawn(async move { h.ask(req("telegram:42")).await });
        // The question goes to the chat, listing what would change...
        let q = bus.consume_outbound().await.expect("question sent");
        assert_eq!(q.chat_id, "42");
        assert!(q.content.contains("blf2.nam"), "{}", q.content);
        // ...and the person's "2" - caught by the channel - answers it.
        let mut delivered = false;
        for _ in 0..50 {
            if metis_core::live_answers::try_answer("telegram:42", "2") {
                delivered = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(delivered, "the waiting request must accept the reply");
        assert_eq!(ask.await.unwrap(), ApprovalDecision::AllowSession);
    }
}
