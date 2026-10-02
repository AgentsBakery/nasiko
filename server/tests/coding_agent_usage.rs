//! Coding-agent usage rollup (`coding_agent_turn_usage`) integration tests.
//!
//! Covers the ingest-time rollup (one row per newly accepted receipt, none for
//! duplicates or rejections), the legacy Codex inclusive-input correction, agent
//! scope attribution, the bounded history backfill, and the
//! `trace_usage_corrected` overlay view.

mod common;

use chrono::{Duration, TimeZone, Utc};
use nasiko_pricing::PricingEngine;
use nasiko_server::coding_agent_usage::{CODEX_INCLUSIVE_INPUT_CORRECTION, tick_usage_backfill};
use nasiko_types::{
    CODEX_ADAPTER_VERSION_EXCLUSIVE_INPUT, CODING_AGENT_EVENT_VERSION, CapturePolicy,
    CodingAgentEventV1, CodingAgentLlmCall, CodingAgentScope, CodingAgentScopeKind,
    CodingAgentSession, CodingAgentSource, CodingAgentTimestampQuality, CodingAgentToolAssociation,
    CodingAgentToolCall, CodingAgentToolCallStatus, CodingAgentTurn, coding_agent_event_id,
    coding_agent_session_id,
};
use rust_decimal::Decimal;
use serde_json::{Value, json};
use serial_test::serial;
use uuid::Uuid;

const AGENT_NAME: &str = "coding-agent";
const BACKFILL_LIMIT: i64 = 100;

// ─── helpers ────────────────────────────────────────────────────────────────

/// Initialise the admin and register one owned integration per source agent.
async fn setup(server: &common::TestServer, sources: &[&str]) -> Uuid {
    let admin: Value = server
        .client
        .post(server.url("/api/auth/initialize-admin"))
        .json(&json!({"username": "admin", "email": "admin@test.local"}))
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
        .bind(format!("{AGENT_NAME}-{source}"))
        .bind(user_id)
        .bind(source)
        .execute(&server.db)
        .await
        .unwrap();
    }
    user_id
}

fn event(source: &str, session: &str, turn: &str, policy: CapturePolicy) -> CodingAgentEventV1 {
    let started_at = Utc.timestamp_opt(1_700_000_000, 0).unwrap();
    let ended_at = Utc.timestamp_opt(1_700_000_002, 0).unwrap();
    let content = policy == CapturePolicy::Content;
    CodingAgentEventV1 {
        version: CODING_AGENT_EVENT_VERSION,
        event_id: coding_agent_event_id(source, session, turn),
        captured_at: ended_at,
        source: CodingAgentSource {
            agent_id: source.into(),
            agent_name: format!("{AGENT_NAME}-{source}"),
            adapter_version: None,
        },
        session: CodingAgentSession {
            id: coding_agent_session_id(source, session),
            source_id: session.into(),
            title: None,
        },
        turn: CodingAgentTurn {
            id: turn.into(),
            prompt: content.then(|| "question".into()),
            response: content.then(|| "answer".into()),
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
        capture_policy: policy,
    }
}

fn spawn_call(id: &str, arguments: Value) -> CodingAgentToolCall {
    CodingAgentToolCall {
        id: id.into(),
        name: "Agent".into(),
        kind: "function".into(),
        model_call_id: None,
        status: CodingAgentToolCallStatus::Succeeded,
        arguments: Some(arguments),
        output: Some(json!("subagent transcript")),
        raw: Some("raw".into()),
        error: None,
        started_at: None,
        ended_at: None,
        duration_ms: None,
        association: CodingAgentToolAssociation::Exact,
        timestamp_quality: CodingAgentTimestampQuality::Unknown,
    }
}

fn subagent_scope(description: Option<&str>) -> CodingAgentScope {
    CodingAgentScope {
        kind: CodingAgentScopeKind::Subagent,
        agent_id: "a1b2c3d4".into(),
        agent_type: Some("Explore".into()),
        parent_tool_call_id: Some("toolu_parent".into()),
        parent_agent_id: Some("parent-agent".into()),
        spawn_depth: Some(1),
        description: description.map(Into::into),
        name: None,
    }
}

async fn post(server: &common::TestServer, user_id: Uuid, events: &[CodingAgentEventV1]) -> Value {
    common::as_member(
        server
            .client
            .post(server.url("/api/telemetry/coding-agent/events/batch")),
        &user_id.to_string(),
        "admin",
    )
    .json(&json!({"events": events}))
    .send()
    .await
    .unwrap()
    .json()
    .await
    .unwrap()
}

async fn post_status(
    server: &common::TestServer,
    user_id: Uuid,
    event: &CodingAgentEventV1,
) -> String {
    let response = post(server, user_id, std::slice::from_ref(event)).await;
    response["data"]["results"][0]["status"]
        .as_str()
        .unwrap_or_else(|| panic!("no status in {response}"))
        .to_owned()
}

/// The rollup row as JSON, without the volatile `computed_at`.
async fn rollup_row(server: &common::TestServer, event_id: &str) -> Option<Value> {
    sqlx::query_scalar(
        "SELECT to_jsonb(u) - 'computed_at' FROM coding_agent_turn_usage u WHERE event_id = $1",
    )
    .bind(event_id)
    .fetch_optional(&server.db)
    .await
    .unwrap()
}

async fn rollup_count(server: &common::TestServer) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM coding_agent_turn_usage")
        .fetch_one(&server.db)
        .await
        .unwrap()
}

