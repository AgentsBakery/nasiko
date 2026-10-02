//! Legacy Codex cached-input correction on the read paths (TELE-02, D2).
//!
//! Codex CLIs before adapter_version 1 reported input tokens inclusive of
//! cache reads, so their history counts cached input twice. The correction is
//! applied at read time from `coding_agent_turn_usage`; receipts and
//! `trace_usage` rows are never rewritten.
//!
//! Covers:
//!   GET /api/observability/finops/dashboard (reads `trace_usage_corrected`)
//!   GET /api/chat/sessions/{id}             (per-trace overlay on messages)
//! and that the stored receipts and `trace_usage` rows stay byte-identical.
//!
//! Tempo-backed session/trace views have no local Tempo; their appliers are
//! unit-tested in `server/src/observability/codex_correction.rs`.
//!
//! Requires infra (Postgres :5432, Redis, S3):
//!   cargo test -p nasiko-server --test codex_correction -- --test-threads=1

mod common;

use chrono::{TimeZone, Utc};
use nasiko_server::observability::codex_correction;
use nasiko_types::{
    CODEX_ADAPTER_VERSION_EXCLUSIVE_INPUT, CODING_AGENT_EVENT_VERSION, CapturePolicy,
    CodingAgentEventV1, CodingAgentLlmCall, CodingAgentScope, CodingAgentScopeKind,
    CodingAgentSession, CodingAgentSource, CodingAgentTurn, coding_agent_event_id,
    coding_agent_session_id,
};
use rust_decimal::Decimal;
use serde_json::{Value, json};
use serial_test::serial;
use uuid::Uuid;

const AGENT_NAME: &str = "coding-agent";
const EPS: f64 = 1e-6;
const CLAUDE_INPUT: i64 = 500;
const CLAUDE_COST: f64 = 0.3;

// ─── helpers ────────────────────────────────────────────────────────────────

