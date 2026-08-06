//! Metis Agent — core loop, tools, and context builder.
//!
//! This crate contains:
//! - **tools**: Tool trait, registry, and built-in tools (filesystem, shell, web, message)
//! - **context**: System prompt and message list construction
//! - **agent_loop**: The LLM ↔ tool-calling main loop

pub mod tools;
pub mod context;
pub mod memory;
pub mod memory_index;
pub mod skills;
pub mod subagent;
pub mod agent_loop;

pub use agent_loop::{AgentLoop, ExecToolConfig, MemorySettings, OutboundFormatting, THINKING_LOG_TARGET};
pub use context::ContextBuilder;
pub use memory::MemoryStore;
pub use memory_index::MemoryIndex;
pub use skills::SkillsLoader;
pub use subagent::SubagentManager;
pub use tools::{Tool, ToolRegistry};
