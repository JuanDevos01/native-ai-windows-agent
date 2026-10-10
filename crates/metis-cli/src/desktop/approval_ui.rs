//! Asking the person, in the desktop, before a destructive change.
//!
//! The agent runs on a background runtime; the window runs on the UI thread.
//! A request crosses over a channel together with a one-shot reply slot, the
//! window shows it as a modal, and the person's button press travels back.
//! Unanswered requests are refused after a timeout, so an agent can never
//! sit forever on a half-finished change - and never proceeds without a yes.

use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::{mpsc, oneshot};

use metis_agent::approval::{ApprovalDecision, ApprovalHandler, ApprovalRequest};

/// A request plus where to send the answer.
pub type ApprovalAsk = (ApprovalRequest, oneshot::Sender<ApprovalDecision>);

/// How long a request waits for the person before it is refused.
const ANSWER_TIMEOUT: Duration = Duration::from_secs(600);

pub struct DesktopApprovalHandler {
    tx: mpsc::UnboundedSender<ApprovalAsk>,
}

impl DesktopApprovalHandler {
    pub fn new(tx: mpsc::UnboundedSender<ApprovalAsk>) -> Self {
        Self { tx }
    }
}

#[async_trait]
impl ApprovalHandler for DesktopApprovalHandler {
    async fn ask(&self, req: ApprovalRequest) -> ApprovalDecision {
        let (reply_tx, reply_rx) = oneshot::channel();
        if self.tx.send((req, reply_tx)).is_err() {
            // The window is gone; nobody can say yes.
            return ApprovalDecision::Deny;
        }
        match tokio::time::timeout(ANSWER_TIMEOUT, reply_rx).await {
            Ok(Ok(decision)) => decision,
            // Closed without an answer, or timed out: the safe answer is no.
            _ => ApprovalDecision::Deny,
        }
    }
}

/// Draw the approval dialog. Returns the person's decision once they click.
pub fn draw_approval_window(ctx: &egui::Context, req: &ApprovalRequest) -> Option<ApprovalDecision> {
    let mut decision = None;
    // Dim everything behind it so the request cannot be missed.
    egui::Area::new(egui::Id::new("approval_backdrop"))
        .fixed_pos(egui::pos2(0.0, 0.0))
        .order(egui::Order::Background)
        .show(ctx, |ui| {
            let screen = ctx.screen_rect();
            ui.painter()
                .rect_filled(screen, 0.0, egui::Color32::from_black_alpha(140));
        });

    egui::Window::new("⚠  Approval needed")
        .id(egui::Id::new("tool_approval_window"))
        .collapsible(false)
        .resizable(true)
        .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
        .default_width(620.0)
        .order(egui::Order::Foreground)
        .show(ctx, |ui| {
            ui.label(egui::RichText::new(&req.title).strong().size(16.0));
            ui.add_space(8.0);
            ui.label("This will change:");
            for change in &req.changes {
                ui.horizontal_wrapped(|ui| {
                    ui.label("•");
                    ui.label(egui::RichText::new(change).monospace());
                });
            }
            ui.add_space(6.0);
            egui::CollapsingHeader::new("Exact command / content")
                .id_salt("approval_detail")
                .show(ui, |ui| {
                    egui::ScrollArea::vertical().max_height(180.0).show(ui, |ui| {
                        ui.label(egui::RichText::new(&req.detail).monospace().small());
                    });
                });
            ui.add_space(10.0);
            ui.label(
                egui::RichText::new(format!(
                    "\"Allow for this session\" also allows further requests to {} in this \
                     conversation without asking again.",
                    req.kind.label()
                ))
                .small()
                .weak(),
            );
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                if ui.button("✅  Allow once").clicked() {
                    decision = Some(ApprovalDecision::AllowOnce);
                }
                if ui.button("🔓  Allow for this session").clicked() {
                    decision = Some(ApprovalDecision::AllowSession);
                }
                ui.add_space(24.0);
                if ui
                    .button(egui::RichText::new("⛔  Deny").strong())
                    .clicked()
                {
                    decision = Some(ApprovalDecision::Deny);
                }
            });
        });
    decision
}

#[cfg(test)]
mod tests {
    use super::*;
    use metis_agent::approval::ChangeKind;

    fn req() -> ApprovalRequest {
        ApprovalRequest {
            session: "desktop:x".into(),
            kind: ChangeKind::Overwrite,
            title: "Run a command that may overwrite 1 existing file".into(),
            changes: vec!["C:\\x\\blf2.nam (6,813,796 bytes) - is given as this command's OUTPUT".into()],
            detail: "python scan_to_blf2.py --output C:\\x\\blf2.nam".into(),
        }
    }

    #[tokio::test]
    async fn the_answer_travels_back_to_the_agent() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let handler = DesktopApprovalHandler::new(tx);
        let ask = tokio::spawn(async move { handler.ask(req()).await });
        let (got, reply) = rx.recv().await.expect("request reaches the window");
        assert!(got.changes[0].contains("blf2.nam"));
        reply.send(ApprovalDecision::AllowOnce).unwrap();
        assert_eq!(ask.await.unwrap(), ApprovalDecision::AllowOnce);
    }

    #[tokio::test]
    async fn a_closed_window_means_no() {
        let (tx, rx) = mpsc::unbounded_channel();
        drop(rx);
        let handler = DesktopApprovalHandler::new(tx);
        assert_eq!(handler.ask(req()).await, ApprovalDecision::Deny);
    }

    #[tokio::test]
    async fn dismissing_without_an_answer_means_no() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let handler = DesktopApprovalHandler::new(tx);
        let ask = tokio::spawn(async move { handler.ask(req()).await });
        let (_req, reply) = rx.recv().await.unwrap();
        drop(reply);
        assert_eq!(ask.await.unwrap(), ApprovalDecision::Deny);
    }
}