/// Initialise the admin and register one owned integration per source agent.
async fn setup(server: &common::TestServer, sources: &[&str]) -> Uuid {
    let admin: Value = server
        .client
        .post(server.url("/api/auth/initialize-admin"))
        .json(&json!({"username": "admin", "email": "admin@codex-correction.test"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let user_id = Uuid::parse_str(admin["user_id"].as_str().unwrap()).unwrap();
    for source in sources {
        sqlx::query(
            "INSERT INTO agents (name, owner_id, coding_agent_integration_id) VALUES ($1, $2, $3)",
        )
        .bind(agent_name(source))
        .bind(user_id)
        .bind(source)
        .execute(&server.db)
        .await
        .unwrap();
    }
    user_id
}

fn agent_name(source: &str) -> String {
    format!("{AGENT_NAME}-{source}")
}

fn event(source: &str, session: &str, turn: &str) -> CodingAgentEventV1 {
    let started_at = Utc.timestamp_opt(1_700_000_000, 0).unwrap();
    let ended_at = Utc.timestamp_opt(1_700_000_002, 0).unwrap();
    CodingAgentEventV1 {
        version: CODING_AGENT_EVENT_VERSION,
        event_id: coding_agent_event_id(source, session, turn),
        captured_at: ended_at,
        source: CodingAgentSource {
            agent_id: source.into(),
            agent_name: agent_name(source),
            adapter_version: None,
        },
        session: CodingAgentSession {
            id: coding_agent_session_id(source, session),
            source_id: session.into(),
            title: None,
        },
        turn: CodingAgentTurn {
            id: turn.into(),
            prompt: Some("question".into()),
            response: Some("answer".into()),
            started_at,
            ended_at,
            llm_calls: vec![CodingAgentLlmCall {
                id: format!("call-{turn}"),
                provider: "openai".into(),
                model: "gpt-4o".into(),
                input_tokens: 1000,
                output_tokens: 50,
                cache_read_tokens: 800,
                cache_creation_tokens: 0,
                accounting: None,
                started_at,
                ended_at,
            }],
            tool_calls: vec![],
            agent_scope: None,
        },
        capture_policy: CapturePolicy::Content,
    }
}

async fn ingest(server: &common::TestServer, user_id: Uuid, event: &CodingAgentEventV1) {
    let response: Value = common::as_member(
        server
            .client
            .post(server.url("/api/telemetry/coding-agent/events/batch")),
        &user_id.to_string(),
        "admin",
    )
    .json(&json!({"events": [event]}))
    .send()
    .await
    .unwrap()
    .json()
    .await
    .unwrap();
    assert_eq!(
        response["data"]["results"][0]["status"], "accepted",
        "{response}"
    );
}

/// The rollup row for one receipt as `(session_id, trace_id, cost delta)`.
async fn rollup(server: &common::TestServer, event_id: &str) -> (String, String, f64) {
    let (session_id, trace_id, delta): (String, String, Decimal) = sqlx::query_as(
        "SELECT session_id, trace_id, COALESCE(reported_cost_usd - cost_usd, 0) \
         FROM coding_agent_turn_usage WHERE event_id = $1",
    )
    .bind(event_id)
    .fetch_one(&server.db)
    .await
    .unwrap();
    (session_id, trace_id, delta.to_string().parse().unwrap())
}

/// The reported (inclusive) chat cost of the assistant reply in `session_id`.
async fn reported_chat_cost(server: &common::TestServer, session_id: &str) -> f64 {
    let cost: Decimal = sqlx::query_scalar(
        "SELECT cost_usd FROM chat_messages WHERE role = 'assistant' AND session_id = $1",
    )
    .bind(session_id)
    .fetch_one(&server.db)
    .await
    .unwrap();
    cost.to_string().parse().unwrap()
}

/// Insert one `trace_usage` row an hour ago, the way the materializer would.
async fn seed_trace_usage(
    server: &common::TestServer,
    trace_id: &str,
    source: &str,
    session_id: &str,
    input: i64,
    cost: f64,
) {
    sqlx::query(
        r#"INSERT INTO trace_usage
             (trace_id, agent_name, session_id, model, provider, input_tokens, output_tokens,
              cache_read_tokens, cost_usd, prompt_cost_usd, completion_cost_usd, latency_ms,
              started_at, tool_call_count, cost_estimated)
           VALUES ($1, $2, $3, 'gpt-4o', 'openai', $4, 50, 800, $5, $5, 0, 1200,
                   now() - interval '1 hour', 0, false)"#,
    )
    .bind(trace_id)
    .bind(agent_name(source))
    .bind(session_id)
    .bind(input)
    .bind(cost)
    .execute(&server.db)
    .await
    .unwrap();
}

async fn get(server: &common::TestServer, user_id: Uuid, path: &str) -> Value {
    let res = common::as_superuser(
        server.client.get(server.url(path)),
        &user_id.to_string(),
        "admin",
    )
    .send()
    .await
    .unwrap();
    assert_eq!(res.status().as_u16(), 200, "{path}");
    res.json().await.unwrap()
}

async fn dashboard(server: &common::TestServer, user_id: Uuid) -> Value {
    get(
        server,
        user_id,
        "/api/observability/finops/dashboard?range=7d",
    )
    .await["data"]
        .clone()
}

/// Every stored receipt payload and `trace_usage` row, for immutability checks.
async fn stored_snapshot(server: &common::TestServer) -> (Value, Value) {
    let receipts: Value = sqlx::query_scalar(
        "SELECT COALESCE(jsonb_agg(payload ORDER BY event_id), '[]'::jsonb) \
         FROM coding_agent_telemetry_events",
    )
    .fetch_one(&server.db)
    .await
    .unwrap();
    let usage: Value = sqlx::query_scalar(
        "SELECT COALESCE(jsonb_agg(to_jsonb(t) ORDER BY trace_id), '[]'::jsonb) FROM trace_usage t",
    )
    .fetch_one(&server.db)
    .await
    .unwrap();
    (receipts, usage)
}

fn assert_close(actual: f64, expected: f64, what: &str) {
    assert!(
        (actual - expected).abs() < EPS,
        "{what}: {actual} vs {expected}"
    );
}

// ─── TokenOps ───────────────────────────────────────────────────────────────

#[tokio::test]
#[serial]
async fn tokenops_dashboard_excludes_legacy_double_count() {
    let server = common::TestServer::start().await;
    let user_id = setup(&server, &["codex", "claude"]).await;
    let legacy = event("codex", "legacy", "turn-1");
    ingest(&server, user_id, &legacy).await;
    let (codex_session, codex_trace, delta) = rollup(&server, &legacy.event_id).await;
    assert!(delta > 0.0, "legacy rollup must carry a cost delta");
    let codex_cost = reported_chat_cost(&server, &codex_session).await;

    let claude = event("claude", "claude-s", "turn-1");
    ingest(&server, user_id, &claude).await;
    let (claude_session, claude_trace, _) = rollup(&server, &claude.event_id).await;

    seed_trace_usage(
        &server,
        &codex_trace,
        "codex",
        &codex_session,
        1000,
        codex_cost,
    )
    .await;
    seed_trace_usage(
        &server,
        &claude_trace,
        "claude",
        &claude_session,
        CLAUDE_INPUT,
        CLAUDE_COST,
    )
    .await;

    let data = dashboard(&server, user_id).await;
    assert_eq!(
        data["token_usage"]["prompt_tokens"],
        200 + CLAUDE_INPUT,
        "{data}"
    );
    assert_close(
        data["summary"]["total_cost"].as_f64().unwrap(),
        codex_cost + CLAUDE_COST - delta,
        "total_cost",
    );
    assert_eq!(data["usage_corrected"], true, "{data}");
    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn tokenops_without_legacy_rows_is_unchanged() {
    let server = common::TestServer::start().await;
    let user_id = setup(&server, &["codex"]).await;
    let mut marked = event("codex", "marked", "turn-1");
    marked.source.adapter_version = Some(CODEX_ADAPTER_VERSION_EXCLUSIVE_INPUT);
    ingest(&server, user_id, &marked).await;
    let (session, trace, delta) = rollup(&server, &marked.event_id).await;
    assert_close(delta, 0.0, "marked receipts carry no delta");
    seed_trace_usage(&server, &trace, "codex", &session, 1000, 0.5).await;

    let data = dashboard(&server, user_id).await;
    assert_eq!(data["token_usage"]["prompt_tokens"], 1000, "{data}");
    assert_close(
        data["summary"]["total_cost"].as_f64().unwrap(),
        0.5,
        "total_cost",
    );
    assert_eq!(data["usage_corrected"], false, "{data}");
    server.cleanup().await;
}

// ─── chat transcript ────────────────────────────────────────────────────────

#[tokio::test]
#[serial]
async fn chat_transcript_corrects_legacy_codex_message() {
    let server = common::TestServer::start().await;
    let user_id = setup(&server, &["codex", "claude"]).await;
    let legacy = event("codex", "legacy", "turn-1");
    ingest(&server, user_id, &legacy).await;
    let (codex_session, _, delta) = rollup(&server, &legacy.event_id).await;
    let reported_cost = reported_chat_cost(&server, &codex_session).await;

    let claude = event("claude", "claude-s", "turn-1");
    ingest(&server, user_id, &claude).await;
    let (claude_session, _, _) = rollup(&server, &claude.event_id).await;

    // Both transcript reads: the session read and the paged messages read the UI uses.
    for path in [
        format!("/api/chat/sessions/{codex_session}"),
        format!("/api/chat/sessions/{codex_session}/messages"),
    ] {
        let messages = get(&server, user_id, &path).await;
        let assistant = messages["data"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["role"] == "assistant")
            .expect("assistant message")
            .clone();
        assert_eq!(assistant["input_tokens"], 200, "{path}: {assistant}");
        assert_eq!(assistant["cache_read_tokens"], 800, "{assistant}");
        let cost: f64 = assistant["cost_usd"]
            .as_str()
            .map(|s| s.parse().unwrap())
            .or_else(|| assistant["cost_usd"].as_f64())
            .unwrap();
        assert_close(cost, reported_cost - delta, "corrected chat cost");
        assert_eq!(assistant["usage_corrected"], true, "{assistant}");
    }

    let messages = get(
        &server,
        user_id,
        &format!("/api/chat/sessions/{claude_session}"),
    )
    .await;
    for message in messages["data"].as_array().unwrap() {
        assert!(
            message.get("usage_corrected").is_none_or(|v| v == false),
            "{message}"
        );
        if message["role"] == "assistant" {
            assert_eq!(message["input_tokens"], 1000, "{message}");
        }
    }
    server.cleanup().await;
}

// ─── session overlay ────────────────────────────────────────────────────────

#[tokio::test]
#[serial]
async fn session_overlay_lists_scoped_traces_and_legacy_deltas() {
    let server = common::TestServer::start().await;
    let user_id = setup(&server, &["codex", "claude"]).await;
    let legacy = event("codex", "legacy", "turn-1");
    ingest(&server, user_id, &legacy).await;
    let (codex_session, codex_trace, delta) = rollup(&server, &legacy.event_id).await;

    let main = event("claude", "claude-s", "turn-1");
    let mut sub = event("claude", "claude-s", "agent-a1:1");
    sub.turn.agent_scope = Some(CodingAgentScope {
        kind: CodingAgentScopeKind::Subagent,
        agent_id: "a1".into(),
        agent_type: Some("Explore".into()),
        parent_tool_call_id: Some("toolu_parent".into()),
        parent_agent_id: None,
        spawn_depth: Some(1),
        description: None,
        name: None,
    });
    ingest(&server, user_id, &main).await;
    ingest(&server, user_id, &sub).await;
    let (claude_session, main_trace, _) = rollup(&server, &main.event_id).await;
    let (_, sub_trace, _) = rollup(&server, &sub.event_id).await;
    assert_ne!(main_trace, sub_trace);

    let overlay =
        codex_correction::load_for_sessions(&server.db, &[codex_session.clone(), claude_session])
            .await
            .unwrap();
    assert!(overlay.scoped_traces().contains(&sub_trace));
    assert!(!overlay.scoped_traces().contains(&main_trace));
    assert!(!overlay.scoped_traces().contains(&codex_trace));
    assert!(
        overlay.trace(&main_trace).is_none(),
        "claude is never corrected"
    );
    let codex = overlay.trace(&codex_trace).expect("legacy trace corrected");
    assert_eq!(codex.input_tokens, 800);
    assert_close(codex.cost_usd, delta, "trace cost delta");
    assert_eq!(overlay.session(&codex_session).unwrap().input_tokens, 800);

    let by_trace =
        codex_correction::load_for_traces(&server.db, &[codex_trace.clone(), main_trace])
            .await
            .unwrap();
    assert_eq!(by_trace.trace(&codex_trace).unwrap().input_tokens, 800);
    assert!(by_trace.scoped_traces().is_empty());
    server.cleanup().await;
}

// ─── immutability ───────────────────────────────────────────────────────────

#[tokio::test]
#[serial]
async fn receipts_and_trace_usage_untouched() {
    let server = common::TestServer::start().await;
    let user_id = setup(&server, &["codex"]).await;
    let legacy = event("codex", "legacy", "turn-1");
    ingest(&server, user_id, &legacy).await;
    let (session, trace, _) = rollup(&server, &legacy.event_id).await;
    seed_trace_usage(&server, &trace, "codex", &session, 1000, 0.5).await;
    let before = stored_snapshot(&server).await;

    dashboard(&server, user_id).await;
    get(&server, user_id, &format!("/api/chat/sessions/{session}")).await;
    for path in [
        "/api/observability/finops/spend-timeseries?range=7d".to_owned(),
        format!(
            "/api/observability/finops/spend-calendar?month={}",
            Utc::now().format("%Y-%m")
        ),
    ] {
        get(&server, user_id, &path).await;
    }

    assert_eq!(stored_snapshot(&server).await, before);
    let (input, cost): (i64, f64) =
        sqlx::query_as("SELECT input_tokens, cost_usd FROM trace_usage WHERE trace_id = $1")
            .bind(&trace)
            .fetch_one(&server.db)
            .await
            .unwrap();
    assert_eq!(input, 1000);
    assert_close(cost, 0.5, "stored trace_usage cost");
    server.cleanup().await;
}
