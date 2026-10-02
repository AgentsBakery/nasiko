//! Per-agent coding-session breakdown (`GET /api/coding-sessions/{id}/agents`)
//! integration tests.
//!
//! Covers grouping of receipts into one main row plus one row per subagent or
//! teammate (across resumed runs), shares and totals, the captured / partial /
//! not-captured status for subagents and teams with its reason, intent hiding
//! for metadata-only receipts, and the session-detail access rule (owner or
//! superuser; everyone else gets the same 404 as a missing session). Receipts
//! are seeded through the real ingest endpoint so rows come from the rollup
//! written at ingest.

mod common;

use chrono::{Duration, TimeZone, Utc};
use nasiko_types::{
    CLAUDE_ADAPTER_VERSION_SUBAGENTS, CODEX_ADAPTER_VERSION_EXCLUSIVE_INPUT,
    CODING_AGENT_EVENT_VERSION, CapturePolicy, CodingAgentCallAccounting, CodingAgentEventV1,
    CodingAgentLlmCall, CodingAgentScope, CodingAgentScopeKind, CodingAgentSession,
    CodingAgentSource, CodingAgentTimestampQuality, CodingAgentToolAssociation,
    CodingAgentToolCall, CodingAgentToolCallStatus, CodingAgentTurn, coding_agent_event_id,
    coding_agent_session_id,
};
use reqwest::StatusCode;
use serde_json::{Value, json};
use serial_test::serial;
use uuid::Uuid;

const AGENT_NAME: &str = "coding-agent";
const SHARE_TOLERANCE: f64 = 0.001;
const MARKED: Option<u32> = Some(CLAUDE_ADAPTER_VERSION_SUBAGENTS);

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

