//! Rendering of a budget refusal in the caller's own error dialect.
//!
//! An agent's SDK parses errors in the format it called us with, so a 429 for an
//! exhausted budget has to look like that provider's rate-limit error while still
//! carrying a machine-readable `nasiko_budget` block. A counter-store outage is a
//! different failure (503) and deliberately carries no budget details: it must
//! not reveal spend, and the message never includes the Redis error text.

use axum::Json;
use axum::http::{HeaderValue, StatusCode, header::RETRY_AFTER};
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};

use super::ExceededInfo;
use super::keys::micros_to_usd;
use crate::inbound::InboundFormat;

/// Stable slug for a refusal caused by an unreachable counter store.
pub const STORE_UNAVAILABLE_CODE: &str = "budget_store_unavailable";
const BUDGET_EXCEEDED_CODE: &str = "budget_exceeded";
const STORE_UNAVAILABLE_MESSAGE: &str = "LLM budget store unavailable; request refused";

#[derive(Debug, Clone)]
pub enum DenialKind {
    /// An applicable block budget is exhausted.
    Exceeded(ExceededInfo),
    /// A budget applies but its spend could not be read (fail closed).
    StoreUnavailable,
}

#[derive(Debug, Clone)]
pub struct BudgetDenial {
    pub kind: DenialKind,
    pub format: InboundFormat,
    pub retry_after_secs: u64,
}

impl BudgetDenial {
    pub fn status(&self) -> StatusCode {
        match self.kind {
            DenialKind::Exceeded(_) => StatusCode::TOO_MANY_REQUESTS,
            DenialKind::StoreUnavailable => StatusCode::SERVICE_UNAVAILABLE,
        }
    }

    /// The response body in `self.format`'s error shape.
    pub fn body(&self) -> Value {
        match &self.kind {
            DenialKind::Exceeded(info) => {
                let message = format!(
                    "LLM budget exceeded for {} budget {}; resets at {}",
                    info.scope.as_str(),
                    info.budget_id,
                    info.resets_at.to_rfc3339()
                );
                let mut body = match self.format {
                    InboundFormat::OpenAi => json!({"error": {
                        "message": message,
                        "type": "insufficient_quota",
                        "code": BUDGET_EXCEEDED_CODE,
                        "param": null,
                    }}),
                    InboundFormat::Anthropic => json!({
                        "type": "error",
                        "error": {"type": "rate_limit_error", "message": message},
                        "code": BUDGET_EXCEEDED_CODE,
                    }),
                    InboundFormat::Gemini => json!({"error": {
                        "code": 429,
                        "message": message,
                        "status": "RESOURCE_EXHAUSTED",
                    }}),
                };
                body["nasiko_budget"] = json!({
                    "budget_id": info.budget_id,
                    "scope": info.scope.as_str(),
                    "period": info.period.as_str(),
                    "limit_usd": micros_to_usd(info.limit_micros),
                    "spend_usd": micros_to_usd(info.spend_micros),
                    "resets_at": info.resets_at.to_rfc3339(),
                });
                body
            }
            DenialKind::StoreUnavailable => match self.format {
                InboundFormat::OpenAi => json!({"error": {
                    "message": STORE_UNAVAILABLE_MESSAGE,
                    "type": "server_error",
                    "code": STORE_UNAVAILABLE_CODE,
                    "param": null,
                }}),
                InboundFormat::Anthropic => json!({
                    "type": "error",
                    "error": {"type": "api_error", "message": STORE_UNAVAILABLE_MESSAGE},
                    "code": STORE_UNAVAILABLE_CODE,
                }),
                InboundFormat::Gemini => json!({
                    "error": {
                        "code": 503,
                        "message": STORE_UNAVAILABLE_MESSAGE,
                        "status": "UNAVAILABLE",
                    },
                    "code": STORE_UNAVAILABLE_CODE,
                }),
            },
        }
    }
}

