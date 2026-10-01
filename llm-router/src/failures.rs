//! Best-effort durable record of failed upstream LLM calls.
//!
//! `token_usage` only holds calls that produced a response, so an error-rate
//! monitor has no numerator without this table. One `llm_call_failures` row is
//! written per final request outcome (not per fallback attempt). Writes are
//! detached and swallowed: a failure to record never changes the response or
//! status the caller gets, and never touches `token_usage` or budget counters.
//! Budget denials are rejected by `budget::enforce` before any hook here runs,
//! so they never produce a row. Only ids, provider, model, status and a coarse
//! kind are stored, never upstream error text or prompts.

use sqlx::PgPool;
use uuid::Uuid;

use crate::providers::ProviderError;

pub const KIND_UPSTREAM_4XX: &str = "upstream_4xx";
pub const KIND_RATE_LIMITED: &str = "rate_limited";
pub const KIND_UPSTREAM_5XX: &str = "upstream_5xx";
pub const KIND_TIMEOUT: &str = "timeout";
pub const KIND_TRANSPORT: &str = "transport";
pub const KIND_PARSE: &str = "parse";
pub const KIND_STREAM_ERROR: &str = "stream_error";
pub const KIND_CONFIG: &str = "config";

/// HTTP status that maps to the dedicated `rate_limited` kind.
const STATUS_RATE_LIMITED: u16 = 429;
/// First status of the server-error range.
const STATUS_SERVER_ERROR_MIN: u16 = 500;

/// What went wrong with an upstream call, coarse enough to filter on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FailureClass {
    pub status_code: Option<u16>,
    pub error_kind: &'static str,
}

impl FailureClass {
    /// A failure with no HTTP status (transport, parse, config, stream).
    pub const fn kind(error_kind: &'static str) -> Self {
        Self {
            status_code: None,
            error_kind,
        }
    }

    /// A mid-stream failure after the response was already committed.
    pub const fn stream_error() -> Self {
        Self::kind(KIND_STREAM_ERROR)
    }
}

/// Classify an upstream HTTP status.
pub fn classify_status(status: u16) -> FailureClass {
    let error_kind = if status == STATUS_RATE_LIMITED {
        KIND_RATE_LIMITED
    } else if status >= STATUS_SERVER_ERROR_MIN {
        KIND_UPSTREAM_5XX
    } else {
        KIND_UPSTREAM_4XX
    };
    FailureClass {
        status_code: Some(status),
        error_kind,
    }
}

/// True for transport text that denotes a timeout. reqwest's `Display` for a
/// timed-out request contains "timed out"; there is no structured signal once
/// the error has been flattened to a `String`, so this is a heuristic.
fn is_timeout_text(text: &str) -> bool {
    text.to_ascii_lowercase().contains("timed out")
}

/// Classify a provider error.
pub fn classify_provider_error(e: &ProviderError) -> FailureClass {
    match e {
        ProviderError::Status { status, .. } => classify_status(*status),
        ProviderError::Transport(text) if is_timeout_text(text) => FailureClass::kind(KIND_TIMEOUT),
        ProviderError::Transport(_) => FailureClass::kind(KIND_TRANSPORT),
        ProviderError::Parse(_) => FailureClass::kind(KIND_PARSE),
    }
}

/// Classify a flattened `GatewayError::Upstream` message (Responses path).
pub fn classify_upstream_text(text: &str) -> FailureClass {
    if is_timeout_text(text) {
        FailureClass::kind(KIND_TIMEOUT)
    } else {
        FailureClass::kind(KIND_TRANSPORT)
    }
}

/// One failed call, ids as the router sees them (strings; non-UUIDs become NULL).
#[derive(Debug, Clone)]
pub struct FailureRecord {
    pub agent_id: String,
    pub user_id: String,
    pub provider: String,
    pub model: String,
    pub class: FailureClass,
    pub streaming: bool,
    pub operation_type: &'static str,
}

/// Spawn the failure write so it never blocks or fails the response.
pub fn spawn_log_failure(db: PgPool, record: FailureRecord) {
    tokio::spawn(async move {
        if let Err(e) = insert_failure(&db, &record).await {
            tracing::warn!(error = %e, "llm_call_failures write failed (swallowed)");
        }
    });
}

async fn insert_failure(db: &PgPool, r: &FailureRecord) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO llm_call_failures \
         (agent_id, user_id, provider, model, status_code, error_kind, streaming, operation_type) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
    )
    .bind(Uuid::parse_str(&r.agent_id).ok())
    .bind(Uuid::parse_str(&r.user_id).ok())
    .bind(&r.provider)
    .bind(&r.model)
    .bind(r.class.status_code.map(i32::from))
    .bind(r.class.error_kind)
    .bind(r.streaming)
    .bind(r.operation_type)
    .execute(db)
    .await
    .map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_status_kinds() {
        for (status, kind) in [
            (400, KIND_UPSTREAM_4XX),
            (404, KIND_UPSTREAM_4XX),
            (429, KIND_RATE_LIMITED),
            (500, KIND_UPSTREAM_5XX),
            (502, KIND_UPSTREAM_5XX),
            (503, KIND_UPSTREAM_5XX),
        ] {
            let c = classify_status(status);
            assert_eq!(c.error_kind, kind, "{status}");
            assert_eq!(c.status_code, Some(status));
        }
    }

    #[test]
    fn classify_provider_error_kinds() {
        let c = classify_provider_error(&ProviderError::Status {
            status: 503,
            message: "x".into(),
            retryable: true,
        });
        assert_eq!(
            (c.error_kind, c.status_code),
            (KIND_UPSTREAM_5XX, Some(503))
        );
        let c = classify_provider_error(&ProviderError::Transport("operation timed out".into()));
        assert_eq!((c.error_kind, c.status_code), (KIND_TIMEOUT, None));
        let c = classify_provider_error(&ProviderError::Transport("connection refused".into()));
        assert_eq!(c.error_kind, KIND_TRANSPORT);
        let c = classify_provider_error(&ProviderError::Parse("bad json".into()));
        assert_eq!(c.error_kind, KIND_PARSE);
    }

    #[test]
    fn upstream_text_timeout_heuristic() {
        assert_eq!(
            classify_upstream_text("error sending request: operation timed out").error_kind,
            KIND_TIMEOUT
        );
        assert_eq!(
            classify_upstream_text("dns error").error_kind,
            KIND_TRANSPORT
        );
    }
}
