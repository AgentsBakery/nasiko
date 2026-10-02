//! Agent-independent representation of one completed coding-agent session.

use chrono::{DateTime, Utc};
use nasiko_types::{
    CodingAgentTimestampQuality, CodingAgentToolAssociation, CodingAgentToolCallStatus,
};
use serde_json::Value;

#[derive(Debug, Clone)]
pub struct LlmCall {
    pub uuid: String,
    pub provider: String,
    pub model: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_creation_tokens: u64,
    pub accounting: Option<nasiko_types::CodingAgentCallAccounting>,
    pub started_at: DateTime<Utc>,
    pub ended_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct Turn {
    pub uuid: String,
    pub prompt: String,
    pub response: Option<String>,
    pub started_at: DateTime<Utc>,
    pub ended_at: DateTime<Utc>,
    pub calls: Vec<LlmCall>,
    pub tool_calls: Vec<ToolCall>,
}

#[derive(Debug, Clone)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub kind: String,
    pub model_call_id: Option<String>,
    pub status: CodingAgentToolCallStatus,
    pub arguments: Option<Value>,
    pub output: Option<Value>,
    pub raw: Option<String>,
    pub error: Option<String>,
    pub started_at: Option<DateTime<Utc>>,
    pub ended_at: Option<DateTime<Utc>>,
    pub duration_ms: Option<u64>,
    pub association: CodingAgentToolAssociation,
    pub timestamp_quality: CodingAgentTimestampQuality,
}

impl Turn {
    pub fn is_empty(&self) -> bool {
        self.calls.is_empty()
    }
}

#[derive(Debug)]
pub struct SessionSnapshot {
    pub session_id: String,
    pub title: Option<String>,
    pub turns: Vec<Turn>,
    /// Receipt marker for adapter-specific accounting (`source.adapter_version`).
    /// `None` keeps the v1.0 meaning of the adapter's token fields.
    pub adapter_version: Option<u32>,
    /// Turns run by a subagent or teammate, reported with `turn.agent_scope`.
    /// Empty until subagent capture lands; unscoped turns stay in `turns`.
    #[allow(dead_code, reason = "emitted by Claude subagent capture (plan 04-06)")]
    pub scoped_turns: Vec<ScopedTurn>,
}

/// A turn attributed to a non-root agent inside the session.
#[derive(Debug, Clone)]
#[allow(
    dead_code,
    reason = "filled and read by Claude subagent capture (plan 04-06)"
)]
pub struct ScopedTurn {
    pub turn: Turn,
    pub scope: nasiko_types::CodingAgentScope,
}

/// Which capability-gated shapes an adapter may produce for this report.
/// Built from the destination's cached capabilities; the default is the v1.0
/// shape every server accepts.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SnapshotOptions {
    /// Report Codex fresh input exclusive of cached input, with the marker.
    pub codex_exclusive_input: bool,
    /// Capture Claude subagent/teammate turns as scoped turns.
    #[allow(dead_code, reason = "read by Claude subagent capture (plan 04-06)")]
    pub capture_subagents: bool,
}