/// One receipt; `offset_secs` orders turns in time.
fn event(
    source: &str,
    session: &str,
    turn: &str,
    policy: CapturePolicy,
    adapter_version: Option<u32>,
    offset_secs: i64,
) -> CodingAgentEventV1 {
    let started_at = Utc.timestamp_opt(1_700_000_000, 0).unwrap() + Duration::seconds(offset_secs);
    let ended_at = started_at + Duration::seconds(2);
    let content = policy == CapturePolicy::Content;
    CodingAgentEventV1 {
        version: CODING_AGENT_EVENT_VERSION,
        event_id: coding_agent_event_id(source, session, turn),
        captured_at: ended_at,
        source: CodingAgentSource {
            agent_id: source.into(),
            agent_name: format!("{AGENT_NAME}-{source}"),
            adapter_version,
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
                cache_creation_tokens: 10,
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

fn main_turn(
    session: &str,
    turn: &str,
    adapter_version: Option<u32>,
    at: i64,
) -> CodingAgentEventV1 {
    event(
        "claude",
        session,
        turn,
        CapturePolicy::Content,
        adapter_version,
        at,
    )
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
        association: CodingAgentToolAssociation::Turn,
        timestamp_quality: CodingAgentTimestampQuality::Unknown,
    }
}

fn unnamed_spawn(id: &str) -> CodingAgentToolCall {
    spawn_call(id, json!({"subagent_type": "Explore", "prompt": "look"}))
}

fn scope(
    kind: CodingAgentScopeKind,
    agent_id: &str,
    parent_tool_call_id: Option<&str>,
    description: Option<&str>,
    name: Option<&str>,
) -> CodingAgentScope {
    CodingAgentScope {
        kind,
        agent_id: agent_id.into(),
        agent_type: Some("Explore".into()),
        parent_tool_call_id: parent_tool_call_id.map(Into::into),
        parent_agent_id: None,
        spawn_depth: Some(1),
        description: description.map(Into::into),
        name: name.map(Into::into),
    }
}

/// A scoped receipt (subagent or teammate run).
fn scoped_turn(
    session: &str,
    turn: &str,
    policy: CapturePolicy,
    agent_scope: CodingAgentScope,
    at: i64,
) -> CodingAgentEventV1 {
    let mut scoped = event("claude", session, turn, policy, MARKED, at);
    scoped.turn.agent_scope = Some(agent_scope);
    scoped
}

async fn ingest(server: &common::TestServer, user_id: Uuid, events: &[&CodingAgentEventV1]) {
    for event in events {
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
}

/// Server-side session id the receipt was stored under.
async fn server_session(server: &common::TestServer, event: &CodingAgentEventV1) -> String {
    sqlx::query_scalar("SELECT session_id FROM coding_agent_telemetry_events WHERE event_id = $1")
        .bind(&event.event_id)
        .fetch_one(&server.db)
        .await
        .unwrap()
}

fn agents_url(server: &common::TestServer, session_id: &str) -> String {
    server.url(&format!("/api/coding-sessions/{session_id}/agents"))
}

async fn fetch_raw(request: reqwest::RequestBuilder) -> (StatusCode, String) {
    let response = request.send().await.unwrap();
    (response.status(), response.text().await.unwrap())
}

/// Breakdown as the owner; asserts 200 and returns `data`.
async fn breakdown(server: &common::TestServer, owner: Uuid, session_id: &str) -> Value {
    let request = common::as_member(
        server.client.get(agents_url(server, session_id)),
        &owner.to_string(),
        "admin",
    );
    let (status, body) = fetch_raw(request).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let body: Value = serde_json::from_str(&body).unwrap();
    body["data"].clone()
}

fn agents(data: &Value) -> &Vec<Value> {
    data["agents"].as_array().expect("agents array")
}

fn assert_shares_sum_to_one(data: &Value) {
    let total: f64 = agents(data)
        .iter()
        .map(|row| row["share"].as_f64().unwrap())
        .sum();
    assert!((total - 1.0).abs() < SHARE_TOLERANCE, "shares sum {total}");
}

fn assert_totals_equal_row_sums(data: &Value) {
    let rows = agents(data);
    for field in ["llm_calls", "tool_calls"] {
        let sum: i64 = rows.iter().map(|row| row[field].as_i64().unwrap()).sum();
        assert_eq!(data["totals"][field].as_i64().unwrap(), sum, "{field}");
    }
    for class in ["input", "output", "cache_read", "cache_creation"] {
        let sum: i64 = rows
            .iter()
            .map(|row| row["tokens"][class].as_i64().unwrap())
            .sum();
        assert_eq!(
            data["totals"]["tokens"][class].as_i64().unwrap(),
            sum,
            "{class}"
        );
    }
}

// ─── grouping ───────────────────────────────────────────────────────────────

#[tokio::test]
#[serial]
async fn owner_sees_main_and_subagent_rows() {
    let server = common::TestServer::start().await;
    let owner = setup(&server, &["claude"]).await;
    let mut main_1 = main_turn("s", "turn-1", MARKED, 0);
    main_1.turn.tool_calls = vec![unnamed_spawn("toolu_A")];
    let main_2 = main_turn("s", "turn-2", MARKED, 30);
    let sub_scope = || {
        scope(
            CodingAgentScopeKind::Subagent,
            "a1",
            Some("toolu_A"),
            None,
            None,
        )
    };
    let sub_1 = scoped_turn("s", "agent-a1:1", CapturePolicy::Content, sub_scope(), 5);
    let sub_2 = scoped_turn("s", "agent-a1:2", CapturePolicy::Content, sub_scope(), 40);
    ingest(&server, owner, &[&main_1, &main_2, &sub_1, &sub_2]).await;
    let session_id = server_session(&server, &main_1).await;

    let data = breakdown(&server, owner, &session_id).await;
    assert_eq!(data["session_id"], session_id.as_str());
    assert_eq!(data["source_agent_id"], "claude");
    let rows = agents(&data);
    assert_eq!(rows.len(), 2, "{data}");
    assert_eq!(rows[0]["key"], "main");
    assert_eq!(rows[0]["kind"], "main");
    assert_eq!(rows[0]["agent_id"], Value::Null);
    assert_eq!(rows[0]["runs"], 2);
    assert_eq!(rows[1]["key"], "subagent:a1");
    assert_eq!(rows[1]["kind"], "subagent");
    assert_eq!(rows[1]["agent_id"], "a1");
    assert_eq!(rows[1]["parent_tool_call_id"], "toolu_A");
    assert_eq!(rows[1]["runs"], 2);
    assert_eq!(rows[1]["llm_calls"], 2);
    assert_eq!(data["totals"]["llm_calls"], 4);
    assert_eq!(data["share_basis"], "cost");
    assert!(data["totals"]["cost_usd"].as_f64().unwrap() > 0.0);
    assert_eq!(data["totals"]["output_incomplete"], false);
    assert_eq!(data["totals"]["usage_corrected"], false);
    assert_totals_equal_row_sums(&data);
    assert_shares_sum_to_one(&data);
    assert_eq!(
        data["capture"]["subagents"],
        json!({"status": "captured", "reason": null, "spawned": 1, "captured": 1})
    );
    assert_eq!(
        data["capture"]["teams"],
        json!({"status": "not_captured", "reason": "agent_teams_unsupported"})
    );
    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn teammate_rows_mark_teams_captured() {
    let server = common::TestServer::start().await;
    let owner = setup(&server, &["claude"]).await;
    let main = main_turn("s", "turn-1", MARKED, 0);
    let mate = scoped_turn(
        "s",
        "agent-t1:1",
        CapturePolicy::Content,
        scope(
            CodingAgentScopeKind::Teammate,
            "t1",
            None,
            Some("own the parser"),
            Some("scout"),
        ),
        10,
    );
    ingest(&server, owner, &[&main, &mate]).await;
    let session_id = server_session(&server, &main).await;

    let data = breakdown(&server, owner, &session_id).await;
    let rows = agents(&data);
    assert_eq!(rows.len(), 2, "{data}");
    assert_eq!(rows[1]["key"], "teammate:t1");
    assert_eq!(rows[1]["kind"], "teammate");
    assert_eq!(rows[1]["name"], "scout");
    assert_eq!(
        data["capture"]["teams"],
        json!({"status": "captured", "reason": null})
    );
    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn metadata_only_hides_intent() {
    let server = common::TestServer::start().await;
    let owner = setup(&server, &["claude"]).await;
    let main = main_turn("s", "turn-1", MARKED, 0);
    let hidden = scoped_turn(
        "s",
        "agent-m1:1",
        CapturePolicy::MetadataOnly,
        scope(CodingAgentScopeKind::Subagent, "m1", None, None, None),
        10,
    );
    let shown = scoped_turn(
        "s",
        "agent-c1:1",
        CapturePolicy::Content,
        scope(
            CodingAgentScopeKind::Subagent,
            "c1",
            None,
            Some("find X"),
            None,
        ),
        20,
    );
    ingest(&server, owner, &[&main, &hidden, &shown]).await;
    let session_id = server_session(&server, &main).await;

    let data = breakdown(&server, owner, &session_id).await;
    let row = |key: &str| {
        agents(&data)
            .iter()
            .find(|row| row["key"] == key)
            .unwrap_or_else(|| panic!("no row {key} in {data}"))
            .clone()
    };
    let hidden_row = row("subagent:m1");
    assert_eq!(hidden_row["intent"], "Explore");
    assert_eq!(hidden_row["intent_hidden"], true);
    assert_eq!(hidden_row["name"], Value::Null);
    assert_eq!(hidden_row["agent_type"], "Explore");
    let shown_row = row("subagent:c1");
    assert_eq!(shown_row["intent"], "find X");
    assert_eq!(shown_row["intent_hidden"], false);
    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn output_incomplete_and_unpriced_propagate() {
    let server = common::TestServer::start().await;
    let owner = setup(&server, &["claude"]).await;
    let mut main = main_turn("s", "turn-1", MARKED, 0);
    main.turn.llm_calls[0].accounting = Some(CodingAgentCallAccounting {
        version: 2,
        output_tokens_final: Some(false),
        ..Default::default()
    });
    let mut idle = scoped_turn(
        "s",
        "agent-i1:1",
        CapturePolicy::Content,
        scope(CodingAgentScopeKind::Subagent, "i1", None, None, None),
        10,
    );
    idle.turn.llm_calls.clear();
    ingest(&server, owner, &[&main, &idle]).await;
    let session_id = server_session(&server, &main).await;

    let data = breakdown(&server, owner, &session_id).await;
    assert_eq!(data["totals"]["output_incomplete"], true);
    assert_eq!(agents(&data)[0]["output_incomplete"], true);
    assert_eq!(data["share_basis"], "tokens");
    let idle_row = agents(&data)
        .iter()
        .find(|row| row["key"] == "subagent:i1")
        .unwrap();
    assert_eq!(idle_row["cost_usd"], Value::Null);
    assert_eq!(idle_row["share"].as_f64().unwrap(), 0.0);
    assert_shares_sum_to_one(&data);
    assert_totals_equal_row_sums(&data);
    server.cleanup().await;
}

// ─── capture status ─────────────────────────────────────────────────────────

#[tokio::test]
#[serial]
async fn partial_when_spawned_agent_missing() {
    let server = common::TestServer::start().await;
    let owner = setup(&server, &["claude"]).await;
    let mut main = main_turn("s", "turn-1", MARKED, 0);
    main.turn.tool_calls = vec![unnamed_spawn("toolu_A"), unnamed_spawn("toolu_B")];
    let sub = scoped_turn(
        "s",
        "agent-a1:1",
        CapturePolicy::Content,
        scope(
            CodingAgentScopeKind::Subagent,
            "a1",
            Some("toolu_A"),
            None,
            None,
        ),
        5,
    );
    ingest(&server, owner, &[&main, &sub]).await;
    let session_id = server_session(&server, &main).await;

    let data = breakdown(&server, owner, &session_id).await;
    assert_eq!(
        data["capture"]["subagents"],
        json!({"status": "partial", "reason": null, "spawned": 2, "captured": 1})
    );
    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn mixed_marker_session_is_partial_cli_version_mixed() {
    let server = common::TestServer::start().await;
    let owner = setup(&server, &["claude"]).await;
    // First Stop ran before the capability cache existed: no marker.
    let first = main_turn("s", "turn-1", None, 0);
    let mut later = main_turn("s", "turn-2", MARKED, 30);
    later.turn.tool_calls = vec![unnamed_spawn("toolu_C")];
    let sub = scoped_turn(
        "s",
        "agent-c3:1",
        CapturePolicy::Content,
        scope(
            CodingAgentScopeKind::Subagent,
            "c3",
            Some("toolu_C"),
            None,
            None,
        ),
        35,
    );
    ingest(&server, owner, &[&first, &later, &sub]).await;
    let session_id = server_session(&server, &first).await;

    let data = breakdown(&server, owner, &session_id).await;
    assert_eq!(
        data["capture"]["subagents"],
        json!({"status": "partial", "reason": "cli_version_mixed", "spawned": 1, "captured": 1})
    );
    assert!(
        agents(&data).iter().any(|row| row["key"] == "subagent:c3"),
        "{data}"
    );
    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn named_spawns_do_not_force_partial() {
    let server = common::TestServer::start().await;
    let owner = setup(&server, &["claude"]).await;
    // Unlinked named spawn: possibly an uncaptured teammate, not a missing subagent.
    let mut lone = main_turn("lone", "turn-1", MARKED, 0);
    lone.turn.tool_calls = vec![spawn_call("toolu_N", json!({"name": "scout"}))];
    // Linked named spawn: the teammate it started reported back.
    let mut lead = main_turn("lead", "turn-1", MARKED, 0);
    lead.turn.tool_calls = vec![spawn_call("toolu_M", json!({"name": "scout"}))];
    let mate = scoped_turn(
        "lead",
        "agent-t2:1",
        CapturePolicy::Content,
        scope(
            CodingAgentScopeKind::Teammate,
            "t2",
            Some("toolu_M"),
            None,
            Some("scout"),
        ),
        5,
    );
    ingest(&server, owner, &[&lone, &lead, &mate]).await;

    let lone_session = server_session(&server, &lone).await;
    let data = breakdown(&server, owner, &lone_session).await;
    assert_eq!(
        data["capture"]["subagents"],
        json!({"status": "captured", "reason": null, "spawned": 0, "captured": 0})
    );

    let lead_session = server_session(&server, &lead).await;
    let data = breakdown(&server, owner, &lead_session).await;
    assert_eq!(
        data["capture"]["subagents"],
        json!({"status": "captured", "reason": null, "spawned": 1, "captured": 1})
    );
    assert_eq!(data["capture"]["teams"]["status"], "captured");
    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn old_cli_session_is_main_only_not_captured() {
    let server = common::TestServer::start().await;
    let owner = setup(&server, &["claude"]).await;
    let mut first = main_turn("s", "turn-1", None, 0);
    first.turn.tool_calls = vec![unnamed_spawn("toolu_old")];
    let second = main_turn("s", "turn-2", None, 30);
    ingest(&server, owner, &[&first, &second]).await;
    let session_id = server_session(&server, &first).await;

    let data = breakdown(&server, owner, &session_id).await;
    let rows = agents(&data);
    assert_eq!(rows.len(), 1, "{data}");
    assert_eq!(rows[0]["key"], "main");
    assert_eq!(rows[0]["runs"], 2);
    assert!((rows[0]["share"].as_f64().unwrap() - 1.0).abs() < SHARE_TOLERANCE);
    assert_eq!(
        data["capture"]["subagents"],
        json!({"status": "not_captured", "reason": "cli_version", "spawned": null, "captured": null})
    );
    assert_eq!(
        data["capture"]["teams"],
        json!({"status": "not_captured", "reason": "agent_teams_unsupported"})
    );
    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn codex_session_not_applicable() {
    let server = common::TestServer::start().await;
    let owner = setup(&server, &["codex"]).await;
    let legacy = event("codex", "c", "turn-1", CapturePolicy::Content, None, 0);
    let marked = event(
        "codex",
        "c",
        "turn-2",
        CapturePolicy::Content,
        Some(CODEX_ADAPTER_VERSION_EXCLUSIVE_INPUT),
        30,
    );
    ingest(&server, owner, &[&legacy, &marked]).await;
    let session_id = server_session(&server, &legacy).await;

    let data = breakdown(&server, owner, &session_id).await;
    assert_eq!(data["source_agent_id"], "codex");
    assert_eq!(agents(&data).len(), 1);
    // The legacy receipt is corrected (inclusive input minus cache reads).
    assert_eq!(data["totals"]["usage_corrected"], true);
    assert_eq!(data["totals"]["tokens"]["input"], 200 + 1000);
    assert_eq!(
        data["capture"]["subagents"],
        json!({"status": "not_applicable", "reason": "agent_unsupported", "spawned": null, "captured": null})
    );
    assert_eq!(
        data["capture"]["teams"],
        json!({"status": "not_applicable", "reason": "agent_unsupported"})
    );
    server.cleanup().await;
}

// ─── access ─────────────────────────────────────────────────────────────────

#[tokio::test]
#[serial]
async fn other_user_gets_identical_404() {
    let server = common::TestServer::start().await;
    let owner = setup(&server, &["claude"]).await;
    let main = main_turn("s", "turn-1", MARKED, 0);
    ingest(&server, owner, &[&main]).await;
    let session_id = server_session(&server, &main).await;
    let other = Uuid::new_v4().to_string();

    let (status, not_yours) = fetch_raw(common::as_member(
        server.client.get(agents_url(&server, &session_id)),
        &other,
        "other-user",
    ))
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (missing_status, missing) = fetch_raw(common::as_member(
        server
            .client
            .get(agents_url(&server, &Uuid::new_v4().to_string())),
        &other,
        "other-user",
    ))
    .await;
    assert_eq!(missing_status, StatusCode::NOT_FOUND);
    assert_eq!(not_yours, missing);
    let body: Value = serde_json::from_str(&not_yours).unwrap();
    assert_eq!(
        body,
        json!({"error": "session not found", "code": "not_found"})
    );
    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn superuser_sees_any_session() {
    let server = common::TestServer::start().await;
    let owner = setup(&server, &["claude"]).await;
    let main = main_turn("s", "turn-1", MARKED, 0);
    ingest(&server, owner, &[&main]).await;
    let session_id = server_session(&server, &main).await;

    let (status, body) = fetch_raw(common::as_superuser(
        server.client.get(agents_url(&server, &session_id)),
        &Uuid::new_v4().to_string(),
        "root",
    ))
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let body: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(body["data"]["agents"][0]["key"], "main");
    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn unauthenticated_is_401() {
    let server = common::TestServer::start().await;
    let (status, _) = fetch_raw(
        server
            .client
            .get(agents_url(&server, &Uuid::new_v4().to_string())),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn works_without_observability() {
    // The default test config has no Tempo/Loki: the breakdown is Postgres only.
    let server = common::TestServer::start().await;
    let owner = setup(&server, &["claude"]).await;
    let main = main_turn("s", "turn-1", MARKED, 0);
    ingest(&server, owner, &[&main]).await;
    let session_id = server_session(&server, &main).await;

    let data = breakdown(&server, owner, &session_id).await;
    assert_eq!(agents(&data).len(), 1);
    assert_eq!(data["totals"]["llm_calls"], 1);
    server.cleanup().await;
}