impl IntoResponse for BudgetDenial {
    fn into_response(self) -> Response {
        let mut response = (self.status(), Json(self.body())).into_response();
        response
            .headers_mut()
            .insert(RETRY_AFTER, HeaderValue::from(self.retry_after_secs));
        response
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::budget::{Period, Scope};
    use chrono::{TimeZone, Utc};
    use uuid::Uuid;

    fn info() -> ExceededInfo {
        ExceededInfo {
            budget_id: Uuid::nil(),
            scope: Scope::Agent,
            period: Period::Daily,
            limit_micros: 4_000_000,
            spend_micros: 4_500_000,
            resets_at: Utc.with_ymd_and_hms(2026, 3, 16, 0, 0, 0).unwrap(),
        }
    }

    fn denial(kind: DenialKind, format: InboundFormat) -> BudgetDenial {
        BudgetDenial {
            kind,
            format,
            retry_after_secs: 42,
        }
    }

    fn assert_budget_block(body: &Value) {
        let nb = &body["nasiko_budget"];
        assert_eq!(nb["budget_id"], Uuid::nil().to_string());
        assert_eq!(nb["scope"], "agent");
        assert_eq!(nb["period"], "daily");
        assert_eq!(nb["limit_usd"], 4.0);
        assert_eq!(nb["spend_usd"], 4.5);
        assert_eq!(nb["resets_at"], "2026-03-16T00:00:00+00:00");
    }

    #[test]
    fn openai_exceeded() {
        let d = denial(DenialKind::Exceeded(info()), InboundFormat::OpenAi);
        assert_eq!(d.status(), StatusCode::TOO_MANY_REQUESTS);
        let body = d.body();
        assert_eq!(body["error"]["type"], "insufficient_quota");
        assert_eq!(body["error"]["code"], "budget_exceeded");
        assert!(body["error"]["param"].is_null());
        assert_budget_block(&body);
        let response = d.into_response();
        assert_eq!(response.headers()[RETRY_AFTER], "42");
    }

    #[test]
    fn anthropic_exceeded() {
        let d = denial(DenialKind::Exceeded(info()), InboundFormat::Anthropic);
        let body = d.body();
        assert_eq!(body["type"], "error");
        assert_eq!(body["error"]["type"], "rate_limit_error");
        assert_eq!(body["code"], "budget_exceeded");
        assert_budget_block(&body);
    }

    #[test]
    fn gemini_exceeded() {
        let d = denial(DenialKind::Exceeded(info()), InboundFormat::Gemini);
        let body = d.body();
        assert_eq!(body["error"]["code"], 429);
        assert_eq!(body["error"]["status"], "RESOURCE_EXHAUSTED");
        assert_budget_block(&body);
    }

    #[test]
    fn exceeded_message_names_no_budget_label() {
        let body = denial(DenialKind::Exceeded(info()), InboundFormat::OpenAi).body();
        let message = body["error"]["message"].as_str().unwrap();
        assert!(message.contains("agent budget"));
        assert!(message.contains("2026-03-16"));
    }

    fn assert_unavailable(format: InboundFormat) -> Value {
        let d = denial(DenialKind::StoreUnavailable, format);
        assert_eq!(d.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body = d.body();
        assert!(body.get("nasiko_budget").is_none());
        let text = body.to_string();
        assert!(text.contains("budget_store_unavailable"));
        assert!(!text.to_lowercase().contains("redis"));
        assert_eq!(d.into_response().headers()[RETRY_AFTER], "42");
        body
    }

    #[test]
    fn openai_store_unavailable() {
        let body = assert_unavailable(InboundFormat::OpenAi);
        assert_eq!(body["error"]["type"], "server_error");
        assert_eq!(body["error"]["code"], "budget_store_unavailable");
    }

    #[test]
    fn anthropic_store_unavailable() {
        let body = assert_unavailable(InboundFormat::Anthropic);
        assert_eq!(body["error"]["type"], "api_error");
        assert_eq!(body["code"], "budget_store_unavailable");
    }

    #[test]
    fn gemini_store_unavailable() {
        let body = assert_unavailable(InboundFormat::Gemini);
        assert_eq!(body["error"]["code"], 503);
        assert_eq!(body["error"]["status"], "UNAVAILABLE");
        assert_eq!(body["code"], "budget_store_unavailable");
    }
}