fn decimal(value: &Value) -> Decimal {
    let text = value
        .as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| value.to_string());
    text.parse()
        .unwrap_or_else(|_| panic!("not a decimal: {value}"))
}

// ─── ingest ─────────────────────────────────────────────────────────────────

#[tokio::test]
#[serial]
async fn legacy_codex_ingest_stores_corrected_rollup() {
    let server = common::TestServer::start().await;
    let user_id = setup(&server, &["codex"]).await;
    let legacy = event("codex", "legacy", "turn-1", CapturePolicy::Content);
    assert_eq!(post_status(&server, user_id, &legacy).await, "accepted");

    let row = rollup_row(&server, &legacy.event_id).await.unwrap();
    assert_eq!(row["input_tokens"], 200);
    assert_eq!(row["reported_input_tokens"], 1000);
    assert_eq!(row["cache_read_tokens"], 800);
    assert_eq!(row["correction"], CODEX_INCLUSIVE_INPUT_CORRECTION);
    assert_eq!(row["agent_kind"], "main");
    assert_eq!(row["source_agent_id"], "codex");
    assert_eq!(row["adapter_version"], Value::Null);
    assert!(
        decimal(&row["cost_usd"]) < decimal(&row["reported_cost_usd"]),
        "{row}"
    );
    // The chat transcript keeps the reported figure; readers correct it.
    let chat_cost: Option<Decimal> = sqlx::query_scalar(
        "SELECT cost_usd FROM chat_messages WHERE role = 'assistant' AND session_id = $1",
    )
    .bind(row["session_id"].as_str().unwrap())
    .fetch_one(&server.db)
    .await
    .unwrap();
    assert_eq!(chat_cost, Some(decimal(&row["reported_cost_usd"])));
    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn marked_codex_ingest_is_not_corrected() {
    let server = common::TestServer::start().await;
    let user_id = setup(&server, &["codex"]).await;
    let mut marked = event("codex", "marked", "turn-1", CapturePolicy::MetadataOnly);
    marked.source.adapter_version = Some(CODEX_ADAPTER_VERSION_EXCLUSIVE_INPUT);
    assert_eq!(post_status(&server, user_id, &marked).await, "accepted");

    let row = rollup_row(&server, &marked.event_id).await.unwrap();
    assert_eq!(row["input_tokens"], 1000);
    assert_eq!(row["reported_input_tokens"], 1000);
    assert_eq!(row["correction"], Value::Null);
    assert_eq!(
        row["adapter_version"],
        CODEX_ADAPTER_VERSION_EXCLUSIVE_INPUT
    );
    assert_eq!(row["capture_policy"], "metadata_only");
    // Metadata-only receipts are priced too.
    assert_eq!(row["cost_usd"], row["reported_cost_usd"]);
    assert!(!row["cost_usd"].is_null(), "{row}");
    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn claude_main_and_subagent_rows() {
    let server = common::TestServer::start().await;
    let user_id = setup(&server, &["claude"]).await;
    let main = event("claude", "s", "turn-1", CapturePolicy::Content);
    let mut metadata = event(
        "claude",
        "s",
        "agent-a1b2c3d4:1",
        CapturePolicy::MetadataOnly,
    );
    metadata.turn.agent_scope = Some(subagent_scope(None));
    let mut content = event("claude", "s", "agent-a1b2c3d4:2", CapturePolicy::Content);
    content.turn.agent_scope = Some(subagent_scope(Some("Find the config loader")));
    for event in [&main, &metadata, &content] {
        assert_eq!(post_status(&server, user_id, event).await, "accepted");
    }

    let main_row = rollup_row(&server, &main.event_id).await.unwrap();
    assert_eq!(main_row["agent_kind"], "main");
    assert_eq!(main_row["scope_agent_id"], Value::Null);
    assert_eq!(main_row["correction"], Value::Null);
    assert_eq!(main_row["input_tokens"], 1000);

    let metadata_row = rollup_row(&server, &metadata.event_id).await.unwrap();
    assert_eq!(metadata_row["agent_kind"], "subagent");
    assert_eq!(metadata_row["scope_agent_id"], "a1b2c3d4");
    assert_eq!(metadata_row["agent_type"], "Explore");
    assert_eq!(metadata_row["parent_tool_call_id"], "toolu_parent");
    assert_eq!(metadata_row["parent_agent_id"], "parent-agent");
    assert_eq!(metadata_row["spawn_depth"], 1);
    assert_eq!(metadata_row["description"], Value::Null);

    let content_row = rollup_row(&server, &content.event_id).await.unwrap();
    assert_eq!(content_row["agent_kind"], "subagent");
    assert_eq!(content_row["description"], "Find the config loader");

    // Every row belongs to the same server session; subagent rows are separate.
    let sessions: Vec<String> =
        sqlx::query_scalar("SELECT DISTINCT session_id FROM coding_agent_turn_usage")
            .fetch_all(&server.db)
            .await
            .unwrap();
    assert_eq!(sessions.len(), 1);
    assert_eq!(rollup_count(&server).await, 3);
    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn duplicate_and_rejected_add_no_rows() {
    let server = common::TestServer::start().await;
    let user_id = setup(&server, &["claude"]).await;
    let first = event("claude", "s", "turn-1", CapturePolicy::Content);
    assert_eq!(post_status(&server, user_id, &first).await, "accepted");
    assert_eq!(post_status(&server, user_id, &first).await, "duplicate");
    assert_eq!(rollup_count(&server).await, 1);

    let mut conflict = first.clone();
    conflict.turn.llm_calls[0].output_tokens = 999;
    assert_eq!(post_status(&server, user_id, &conflict).await, "rejected");
    let mut unknown_integration = event("claude", "s", "turn-2", CapturePolicy::Content);
    unknown_integration.source.agent_name = "not-registered".into();
    assert_eq!(
        post_status(&server, user_id, &unknown_integration).await,
        "rejected"
    );
    assert_eq!(rollup_count(&server).await, 1);
    let first_row = rollup_row(&server, &first.event_id).await.unwrap();
    assert_eq!(first_row["output_tokens"], 50);
    server.cleanup().await;
}

// ─── backfill ───────────────────────────────────────────────────────────────

#[tokio::test]
#[serial]
async fn backfill_fills_pre_0050_receipts_idempotently() {
    let server = common::TestServer::start().await;
    let user_id = setup(&server, &["claude", "codex"]).await;
    let mut main = event("claude", "s", "turn-1", CapturePolicy::Content);
    main.turn.tool_calls = vec![
        spawn_call(
            "toolu_plain",
            json!({"subagent_type": "Explore", "prompt": "secret"}),
        ),
        spawn_call("toolu_named", json!({"name": "scout", "prompt": "secret"})),
    ];
    let mut scoped = event("claude", "s", "agent-a1b2c3d4:1", CapturePolicy::Content);
    scoped.turn.agent_scope = Some(subagent_scope(Some("Find the config loader")));
    let legacy = event("codex", "c", "turn-1", CapturePolicy::MetadataOnly);
    let events = [&main, &scoped, &legacy];
    for event in events {
        assert_eq!(post_status(&server, user_id, event).await, "accepted");
    }
    let mut ingested = Vec::new();
    for event in events {
        ingested.push(rollup_row(&server, &event.event_id).await.unwrap());
    }
    assert_eq!(
        ingested[0]["spawned_agent_call_ids"],
        json!(["toolu_plain"])
    );
    assert_eq!(ingested[0]["named_agent_call_ids"], json!(["toolu_named"]));
    let payloads_before: Vec<String> = sqlx::query_scalar(
        "SELECT payload::text FROM coding_agent_telemetry_events ORDER BY event_id",
    )
    .fetch_all(&server.db)
    .await
    .unwrap();

    // Simulate history stored before migration 0050.
    sqlx::query("DELETE FROM coding_agent_turn_usage")
        .execute(&server.db)
        .await
        .unwrap();
    let pricing = PricingEngine::new(server.db.clone());
    let filled = tick_usage_backfill(&server.db, &pricing, BACKFILL_LIMIT, Utc::now())
        .await
        .unwrap();
    assert_eq!(filled, 3);
    for (event, expected) in events.iter().zip(&ingested) {
        let row = rollup_row(&server, &event.event_id).await.unwrap();
        assert_eq!(
            &row, expected,
            "backfilled row differs for {}",
            event.event_id
        );
    }

    let again = tick_usage_backfill(&server.db, &pricing, BACKFILL_LIMIT, Utc::now())
        .await
        .unwrap();
    assert_eq!(again, 0);
    assert_eq!(rollup_count(&server).await, 3);
    let payloads_after: Vec<String> = sqlx::query_scalar(
        "SELECT payload::text FROM coding_agent_telemetry_events ORDER BY event_id",
    )
    .fetch_all(&server.db)
    .await
    .unwrap();
    assert_eq!(payloads_before, payloads_after);
    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn backfill_skips_undecodable_rows() {
    let server = common::TestServer::start().await;
    let user_id = setup(&server, &["claude"]).await;
    let good = event("claude", "s", "turn-1", CapturePolicy::MetadataOnly);
    assert_eq!(post_status(&server, user_id, &good).await, "accepted");
    sqlx::query("DELETE FROM coding_agent_turn_usage")
        .execute(&server.db)
        .await
        .unwrap();
    // An older receipt whose payload no longer decodes, sharing the good one's
    // agent and session so every foreign key holds.
    sqlx::query(
        r#"INSERT INTO coding_agent_telemetry_events
             (user_id, event_id, payload, agent_id, agent_name, source_agent_id,
              session_id, source_session_id, turn_id, captured_at, received_at)
           SELECT user_id, 'undecodable', '{"version":1}'::jsonb, agent_id, agent_name,
                  source_agent_id, session_id, source_session_id, 'bad-turn',
                  captured_at, received_at - interval '1 hour'
           FROM coding_agent_telemetry_events WHERE event_id = $1"#,
    )
    .bind(&good.event_id)
    .execute(&server.db)
    .await
    .unwrap();

    let pricing = PricingEngine::new(server.db.clone());
    let filled = tick_usage_backfill(&server.db, &pricing, BACKFILL_LIMIT, Utc::now())
        .await
        .unwrap();
    assert_eq!(filled, 1);
    assert!(rollup_row(&server, &good.event_id).await.is_some());
    assert!(rollup_row(&server, "undecodable").await.is_none());
    let failures: Vec<String> =
        sqlx::query_scalar("SELECT event_id FROM coding_agent_usage_backfill_failures")
            .fetch_all(&server.db)
            .await
            .unwrap();
    assert_eq!(failures, vec!["undecodable"]);

    let again = tick_usage_backfill(&server.db, &pricing, BACKFILL_LIMIT, Utc::now())
        .await
        .unwrap();
    assert_eq!(again, 0);
    assert!(rollup_row(&server, "undecodable").await.is_none());
    let failed_once: i64 =
        sqlx::query_scalar("SELECT count(*) FROM coding_agent_usage_backfill_failures")
            .fetch_one(&server.db)
            .await
            .unwrap();
    assert_eq!(failed_once, 1);
    server.cleanup().await;
}

// ─── overlay view ───────────────────────────────────────────────────────────

#[tokio::test]
#[serial]
async fn trace_usage_corrected_overlay() {
    let server = common::TestServer::start().await;
    let user_id = setup(&server, &["codex"]).await;
    let legacy = event("codex", "legacy", "turn-1", CapturePolicy::MetadataOnly);
    assert_eq!(post_status(&server, user_id, &legacy).await, "accepted");
    let row = rollup_row(&server, &legacy.event_id).await.unwrap();
    let trace_id = row["trace_id"].as_str().unwrap().to_owned();
    let cost_delta = decimal(&row["reported_cost_usd"]) - decimal(&row["cost_usd"]);
    assert!(cost_delta > Decimal::ZERO);

    let started_at = Utc::now() - Duration::minutes(5);
    for (trace, input, cost) in [
        (trace_id.as_str(), 1000_i64, 0.5_f64),
        ("unrelated", 700, 0.25),
    ] {
        sqlx::query(
            r#"INSERT INTO trace_usage
                 (trace_id, agent_name, session_id, input_tokens, output_tokens,
                  cache_read_tokens, cost_usd, prompt_cost_usd, completion_cost_usd,
                  started_at, tool_call_count, cost_estimated)
               VALUES ($1, $2, $3, $4, 50, 800, $5, $5 - 0.1, 0.1, $6, 2, false)"#,
        )
        .bind(trace)
        .bind(&legacy.source.agent_name)
        .bind(row["session_id"].as_str().unwrap())
        .bind(input)
        .bind(cost)
        .bind(started_at)
        .execute(&server.db)
        .await
        .unwrap();
    }

    let (input, cost, prompt_cost, corrected): (i64, f64, f64, bool) = sqlx::query_as(
        "SELECT input_tokens, cost_usd, prompt_cost_usd, usage_corrected \
         FROM trace_usage_corrected WHERE trace_id = $1",
    )
    .bind(&trace_id)
    .fetch_one(&server.db)
    .await
    .unwrap();
    let delta: f64 = cost_delta.to_string().parse().unwrap();
    assert_eq!(input, 200);
    assert!((cost - (0.5 - delta)).abs() < 1e-9, "{cost} vs {delta}");
    assert!((prompt_cost - (0.4 - delta)).abs() < 1e-9, "{prompt_cost}");
    assert!(corrected);

    let (view_row, base_row, unrelated_corrected): (Value, Value, bool) = sqlx::query_as(
        "SELECT to_jsonb(v) - 'usage_corrected', to_jsonb(t), v.usage_corrected \
         FROM trace_usage_corrected v JOIN trace_usage t USING (trace_id, agent_name) \
         WHERE trace_id = 'unrelated'",
    )
    .fetch_one(&server.db)
    .await
    .unwrap();
    assert_eq!(view_row, base_row);
    assert!(!unrelated_corrected);
    server.cleanup().await;
}
