//! Delegate tool — hand a task to another named agent and get its answer.
//!
//! Each roster agent is a complete AgentLoop with its own model, so a
//! delegated task gets the same safeguards as a direct one: the tool log,
//! the fact check against real tool output, forged-output stripping, typed
//! events. Its tool calls are recorded in the PARENT's session and turn, so
//! `metis events` shows the hand-off, the other agent's work, and the
//! answer as one conversation.
//!
//! Every delegation starts from a fresh history. A delegated agent that
//! carried its previous tasks around would be exposed to exactly the
//! self-copying that made the main agent repeat its own old answers.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::sync::Mutex;
use tracing::{info, warn};

use metis_core::events::{Event, EventKind, EventLog};

use super::base::{require_string, Tool};
use crate::agent_loop::AgentLoop;

/// How long one delegated task may run. A coder running several commands
/// can legitimately take minutes; past this the task is abandoned and the
/// delegating agent is told so, rather than waiting forever.
const DELEGATION_TIMEOUT: Duration = Duration::from_secs(600);

/// One agent that can be delegated to.
pub struct RosterMember {
    pub name: String,
    pub job: String,
    pub model: String,
    pub agent: Arc<AgentLoop>,
}

pub struct DelegateTool {
    members: BTreeMap<String, RosterMember>,
    /// Name of the agent that owns this tool (the delegator).
    owner: String,
    events: Option<Arc<EventLog>>,
    /// (session, turn) of the turn currently being handled, set per message.
    context: Mutex<(String, String)>,
    /// Per-turn counter so each delegation gets its own fresh session.
    seq: std::sync::atomic::AtomicU32,
    /// Cached tool description, which lists the roster.
    description: String,
}

impl DelegateTool {
    pub fn new(owner: &str, members: Vec<RosterMember>, events: Option<Arc<EventLog>>) -> Self {
        let members: BTreeMap<String, RosterMember> =
            members.into_iter().map(|m| (m.name.clone(), m)).collect();
        let mut description = String::from(
            "Hand a task to another agent and wait for its answer. Each agent has its own model \
             and job. Use this when a task matches an agent's job below; give it a complete, \
             self-contained task (it does not see this conversation). When the user says which \
             agent should do a step, delegate THAT step to THAT agent - do not do it yourself. \
             You remain responsible for the answer: check what it reports before telling the \
             user.\n\nAgents:\n",
        );
        for m in members.values() {
            description.push_str(&format!("- {} ({}): {}\n", m.name, m.model, m.job));
        }
        Self {
            members,
            owner: owner.to_string(),
            events,
            context: Mutex::new((String::new(), String::new())),
            seq: std::sync::atomic::AtomicU32::new(0),
            description,
        }
    }

    pub fn agent_names(&self) -> Vec<String> {
        self.members.keys().cloned().collect()
    }

    /// Called by the owning agent loop before each turn.
    pub async fn set_context(&self, session: &str, turn: &str) {
        *self.context.lock().await = (session.to_string(), turn.to_string());
    }

    fn record(&self, session: &str, turn: &str, event: EventKind) {
        if let Some(log) = &self.events {
            if let Err(e) = log.append(&Event::new(session, turn, &self.owner, event)) {
                warn!(error = %e, "failed to write delegation event");
            }
        }
    }
}

#[async_trait]
impl Tool for DelegateTool {
    fn name(&self) -> &str {
        "delegate"
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn parameters(&self) -> Value {
        let names: Vec<&str> = self.members.keys().map(String::as_str).collect();
        json!({
            "type": "object",
            "properties": {
                "agent": {
                    "type": "string",
                    "enum": names,
                    "description": "Which agent to hand the task to"
                },
                "task": {
                    "type": "string",
                    "description": "The complete task, including every detail the agent needs - it cannot see this conversation"
                }
            },
            "required": ["agent", "task"]
        })
    }

    async fn execute(&self, params: HashMap<String, Value>) -> anyhow::Result<String> {
        let name = require_string(&params, "agent")?;
        let task = require_string(&params, "task")?;
        let Some(member) = self.members.get(name.trim()) else {
            anyhow::bail!(
                "No agent named \"{name}\". Available: {}.",
                self.agent_names().join(", ")
            );
        };
        if task.trim().is_empty() {
            anyhow::bail!("Give the agent a task.");
        }

        // Supplied per call by the owning loop (race-free); the shared context
        // is only a fallback for callers that execute the tool directly.
        let (ctx_session, ctx_turn) = self.context.lock().await.clone();
        let from_call = |k: &str| {
            params
                .get(k)
                .and_then(|v| v.as_str())
                .map(str::to_string)
                .filter(|s| !s.is_empty())
        };
        let session = from_call("__event_session").unwrap_or(ctx_session);
        let turn = from_call("__event_turn").unwrap_or(ctx_turn);
        let n = self
            .seq
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        // A fresh conversation for every delegation.
        let delegation_id = format!("{}-{}-{n}", member.name, if turn.is_empty() { "t" } else { &turn });

        self.record(
            &session,
            &turn,
            EventKind::Delegation {
                to_agent: member.name.clone(),
                task: task.clone(),
            },
        );
        info!(from = %self.owner, to = %member.name, "delegating task");

        let run = member
            .agent
            .run_delegated(&task, &session, &turn, &delegation_id);
        let answer = match tokio::time::timeout(DELEGATION_TIMEOUT, run).await {
            Ok(Ok(text)) => text,
            Ok(Err(e)) => {
                let msg = format!("Agent {} failed: {e}", member.name);
                self.record(
                    &session,
                    &turn,
                    EventKind::DelegationResult {
                        from_agent: member.name.clone(),
                        text: msg.clone(),
                    },
                );
                anyhow::bail!("{msg}");
            }
            Err(_) => {
                let msg = format!(
                    "Agent {} did not finish within {} minutes and was stopped. Nothing it was \
                     doing can be assumed complete.",
                    member.name,
                    DELEGATION_TIMEOUT.as_secs() / 60
                );
                self.record(
                    &session,
                    &turn,
                    EventKind::DelegationResult {
                        from_agent: member.name.clone(),
                        text: msg.clone(),
                    },
                );
                anyhow::bail!("{msg}");
            }
        };

        self.record(
            &session,
            &turn,
            EventKind::DelegationResult {
                from_agent: member.name.clone(),
                text: answer.clone(),
            },
        );

        Ok(format!(
            "Answer from {} ({}):\n\n{answer}\n\n[Check this before relying on it. Anything \
             marked \"Unverified\" was not backed by a tool output.]",
            member.name, member.model
        ))
    }
}
