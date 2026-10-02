//! Versioned, transport-independent coding-agent telemetry events.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub const CODING_AGENT_EVENT_VERSION: u32 = 1;
pub const CODING_AGENT_BATCH_MAX_EVENTS: usize = 100;
pub const CODING_AGENT_ID_MAX_BYTES: usize = 512;
pub const CODING_AGENT_NAME_MAX_BYTES: usize = 256;
/// Maximum UTF-8 byte length of a content-captured external session title.
pub const CODING_AGENT_SESSION_TITLE_MAX_BYTES: usize = 512;
pub const CODING_AGENT_CONTENT_MAX_BYTES: usize = 1_048_576;
pub const CODING_AGENT_LLM_CALLS_MAX: usize = 1_000;
pub const CODING_AGENT_TOOL_CALLS_MAX: usize = 2_000;
/// Maximum UTF-8 byte length of a content-captured agent-scope description (task intent).
pub const CODING_AGENT_SCOPE_DESCRIPTION_MAX_BYTES: usize = 1024;
/// Capability slug: the server accepts `turn.agent_scope` (scoped subagent/teammate turns).
pub const CODING_AGENT_FEATURE_AGENT_SCOPE: &str = "agent_scope";
/// Capability slug: the server stores `source.adapter_version` receipt markers.
pub const CODING_AGENT_FEATURE_ADAPTER_VERSION: &str = "adapter_version";
/// Every additive v1 feature this build understands, advertised by the capabilities endpoint.
/// A CLI sends a new shape only when the destination server lists its slug here.
pub const CODING_AGENT_TELEMETRY_FEATURES: &[&str] = &[
    CODING_AGENT_FEATURE_AGENT_SCOPE,
    CODING_AGENT_FEATURE_ADAPTER_VERSION,
];
/// Codex receipts carrying this marker report `input_tokens` exclusive of cached input.
/// Receipts without a marker use the legacy inclusive numbers and are corrected at read time.
pub const CODEX_ADAPTER_VERSION_EXCLUSIVE_INPUT: u32 = 1;
/// Claude receipts carrying this marker come from a CLI that captures subagent transcripts.
pub const CLAUDE_ADAPTER_VERSION_SUBAGENTS: u32 = 1;
/// Reserved: set only by a CLI that also captures in-process teammates. Implies subagent
/// capture, because markers are ordered (`2 >= CLAUDE_ADAPTER_VERSION_SUBAGENTS`).
pub const CLAUDE_ADAPTER_VERSION_TEAMMATES: u32 = 2;
const EVENT_NAMESPACE: Uuid = Uuid::from_u128(0xe8c6fef8_5fe4_4dc2_9b48_47466355ced7);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapturePolicy {
    MetadataOnly,
    Content,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CodingAgentEventStatus {
    Accepted,
    Duplicate,
    Rejected,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodingAgentEventBatchRequest {
    pub events: Vec<CodingAgentEventV1>,
}

impl CodingAgentEventBatchRequest {
    pub fn validate(&self) -> Result<(), String> {
        if self.events.is_empty() {
            return Err("events must not be empty".into());
        }
        if self.events.len() > CODING_AGENT_BATCH_MAX_EVENTS {
            return Err(format!(
                "events must contain at most {CODING_AGENT_BATCH_MAX_EVENTS} items"
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodingAgentEventResult {
    pub event_id: String,
    pub status: CodingAgentEventStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodingAgentEventBatchResponse {
    pub results: Vec<CodingAgentEventResult>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodingAgentSource {
    pub agent_id: String,
    pub agent_name: String,
    /// Adapter semantics marker. Absent means the legacy adapter semantics of the
    /// original v1.0 CLI; it is not part of the event identity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub adapter_version: Option<u32>,
}

/// Feature negotiation payload served by `GET /api/telemetry/coding-agent/capabilities`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodingAgentCapabilities {
    /// The event version the server validates; stays `CODING_AGENT_EVENT_VERSION`.
    pub event_version: u32,
    /// Additive feature slugs the server accepts (see `CODING_AGENT_TELEMETRY_FEATURES`).
    pub features: Vec<String>,
}

/// Which agent inside a coding-agent session produced a turn.
/// A turn without a scope is main-agent work.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodingAgentScope {
    pub kind: CodingAgentScopeKind,
    /// Native agent id (Claude `agentId`), bounded by `CODING_AGENT_ID_MAX_BYTES`.
    pub agent_id: String,
    /// Subagent or teammate type (`Explore`, `general-purpose`, a custom name). Metadata.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_type: Option<String>,
    /// Native tool-call id of the Agent/Task call that spawned this agent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_tool_call_id: Option<String>,
    /// For nested agents, the spawning agent's id. Absent means spawned by the main agent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_agent_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spawn_depth: Option<u32>,
    /// Task description or teammate assignment. Content: rejected under metadata-only capture.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Teammate display name. Treated as content: rejected under metadata-only capture.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

/// Kind of a scoped agent. There is deliberately no `Main` variant: an absent scope
/// already means the main agent, and two encodings of one fact would diverge.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CodingAgentScopeKind {
    Subagent,
    Teammate,
    /// Any kind a newer CLI sends that this build does not know.
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodingAgentSession {
    /// Stable, source-scoped session identity used by Nasiko.
    pub id: String,
    /// Session identity supplied by the coding agent.
    pub source_id: String,
    /// External session title; only allowed with content capture and preserved verbatim.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodingAgentLlmCall {
    pub id: String,
    pub provider: String,
    pub model: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_creation_tokens: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accounting: Option<CodingAgentCallAccounting>,
    pub started_at: DateTime<Utc>,
    pub ended_at: DateTime<Utc>,
}

/// Evidence retained from the provider response, independent of transcript record IDs.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodingAgentCallAccounting {
    pub version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_creation_5m_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_creation_1h_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub speed: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service_tier: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inference_geo: Option<String>,
    #[serde(default)]
    pub conflicting_observations: bool,
    /// `Some(false)` when the transcript only held the start-of-stream usage snapshot,
    /// so `output_tokens` is a lower bound. Absent means unknown (legacy semantics).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_tokens_final: Option<bool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CodingAgentToolCallStatus {
    Pending,
    Running,
    Succeeded,
    Failed,
    Denied,
    TimedOut,
    Cancelled,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CodingAgentToolAssociation {
    Exact,
    Turn,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CodingAgentTimestampQuality {
    Exact,
    Inferred,
    Receipt,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodingAgentToolCall {
    pub id: String,
    pub name: String,
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_call_id: Option<String>,
    pub status: CodingAgentToolCallStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub arguments: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub raw: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub started_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    pub association: CodingAgentToolAssociation,
    pub timestamp_quality: CodingAgentTimestampQuality,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodingAgentTurn {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response: Option<String>,
    pub started_at: DateTime<Utc>,
    pub ended_at: DateTime<Utc>,
    pub llm_calls: Vec<CodingAgentLlmCall>,
    #[serde(default)]
    pub tool_calls: Vec<CodingAgentToolCall>,
    /// Which agent did this work. Absent means the main agent, which is how every
    /// event from an older CLI is read; nothing is reclassified.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_scope: Option<CodingAgentScope>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodingAgentEventV1 {
    pub version: u32,
    pub event_id: String,
    pub captured_at: DateTime<Utc>,
    pub source: CodingAgentSource,
    pub session: CodingAgentSession,
    pub turn: CodingAgentTurn,
    pub capture_policy: CapturePolicy,
}

impl CodingAgentEventV1 {
    pub fn validate(&self) -> Result<(), String> {
        if self.version != CODING_AGENT_EVENT_VERSION {
            return Err(format!(
                "unsupported coding-agent event version {}",
                self.version
            ));
        }
        for (name, value) in [
            ("event_id", self.event_id.as_str()),
            ("source.agent_id", self.source.agent_id.as_str()),
            ("source.agent_name", self.source.agent_name.as_str()),
            ("session.id", self.session.id.as_str()),
            ("session.source_id", self.session.source_id.as_str()),
            ("turn.id", self.turn.id.as_str()),
        ] {
            if value.trim().is_empty() {
                return Err(format!("{name} must not be empty"));
            }
            if value.len() > CODING_AGENT_ID_MAX_BYTES {
                return Err(format!(
                    "{name} must be at most {CODING_AGENT_ID_MAX_BYTES} bytes"
                ));
            }
        }
        if self.source.agent_name.len() > CODING_AGENT_NAME_MAX_BYTES {
            return Err(format!(
                "source.agent_name must be at most {CODING_AGENT_NAME_MAX_BYTES} bytes"
            ));
        }
        if let Some(title) = &self.session.title {
            if title.trim().is_empty() {
                return Err("session.title must not be empty".into());
            }
            if title.len() > CODING_AGENT_SESSION_TITLE_MAX_BYTES {
                return Err(format!(
                    "session.title must be at most {CODING_AGENT_SESSION_TITLE_MAX_BYTES} bytes"
                ));
            }
            if self.capture_policy == CapturePolicy::MetadataOnly {
                return Err("metadata-only events must not contain session.title".into());
            }
        }
        for (name, content) in [
            ("turn.prompt", self.turn.prompt.as_deref()),
            ("turn.response", self.turn.response.as_deref()),
        ] {
            if content.is_some_and(|content| content.len() > CODING_AGENT_CONTENT_MAX_BYTES) {
                return Err(format!(
                    "{name} must be at most {CODING_AGENT_CONTENT_MAX_BYTES} bytes"
                ));
            }
        }
        if self.turn.llm_calls.len() > CODING_AGENT_LLM_CALLS_MAX {
            return Err(format!(
                "turn.llm_calls must contain at most {CODING_AGENT_LLM_CALLS_MAX} items"
            ));
        }
        if self.turn.tool_calls.len() > CODING_AGENT_TOOL_CALLS_MAX {
            return Err(format!(
                "turn.tool_calls must contain at most {CODING_AGENT_TOOL_CALLS_MAX} items"
            ));
        }
        let expected = coding_agent_event_id(
            &self.source.agent_id,
            &self.session.source_id,
            &self.turn.id,
        );
        if self.event_id != expected {
            return Err("event_id does not match the stable source/session/turn identity".into());
        }
        if self.session.id
            != coding_agent_session_id(&self.source.agent_id, &self.session.source_id)
        {
            return Err("session.id does not match the stable source session identity".into());
        }
        if self.capture_policy == CapturePolicy::MetadataOnly
            && (self.turn.prompt.is_some() || self.turn.response.is_some())
        {
            return Err("metadata-only events must not contain prompt or response content".into());
        }
        if let Some(scope) = &self.turn.agent_scope {
            self.validate_agent_scope(scope)?;
        } else if self.capture_policy == CapturePolicy::Content
            && (self
                .turn
                .prompt
                .as_deref()
                .is_none_or(|content| content.trim().is_empty())
                || self
                    .turn
                    .response
                    .as_deref()
                    .is_none_or(|content| content.trim().is_empty()))
        {
            return Err("content events must contain nonempty prompt and response content".into());
        }
        if self.turn.ended_at < self.turn.started_at {
            return Err("turn ended_at precedes started_at".into());
        }
        let mut llm_ids = std::collections::HashSet::new();
        for call in &self.turn.llm_calls {
            for (name, value) in [
                ("turn.llm_calls[].id", call.id.as_str()),
                ("turn.llm_calls[].provider", call.provider.as_str()),
                ("turn.llm_calls[].model", call.model.as_str()),
            ] {
                if value.trim().is_empty() {
                    return Err(format!("{name} must not be empty"));
                }
                let max = if name == "turn.llm_calls[].id" {
                    CODING_AGENT_ID_MAX_BYTES
                } else {
                    CODING_AGENT_NAME_MAX_BYTES
                };
                if value.len() > max {
                    return Err(format!("{name} must be at most {max} bytes"));
                }
            }
            if let Some(accounting) = &call.accounting {
                if accounting.version != 2 {
                    return Err("unsupported call accounting version".into());
                }
                let ttl_total = accounting
                    .cache_creation_5m_tokens
                    .unwrap_or(0)
                    .checked_add(accounting.cache_creation_1h_tokens.unwrap_or(0))
                    .ok_or("cache creation TTL counts overflow")?;
                if ttl_total > call.cache_creation_tokens {
                    return Err("cache creation TTL counts exceed aggregate creation".into());
                }
                for value in [
                    &accounting.request_id,
                    &accounting.message_id,
                    &accounting.speed,
                    &accounting.service_tier,
                    &accounting.inference_geo,
                ]
                .into_iter()
                .flatten()
                {
                    if value.len() > CODING_AGENT_ID_MAX_BYTES {
                        return Err("call accounting field exceeds maximum size".into());
                    }
                }
            }
            if call.ended_at < call.started_at {
                return Err("LLM call ended_at precedes started_at".into());
            }
            if !llm_ids.insert(call.id.as_str()) {
                return Err("turn.llm_calls IDs must be unique".into());
            }
        }
        let mut tool_ids = std::collections::HashSet::new();
        for tool in &self.turn.tool_calls {
            for (name, value, max) in [
                (
                    "turn.tool_calls[].id",
                    tool.id.as_str(),
                    CODING_AGENT_ID_MAX_BYTES,
                ),
                (
                    "turn.tool_calls[].name",
                    tool.name.as_str(),
                    CODING_AGENT_NAME_MAX_BYTES,
                ),
                (
                    "turn.tool_calls[].kind",
                    tool.kind.as_str(),
                    CODING_AGENT_NAME_MAX_BYTES,
                ),
            ] {
                if value.trim().is_empty() {
                    return Err(format!("{name} must not be empty"));
                }
                if value.len() > max {
                    return Err(format!("{name} must be at most {max} bytes"));
                }
            }
            if !tool_ids.insert(&tool.id) {
                return Err("turn.tool_calls IDs must be unique".into());
            }
            if tool
                .model_call_id
                .as_ref()
                .is_some_and(|id| id.trim().is_empty() || id.len() > CODING_AGENT_ID_MAX_BYTES)
            {
                return Err("turn.tool_calls[].model_call_id is invalid".into());
            }
            match tool.association {
                CodingAgentToolAssociation::Exact => {
                    let Some(model_call_id) = tool.model_call_id.as_deref() else {
                        return Err("exact tool association requires model_call_id".into());
                    };
                    if !llm_ids.contains(model_call_id) {
                        return Err("exact tool association references an unknown LLM call".into());
                    }
                }
                CodingAgentToolAssociation::Turn | CodingAgentToolAssociation::Unknown => {
                    if tool.model_call_id.is_some() {
                        return Err(
                            "non-exact tool association must not contain model_call_id".into()
                        );
                    }
                }
            }
            if matches!((tool.started_at, tool.ended_at), (Some(start), Some(end)) if end < start) {
                return Err("tool call ended_at precedes started_at".into());
            }
            for (name, value) in [
                ("turn.tool_calls[].arguments", tool.arguments.as_ref()),
                ("turn.tool_calls[].output", tool.output.as_ref()),
            ] {
                if value.is_some_and(|value| {
                    serde_json::to_vec(value)
                        .is_ok_and(|v| v.len() > CODING_AGENT_CONTENT_MAX_BYTES)
                }) {
                    return Err(format!(
                        "{name} must be at most {CODING_AGENT_CONTENT_MAX_BYTES} bytes"
                    ));
                }
            }
            for (name, value) in [
                ("turn.tool_calls[].raw", tool.raw.as_deref()),
                ("turn.tool_calls[].error", tool.error.as_deref()),
            ] {
                if value.is_some_and(|value| value.len() > CODING_AGENT_CONTENT_MAX_BYTES) {
                    return Err(format!(
                        "{name} must be at most {CODING_AGENT_CONTENT_MAX_BYTES} bytes"
                    ));
                }
            }
            if self.capture_policy == CapturePolicy::MetadataOnly
                && (tool.arguments.is_some()
                    || tool.output.is_some()
                    || tool.raw.is_some()
                    || tool.error.is_some())
            {
                return Err("metadata-only events must not contain tool content".into());
            }
        }
        let payload = serde_json::to_value(self)
            .map_err(|error| format!("coding-agent event could not be serialized: {error}"))?;
        if json_contains_nul(&payload) {
            return Err("coding-agent event must not contain NUL characters".into());
        }
        Ok(())
    }
}

impl CodingAgentEventV1 {
    /// Rules that apply only to scoped (subagent/teammate) turns. A scoped turn may lack
    /// a prompt or response (a subagent's run segment need not end in a final answer),
    /// but present content must be nonempty. Intent text is content and is rejected
    /// under metadata-only capture, like `session.title`.
    fn validate_agent_scope(&self, scope: &CodingAgentScope) -> Result<(), String> {
        if scope.agent_id.trim().is_empty() {
            return Err("turn.agent_scope.agent_id must not be empty".into());
        }
        for (name, value, max) in [
            (
                "turn.agent_scope.agent_id",
                Some(scope.agent_id.as_str()),
                CODING_AGENT_ID_MAX_BYTES,
            ),
            (
                "turn.agent_scope.agent_type",
                scope.agent_type.as_deref(),
                CODING_AGENT_NAME_MAX_BYTES,
            ),
            (
                "turn.agent_scope.parent_tool_call_id",
                scope.parent_tool_call_id.as_deref(),
                CODING_AGENT_ID_MAX_BYTES,
            ),
            (
                "turn.agent_scope.parent_agent_id",
                scope.parent_agent_id.as_deref(),
                CODING_AGENT_ID_MAX_BYTES,
            ),
            (
                "turn.agent_scope.name",
                scope.name.as_deref(),
                CODING_AGENT_NAME_MAX_BYTES,
            ),
            (
                "turn.agent_scope.description",
                scope.description.as_deref(),
                CODING_AGENT_SCOPE_DESCRIPTION_MAX_BYTES,
            ),
        ] {
            if value.is_some_and(|value| value.len() > max) {
                return Err(format!("{name} must be at most {max} bytes"));
            }
        }
        for (name, value) in [
            ("turn.agent_scope.description", scope.description.as_deref()),
            ("turn.agent_scope.name", scope.name.as_deref()),
        ] {
            let Some(value) = value else { continue };
            if value.trim().is_empty() {
                return Err(format!("{name} must not be empty"));
            }
            if value.contains('\0') {
                return Err("coding-agent event must not contain NUL characters".into());
            }
        }
        if self.capture_policy == CapturePolicy::MetadataOnly
            && (scope.description.is_some() || scope.name.is_some())
        {
            return Err("metadata-only events must not contain agent scope content".into());
        }
        if self.capture_policy == CapturePolicy::Content
            && [self.turn.prompt.as_deref(), self.turn.response.as_deref()]
                .into_iter()
                .flatten()
                .any(|content| content.trim().is_empty())
        {
            return Err("scoped content events must not contain blank prompt or response".into());
        }
        Ok(())
    }
}

fn json_contains_nul(value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::String(value) => value.contains('\0'),
        serde_json::Value::Array(values) => values.iter().any(json_contains_nul),
        serde_json::Value::Object(values) => values.values().any(json_contains_nul),
        _ => false,
    }
}

pub fn coding_agent_session_id(agent_id: &str, source_session_id: &str) -> String {
    format!("{agent_id}:{source_session_id}")
}

pub fn coding_agent_event_id(agent_id: &str, source_session_id: &str, turn_id: &str) -> String {
    let identity = format!(
        "v{CODING_AGENT_EVENT_VERSION}|{}:{agent_id}|{}:{source_session_id}|{}:{turn_id}",
        agent_id.len(),
        source_session_id.len(),
        turn_id.len()
    );
    Uuid::new_v5(&EVENT_NAMESPACE, identity.as_bytes()).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn event() -> CodingAgentEventV1 {
        let at = Utc.timestamp_opt(1, 0).unwrap();
        CodingAgentEventV1 {
            version: CODING_AGENT_EVENT_VERSION,
            event_id: coding_agent_event_id("claude", "session", "turn"),
            captured_at: at,
            source: CodingAgentSource {
                agent_id: "claude".into(),
                agent_name: "claude-code".into(),
                adapter_version: None,
            },
            session: CodingAgentSession {
                id: coding_agent_session_id("claude", "session"),
                source_id: "session".into(),
                title: None,
            },
            turn: CodingAgentTurn {
                id: "turn".into(),
                prompt: None,
                response: None,
                started_at: at,
                ended_at: at,
                llm_calls: vec![],
                tool_calls: vec![],
                agent_scope: None,
            },
            capture_policy: CapturePolicy::MetadataOnly,
        }
    }

    #[test]
    fn event_ids_are_deterministic_and_identity_scoped() {
        let first = coding_agent_event_id("claude", "session", "turn");
        assert_eq!(first, coding_agent_event_id("claude", "session", "turn"));
        assert_ne!(first, coding_agent_event_id("opencode", "session", "turn"));
        assert_ne!(first, coding_agent_event_id("claude", "other", "turn"));
        assert_ne!(
            coding_agent_event_id("claude", "a\0b", "c"),
            coding_agent_event_id("claude", "a", "b\0c")
        );
        assert!(Uuid::parse_str(&first).is_ok());
    }

    #[test]
    fn session_ids_preserve_existing_stable_identity() {
        assert_eq!(coding_agent_session_id("claude", "abc"), "claude:abc");
    }

    #[test]
    fn batch_size_is_bounded() {
        assert!(
            CodingAgentEventBatchRequest { events: vec![] }
                .validate()
                .is_err()
        );
        let events = vec![event(); CODING_AGENT_BATCH_MAX_EVENTS + 1];
        assert!(CodingAgentEventBatchRequest { events }.validate().is_err());
    }

    #[test]
    fn semantic_field_sizes_and_call_count_are_bounded() {
        let mut oversized_id = event();
        oversized_id.turn.id = "x".repeat(CODING_AGENT_ID_MAX_BYTES + 1);
        assert!(oversized_id.validate().unwrap_err().contains("turn.id"));

        let mut oversized_content = event();
        oversized_content.capture_policy = CapturePolicy::Content;
        oversized_content.turn.prompt = Some("x".repeat(CODING_AGENT_CONTENT_MAX_BYTES + 1));
        oversized_content.turn.response = Some("response".into());
        assert!(
            oversized_content
                .validate()
                .unwrap_err()
                .contains("turn.prompt")
        );

        let mut too_many_calls = event();
        let at = too_many_calls.turn.started_at;
        too_many_calls.turn.llm_calls = (0..=CODING_AGENT_LLM_CALLS_MAX)
            .map(|index| CodingAgentLlmCall {
                id: index.to_string(),
                provider: "provider".into(),
                model: "model".into(),
                input_tokens: 0,
                output_tokens: 0,
                cache_read_tokens: 0,
                cache_creation_tokens: 0,
                accounting: None,
                started_at: at,
                ended_at: at,
            })
            .collect();
        assert!(too_many_calls.validate().unwrap_err().contains("llm_calls"));

        let mut nul_content = event();
        nul_content.capture_policy = CapturePolicy::Content;
        nul_content.turn.prompt = Some("question\0with nul".into());
        nul_content.turn.response = Some("response".into());
        assert!(nul_content.validate().unwrap_err().contains("NUL"));

        let mut nul_json = event();
        nul_json.capture_policy = CapturePolicy::Content;
        nul_json.turn.prompt = Some("question".into());
        nul_json.turn.response = Some("response".into());
        nul_json.turn.tool_calls.push(CodingAgentToolCall {
            id: "tool".into(),
            name: "Read".into(),
            kind: "tool".into(),
            model_call_id: None,
            status: CodingAgentToolCallStatus::Unknown,
            arguments: Some(serde_json::json!({"value": "bad\0value"})),
            output: None,
            raw: None,
            error: None,
            started_at: None,
            ended_at: None,
            duration_ms: None,
            association: CodingAgentToolAssociation::Turn,
            timestamp_quality: CodingAgentTimestampQuality::Unknown,
        });
        assert!(nul_json.validate().unwrap_err().contains("NUL"));
    }

    #[test]
    fn old_v1_turns_without_tools_deserialize_with_an_empty_list() {
        let mut value = serde_json::to_value(event()).unwrap();
        value["turn"].as_object_mut().unwrap().remove("tool_calls");
        let decoded: CodingAgentEventV1 = serde_json::from_value(value).unwrap();
        assert!(decoded.turn.tool_calls.is_empty());
        assert!(decoded.validate().is_ok());
    }

    fn content_event() -> CodingAgentEventV1 {
        let mut event = event();
        event.capture_policy = CapturePolicy::Content;
        event.turn.prompt = Some("question".into());
        event.turn.response = Some("answer".into());
        event
    }

    #[test]
    fn old_v1_sessions_without_titles_round_trip_unchanged() {
        let original = serde_json::to_value(event()).expect("event serializes");
        assert!(original["session"].get("title").is_none());
        let decoded: CodingAgentEventV1 =
            serde_json::from_value(original.clone()).expect("legacy event deserializes");
        assert_eq!(decoded.session.title, None);
        assert!(decoded.validate().is_ok());
        assert_eq!(
            serde_json::to_value(decoded).expect("event serializes"),
            original
        );

        let mut explicit_null = original.clone();
        explicit_null["session"]["title"] = serde_json::Value::Null;
        let decoded: CodingAgentEventV1 =
            serde_json::from_value(explicit_null).expect("null title deserializes");
        assert_eq!(
            serde_json::to_value(decoded).expect("event serializes"),
            original
        );
    }

    #[test]
    fn session_titles_round_trip_verbatim_without_changing_v1_identity() {
        let mut event = content_event();
        let event_id = event.event_id.clone();
        event.session.title = Some("  External session title  ".into());
        assert!(event.validate().is_ok());
        let encoded = serde_json::to_value(&event).expect("event serializes");
        assert_eq!(encoded["session"]["title"], "  External session title  ");
        let decoded: CodingAgentEventV1 =
            serde_json::from_value(encoded).expect("titled event deserializes");
        assert_eq!(decoded, event);
        assert_eq!(decoded.version, 1);
        assert_eq!(decoded.event_id, event_id);
        assert_eq!(
            decoded.session.id,
            coding_agent_session_id("claude", "session")
        );
    }

    #[test]
    fn metadata_only_events_reject_session_titles() {
        let mut event = event();
        event.session.title = Some("private session content".into());
        assert_eq!(
            event.validate().expect_err("title is content"),
            "metadata-only events must not contain session.title"
        );
    }

    #[test]
    fn session_titles_reject_blank_and_nul_content() {
        let mut event = content_event();
        for title in ["", " \t\n", "\u{2003}"] {
            event.session.title = Some(title.into());
            assert_eq!(
                event.validate().expect_err("blank title"),
                "session.title must not be empty"
            );
        }
        event.session.title = Some("title\0with nul".into());
        assert!(event.validate().expect_err("NUL title").contains("NUL"));
    }

    #[test]
    fn session_titles_are_bounded_by_utf8_bytes() {
        let mut event = content_event();
        for title in [
            "x".repeat(CODING_AGENT_SESSION_TITLE_MAX_BYTES),
            "é".repeat(CODING_AGENT_SESSION_TITLE_MAX_BYTES / 2),
        ] {
            event.session.title = Some(title.clone());
            assert!(event.validate().is_ok(), "exact byte limit is allowed");
            event.session.title = Some(title + "x");
            assert_eq!(
                event.validate().expect_err("oversized title"),
                format!(
                    "session.title must be at most {CODING_AGENT_SESSION_TITLE_MAX_BYTES} bytes"
                )
            );
        }
    }

    #[test]
    fn tool_association_rejects_missing_dangling_and_non_exact_call_ids() {
        let mut value = event();
        let at = value.turn.started_at;
        value.turn.llm_calls.push(CodingAgentLlmCall {
            id: "call-1".into(),
            provider: "p".into(),
            model: "m".into(),
            input_tokens: 0,
            output_tokens: 0,
            cache_read_tokens: 0,
            cache_creation_tokens: 0,
            accounting: None,
            started_at: at,
            ended_at: at,
        });
        let tool = CodingAgentToolCall {
            id: "tool-1".into(),
            name: "Read".into(),
            kind: "tool".into(),
            model_call_id: None,
            status: CodingAgentToolCallStatus::Unknown,
            arguments: None,
            output: None,
            raw: None,
            error: None,
            started_at: None,
            ended_at: None,
            duration_ms: None,
            association: CodingAgentToolAssociation::Exact,
            timestamp_quality: CodingAgentTimestampQuality::Unknown,
        };
        value.turn.tool_calls.push(tool);
        assert!(
            value
                .validate()
                .unwrap_err()
                .contains("requires model_call_id")
        );
        value.turn.tool_calls[0].model_call_id = Some("missing".into());
        assert!(value.validate().unwrap_err().contains("unknown LLM call"));
        value.turn.tool_calls[0].association = CodingAgentToolAssociation::Turn;
        assert!(value.validate().unwrap_err().contains("non-exact"));
        value.turn.tool_calls[0].model_call_id = None;
        assert!(value.validate().is_ok());
    }
    // ─── cross-version contract (TELE-08) ───────────────────────────────────

    const CLAUDE_V1_0_CONTENT: &str =
        include_str!("../tests/fixtures/coding_agent_v1_0_claude_content.json");
    const CODEX_V1_0_METADATA: &str =
        include_str!("../tests/fixtures/coding_agent_v1_0_codex_metadata.json");

    /// Frozen copies of the v1.0 wire structs as shipped before the additive
    /// fields existed. They stand in for an old server decoding a new CLI's
    /// payload; like the originals they do not deny unknown fields.
    mod legacy_v1_0 {
        use super::super::{
            CapturePolicy, CodingAgentTimestampQuality, CodingAgentToolAssociation,
            CodingAgentToolCallStatus,
        };
        use chrono::{DateTime, Utc};
        use serde::Deserialize;

        #[derive(Debug, Deserialize)]
        pub struct Source {
            pub agent_id: String,
            #[allow(dead_code, reason = "frozen wire shape; only decoded")]
            pub agent_name: String,
        }

        #[derive(Debug, Deserialize)]
        #[allow(dead_code, reason = "frozen wire shape; only decoded")]
        pub struct Session {
            pub id: String,
            pub source_id: String,
            #[serde(default)]
            pub title: Option<String>,
        }

        #[derive(Debug, Deserialize)]
        #[allow(dead_code, reason = "frozen wire shape; only decoded")]
        pub struct Accounting {
            pub version: u32,
            #[serde(default)]
            pub request_id: Option<String>,
            #[serde(default)]
            pub message_id: Option<String>,
            #[serde(default)]
            pub cache_creation_5m_tokens: Option<u64>,
            #[serde(default)]
            pub cache_creation_1h_tokens: Option<u64>,
            #[serde(default)]
            pub speed: Option<String>,
            #[serde(default)]
            pub service_tier: Option<String>,
            #[serde(default)]
            pub inference_geo: Option<String>,
            #[serde(default)]
            pub conflicting_observations: bool,
        }

        #[derive(Debug, Deserialize)]
        #[allow(dead_code, reason = "frozen wire shape; only decoded")]
        pub struct LlmCall {
            pub id: String,
            pub provider: String,
            pub model: String,
            pub input_tokens: u64,
            pub output_tokens: u64,
            pub cache_read_tokens: u64,
            pub cache_creation_tokens: u64,
            #[serde(default)]
            pub accounting: Option<Accounting>,
            pub started_at: DateTime<Utc>,
            pub ended_at: DateTime<Utc>,
        }

        #[derive(Debug, Deserialize)]
        #[allow(dead_code, reason = "frozen wire shape; only decoded")]
        pub struct ToolCall {
            pub id: String,
            pub name: String,
            pub kind: String,
            pub model_call_id: Option<String>,
            pub status: CodingAgentToolCallStatus,
            pub arguments: Option<serde_json::Value>,
            pub output: Option<serde_json::Value>,
            pub raw: Option<String>,
            pub error: Option<String>,
            pub started_at: Option<DateTime<Utc>>,
            pub ended_at: Option<DateTime<Utc>>,
            pub duration_ms: Option<u64>,
            pub association: CodingAgentToolAssociation,
            pub timestamp_quality: CodingAgentTimestampQuality,
        }

        #[derive(Debug, Deserialize)]
        #[allow(dead_code, reason = "frozen wire shape; only decoded")]
        pub struct Turn {
            pub id: String,
            pub prompt: Option<String>,
            pub response: Option<String>,
            pub started_at: DateTime<Utc>,
            pub ended_at: DateTime<Utc>,
            pub llm_calls: Vec<LlmCall>,
            #[serde(default)]
            pub tool_calls: Vec<ToolCall>,
        }

        #[derive(Debug, Deserialize)]
        #[allow(dead_code, reason = "frozen wire shape; only decoded")]
        pub struct Event {
            pub version: u32,
            pub event_id: String,
            pub captured_at: DateTime<Utc>,
            pub source: Source,
            pub session: Session,
            pub turn: Turn,
            pub capture_policy: CapturePolicy,
        }
    }

    fn scope(kind: CodingAgentScopeKind) -> CodingAgentScope {
        CodingAgentScope {
            kind,
            agent_id: "a1b2c3d4e5f6".into(),
            agent_type: Some("Explore".into()),
            parent_tool_call_id: Some("toolu_01Parent".into()),
            parent_agent_id: None,
            spawn_depth: Some(1),
            description: None,
            name: None,
        }
    }

    fn scoped_content_event() -> CodingAgentEventV1 {
        let mut event = event();
        event.capture_policy = CapturePolicy::Content;
        event.turn.agent_scope = Some(scope(CodingAgentScopeKind::Subagent));
        event
    }

    #[test]
    fn event_version_stays_one() {
        assert_eq!(CODING_AGENT_EVENT_VERSION, 1);
    }

    #[test]
    fn v1_0_payloads_round_trip_byte_identical() {
        for fixture in [CLAUDE_V1_0_CONTENT, CODEX_V1_0_METADATA] {
            let original: serde_json::Value =
                serde_json::from_str(fixture).expect("fixture is JSON");
            let decoded: CodingAgentEventV1 =
                serde_json::from_value(original.clone()).expect("v1.0 fixture decodes");
            assert_eq!(decoded.source.adapter_version, None);
            assert_eq!(decoded.turn.agent_scope, None);
            assert!(
                decoded
                    .turn
                    .llm_calls
                    .iter()
                    .filter_map(|call| call.accounting.as_ref())
                    .all(|accounting| accounting.output_tokens_final.is_none())
            );
            assert_eq!(decoded.validate(), Ok(()));
            assert_eq!(
                serde_json::to_value(&decoded).expect("event serializes"),
                original
            );
        }
    }

    #[test]
    fn v1_0_types_ignore_v1_1_fields() {
        let mut event: CodingAgentEventV1 =
            serde_json::from_str(CLAUDE_V1_0_CONTENT).expect("fixture decodes");
        event.source.adapter_version = Some(CLAUDE_ADAPTER_VERSION_SUBAGENTS);
        event.turn.agent_scope = Some(CodingAgentScope {
            description: Some("Find where reconcile is defined".into()),
            name: Some("researcher".into()),
            ..scope(CodingAgentScopeKind::Subagent)
        });
        for call in &mut event.turn.llm_calls {
            if let Some(accounting) = call.accounting.as_mut() {
                accounting.output_tokens_final = Some(false);
            }
        }
        assert_eq!(event.validate(), Ok(()));
        let encoded = serde_json::to_value(&event).expect("event serializes");
        assert_eq!(encoded["source"]["adapter_version"], 1);
        assert_eq!(encoded["turn"]["agent_scope"]["kind"], "subagent");

        let legacy: legacy_v1_0::Event =
            serde_json::from_value(encoded).expect("old structs tolerate new fields");
        assert_eq!(legacy.event_id, event.event_id);
        assert_eq!(
            coding_agent_event_id(
                &legacy.source.agent_id,
                &legacy.session.source_id,
                &legacy.turn.id
            ),
            event.event_id
        );
    }

    #[test]
    fn unknown_scope_kind_maps_to_unknown() {
        let kind: CodingAgentScopeKind =
            serde_json::from_str("\"swarm_member\"").expect("unknown kind decodes");
        assert_eq!(kind, CodingAgentScopeKind::Unknown);
        for (kind, wire) in [
            (CodingAgentScopeKind::Subagent, "\"subagent\""),
            (CodingAgentScopeKind::Teammate, "\"teammate\""),
        ] {
            assert_eq!(serde_json::to_string(&kind).expect("serializes"), wire);
            assert_eq!(
                serde_json::from_str::<CodingAgentScopeKind>(wire).expect("decodes"),
                kind
            );
        }
    }

    #[test]
    fn scoped_content_turns_may_omit_prompt_and_response() {
        assert_eq!(scoped_content_event().validate(), Ok(()));

        let mut with_prompt = scoped_content_event();
        with_prompt.turn.prompt = Some("Find where reconcile is defined".into());
        assert_eq!(with_prompt.validate(), Ok(()));

        let mut blank = scoped_content_event();
        blank.turn.prompt = Some("  ".into());
        assert!(blank.validate().is_err());

        let mut blank_response = scoped_content_event();
        blank_response.turn.response = Some("\n".into());
        assert!(blank_response.validate().is_err());

        let mut unscoped = content_event();
        unscoped.turn.response = None;
        assert_eq!(
            unscoped
                .validate()
                .expect_err("main turns still need content"),
            "content events must contain nonempty prompt and response content"
        );
    }

    #[test]
    fn metadata_only_scoped_turns_reject_intent_content() {
        let mut metadata = event();
        metadata.turn.agent_scope = Some(CodingAgentScope {
            parent_agent_id: Some("parent-agent".into()),
            ..scope(CodingAgentScopeKind::Subagent)
        });
        assert_eq!(metadata.validate(), Ok(()));

        for scope in [
            CodingAgentScope {
                description: Some("Find where X is defined".into()),
                ..scope(CodingAgentScopeKind::Subagent)
            },
            CodingAgentScope {
                name: Some("researcher".into()),
                ..scope(CodingAgentScopeKind::Teammate)
            },
        ] {
            metadata.turn.agent_scope = Some(scope);
            assert_eq!(
                metadata.validate().expect_err("intent is content"),
                "metadata-only events must not contain agent scope content"
            );
        }
    }

    #[test]
    fn agent_scope_fields_are_bounded() {
        let cases: Vec<fn(&mut CodingAgentScope)> = vec![
            |scope| scope.agent_id = String::new(),
            |scope| scope.agent_id = "  ".into(),
            |scope| scope.agent_id = "x".repeat(CODING_AGENT_ID_MAX_BYTES + 1),
            |scope| scope.agent_type = Some("x".repeat(CODING_AGENT_NAME_MAX_BYTES + 1)),
            |scope| scope.name = Some("x".repeat(CODING_AGENT_NAME_MAX_BYTES + 1)),
            |scope| {
                scope.description = Some("x".repeat(CODING_AGENT_SCOPE_DESCRIPTION_MAX_BYTES + 1))
            },
            |scope| scope.parent_tool_call_id = Some("x".repeat(CODING_AGENT_ID_MAX_BYTES + 1)),
            |scope| scope.parent_agent_id = Some("x".repeat(CODING_AGENT_ID_MAX_BYTES + 1)),
            |scope| scope.description = Some(" ".into()),
            |scope| scope.name = Some(String::new()),
            |scope| scope.description = Some("task\0with nul".into()),
        ];
        for (index, mutate) in cases.into_iter().enumerate() {
            let mut event = scoped_content_event();
            let scope = event.turn.agent_scope.as_mut().expect("scoped");
            mutate(scope);
            assert!(event.validate().is_err(), "case {index} must be rejected");
        }

        let mut at_limit = scoped_content_event();
        let scope = at_limit.turn.agent_scope.as_mut().expect("scoped");
        scope.description = Some("x".repeat(CODING_AGENT_SCOPE_DESCRIPTION_MAX_BYTES));
        scope.name = Some("x".repeat(CODING_AGENT_NAME_MAX_BYTES));
        assert_eq!(at_limit.validate(), Ok(()));
    }

    #[test]
    fn capabilities_list_the_shared_feature_slugs() {
        assert_eq!(
            CODING_AGENT_TELEMETRY_FEATURES,
            &[
                CODING_AGENT_FEATURE_AGENT_SCOPE,
                CODING_AGENT_FEATURE_ADAPTER_VERSION
            ]
        );
        let capabilities = CodingAgentCapabilities {
            event_version: CODING_AGENT_EVENT_VERSION,
            features: CODING_AGENT_TELEMETRY_FEATURES
                .iter()
                .map(|feature| feature.to_string())
                .collect(),
        };
        assert_eq!(
            serde_json::to_value(&capabilities).expect("serializes"),
            serde_json::json!({"event_version": 1, "features": ["agent_scope", "adapter_version"]})
        );
        const { assert!(CLAUDE_ADAPTER_VERSION_TEAMMATES > CLAUDE_ADAPTER_VERSION_SUBAGENTS) };
        assert_eq!(CODEX_ADAPTER_VERSION_EXCLUSIVE_INPUT, 1);
    }
}
