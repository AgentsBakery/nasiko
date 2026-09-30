//! RBAC regression tests for the FinOps read endpoints.
//!
//! Covers, for a non-superuser with no grants, an owner, an explicit grantee
//! and a superuser:
//!   GET /api/observability/finops/dashboard
//!   GET /api/observability/finops/spend-timeseries
//!   GET /api/observability/finops/spend-calendar
//!   GET /api/observability/finops/spend-calendar/day
//!   GET /api/observability/finops/attributions
//!
//! Spend rows are seeded straight into `trace_usage` (and `maf_executions` for
//! the workflow view), so the tests need no Tempo/Loki.
//!
//! Requires infra (Postgres :5432, Redis, MinIO):
//!   cargo test -p nasiko-server --test finops_rbac -- --test-threads=1

mod common;

use serde_json::{Value, json};
use serial_test::serial;
use uuid::Uuid;

const EPS: f64 = 1e-6;
const COST_A: f64 = 1.25;
const COST_B: f64 = 4.5;

// ─── shared helpers ──────────────────────────────────────────────────────────

async fn init_admin(server: &common::TestServer) -> Value {
    server
        .client
        .post(server.url("/api/auth/initialize-admin"))
        .json(&json!({"username": "admin", "email": "admin@finops-rbac.test"}))
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap()
}

async fn seed_user(server: &common::TestServer, username: &str) -> Uuid {
    sqlx::query_scalar("INSERT INTO users (username, email) VALUES ($1, $2) RETURNING id")
        .bind(username)
        .bind(format!("{username}@finops-rbac.test"))
        .fetch_one(&server.db)
        .await
        .expect("seed user")
}

async fn seed_agent(server: &common::TestServer, owner_id: Uuid, name: &str) -> Uuid {
    sqlx::query_scalar("INSERT INTO agents (name, owner_id) VALUES ($1, $2) RETURNING id")
        .bind(name)
        .bind(owner_id)
        .fetch_one(&server.db)
        .await
        .expect("seed agent")
}

/// Insert one `trace_usage` row an hour ago so it lands in every default window.
#[allow(clippy::too_many_arguments)]
async fn seed_spend(
    server: &common::TestServer,
    agent_id: Uuid,
    agent_name: &str,
    user_id: Uuid,
    model: &str,
    cost: f64,
    cache_read: i64,
) {
    sqlx::query(
        r#"INSERT INTO trace_usage
             (trace_id, agent_name, agent_id, user_id, model, provider,
              input_tokens, output_tokens, cache_read_tokens, cost_usd, latency_ms, started_at)
           VALUES ($1, $2, $3, $4, $5, 'anthropic', 1000, 500, $6, $7, 1200,
                   now() - interval '1 hour')"#,
    )
    .bind(format!("trace-{}", Uuid::new_v4()))
    .bind(agent_name)
    .bind(agent_id)
    .bind(user_id)
    .bind(model)
    .bind(cache_read)
    .bind(cost)
    .execute(&server.db)
    .await
    .expect("seed trace_usage");
}

/// Insert a successful workflow execution owned by `owner_id` an hour ago.
async fn seed_workflow_execution(server: &common::TestServer, owner_id: Uuid, cost: f64) {
    let maf_id: Uuid = sqlx::query_scalar(
        "INSERT INTO mafs (user_id, name, maf_json) VALUES ($1, $2, '{}'::jsonb) RETURNING id",
    )
    .bind(owner_id)
    .bind(format!("wf-{}", Uuid::new_v4()))
    .fetch_one(&server.db)
    .await
    .expect("seed maf");
    sqlx::query(
        r#"INSERT INTO maf_executions (maf_id, user_id, status, cost_usd, started_at, duration_ms)
           VALUES ($1, $2, 'success', $3, now() - interval '1 hour', 1000)"#,
    )
    .bind(maf_id)
    .bind(owner_id)
    .bind(cost)
    .execute(&server.db)
    .await
    .expect("seed maf_execution");
}

async fn grant(server: &common::TestServer, agent_id: Uuid, grantee: Uuid) {
    sqlx::query(
        "INSERT INTO agent_grants (agent_id, grant_type, grantee_id) VALUES ($1, 'user', $2)",
    )
    .bind(agent_id)
    .bind(grantee.to_string())
    .execute(&server.db)
    .await
    .expect("seed grant");
}

/// `(YYYY-MM, YYYY-MM-DD)` of the seeded timestamp, computed in SQL so a seed
/// just after midnight on the 1st cannot disagree with the query strings.
async fn seed_month_and_day(server: &common::TestServer) -> (String, String) {
    let month: String = sqlx::query_scalar(
        "SELECT to_char((now() - interval '1 hour') AT TIME ZONE 'UTC', 'YYYY-MM')",
    )
    .fetch_one(&server.db)
    .await
    .unwrap();
    let day: String = sqlx::query_scalar(
        "SELECT to_char((now() - interval '1 hour') AT TIME ZONE 'UTC', 'YYYY-MM-DD')",
    )
    .fetch_one(&server.db)
    .await
    .unwrap();
    (month, day)
}

struct World {
    alice: Uuid,
    bob: Uuid,
    nobody: Uuid,
    admin: Uuid,
    agent_a: Uuid,
    agent_b: Uuid,
    month: String,
    day: String,
}

impl World {
    fn token(&self, who: Uuid, name: &str) -> String {
        common::sign_token(&who.to_string(), name, who == self.admin, "member")
    }
    fn alice_token(&self) -> String {
        self.token(self.alice, "alice")
    }
    fn nobody_token(&self) -> String {
        self.token(self.nobody, "nobody")
    }
    fn admin_token(&self) -> String {
        self.token(self.admin, "root")
    }
}

/// alice owns agent-a, bob owns agent-b; each has one priced row. bob also has
/// one unpriced row (tokens, no cost) that alice must never count.
async fn seed_world(server: &common::TestServer) -> World {
    let _ = init_admin(server).await;
    let alice = seed_user(server, "alice").await;
    let bob = seed_user(server, "bob").await;
    let nobody = seed_user(server, "nobody").await;
    let admin = seed_user(server, "root").await;
    let agent_a = seed_agent(server, alice, "agent-a").await;
    let agent_b = seed_agent(server, bob, "agent-b").await;
    seed_spend(server, agent_a, "agent-a", alice, "claude-x", COST_A, 0).await;
    seed_spend(server, agent_b, "agent-b", bob, "claude-x", COST_B, 0).await;
    seed_spend(server, agent_b, "agent-b", bob, "claude-x", 0.0, 0).await;
    let (month, day) = seed_month_and_day(server).await;
    World {
        alice,
        bob,
        nobody,
        admin,
        agent_a,
        agent_b,
        month,
        day,
    }
}

async fn get(server: &common::TestServer, path: &str, token: &str) -> (u16, Value) {
    let res = server
        .client
        .get(server.url(path))
        .bearer_auth(token)
        .send()
        .await
        .unwrap();
    let status = res.status().as_u16();
    let text = res.text().await.unwrap();
    let body = serde_json::from_str(&text).unwrap_or(Value::String(text));
    (status, body)
}

fn sum_f64(items: &Value, key: &str) -> f64 {
    items
        .as_array()
        .expect("array")
        .iter()
        .map(|i| i[key].as_f64().unwrap_or(0.0))
        .sum()
}

fn assert_close(actual: f64, expected: f64, what: &str) {
    assert!(
        (actual - expected).abs() < EPS,
        "{what}: expected {expected}, got {actual}"
    );
}

/// Every endpoint's spend total for one caller, plus the raw bodies for leak checks.
struct Views {
    dashboard: Value,
    timeseries: Value,
    calendar: Value,
    day: Value,
    attributions: Value,
}

impl Views {
    fn dashboard_total(&self) -> f64 {
        self.dashboard["data"]["summary"]["total_cost"]
            .as_f64()
            .unwrap()
    }
    fn timeseries_total(&self) -> f64 {
        sum_f64(&self.timeseries["data"]["points"], "spend_usd")
    }
    fn calendar_total(&self) -> f64 {
        sum_f64(&self.calendar["data"]["days"], "spend_usd")
    }
    fn day_total(&self) -> f64 {
        sum_f64(&self.day["data"]["hours"], "spend_usd")
    }
    fn attributions_total(&self) -> f64 {
        sum_f64(&self.attributions["data"]["rows"], "total_cost")
    }
    fn bodies(&self) -> [&Value; 5] {
        [
            &self.dashboard,
            &self.timeseries,
            &self.calendar,
            &self.day,
            &self.attributions,
        ]
    }
}

async fn fetch_views(server: &common::TestServer, w: &World, token: &str) -> Views {
    let (s, dashboard) = get(
        server,
        "/api/observability/finops/dashboard?range=7d",
        token,
    )
    .await;
    assert_eq!(s, 200, "dashboard: {dashboard}");
    let (s, timeseries) = get(
        server,
        "/api/observability/finops/spend-timeseries?range=7d",
        token,
    )
    .await;
    assert_eq!(s, 200, "timeseries: {timeseries}");
    let (s, calendar) = get(
        server,
        &format!("/api/observability/finops/spend-calendar?month={}", w.month),
        token,
    )
    .await;
    assert_eq!(s, 200, "calendar: {calendar}");
    let (s, day) = get(
        server,
        &format!(
            "/api/observability/finops/spend-calendar/day?date={}",
            w.day
        ),
        token,
    )
    .await;
    assert_eq!(s, 200, "day: {day}");
    let (s, attributions) = get(
        server,
        "/api/observability/finops/attributions?range=7d",
        token,
    )
    .await;
    assert_eq!(s, 200, "attributions: {attributions}");
    Views {
        dashboard,
        timeseries,
        calendar,
        day,
        attributions,
    }
}

// ─── no grants ───────────────────────────────────────────────────────────────

#[tokio::test]
#[serial]
async fn no_grants_user_sees_zero_from_all_five_endpoints() {
    let server = common::TestServer::start().await;
    let w = seed_world(&server).await;

    let v = fetch_views(&server, &w, &w.nobody_token()).await;

    assert_close(v.dashboard_total(), 0.0, "dashboard total_cost");
    assert_eq!(v.dashboard["data"]["agents"], json!([]));
    let summary = &v.dashboard["data"]["summary"];
    assert_close(
        summary["estimated_cost"].as_f64().unwrap(),
        0.0,
        "estimated_cost",
    );
    assert_eq!(summary["unpriced_calls"], 0);
    assert_eq!(summary["operations_last_24h"], 0);
    assert_eq!(summary["unknown_confidence_calls"], 0);
    assert_eq!(v.timeseries["data"]["points"], json!([]));
    assert_close(v.calendar_total(), 0.0, "calendar total");
    assert_eq!(v.day["data"]["top_agents"], json!([]));
    assert_close(v.day_total(), 0.0, "day total");
    assert_eq!(v.attributions["data"]["rows"], json!([]));

    server.cleanup().await;
}

// ─── owner ───────────────────────────────────────────────────────────────────

#[tokio::test]
#[serial]
async fn owner_sees_own_spend_not_others() {
    let server = common::TestServer::start().await;
    let w = seed_world(&server).await;

    let v = fetch_views(&server, &w, &w.alice_token()).await;

    assert_close(v.dashboard_total(), COST_A, "dashboard total");
    assert_close(v.timeseries_total(), COST_A, "timeseries total");
    assert_close(v.calendar_total(), COST_A, "calendar total");
    assert_close(v.day_total(), COST_A, "day total");
    assert_close(v.attributions_total(), COST_A, "attributions total");

    // bob's unpriced row and his 24h operations must not reach alice.
    let summary = &v.dashboard["data"]["summary"];
    assert_eq!(summary["unpriced_calls"], 0, "unpriced_calls leak");
    assert_eq!(
        summary["operations_last_24h"], 1,
        "operations_last_24h leak"
    );
    assert_eq!(
        summary["unknown_confidence_calls"], 1,
        "own row is the only one counted"
    );
    assert_close(
        summary["estimated_cost"].as_f64().unwrap(),
        0.0,
        "estimated_cost",
    );

    for body in v.bodies() {
        let text = body.to_string();
        assert!(!text.contains("agent-b"), "agent-b leaked: {text}");
        assert!(
            !text.contains(&w.agent_b.to_string()),
            "agent-b id leaked: {text}"
        );
    }
    assert!(
        v.attributions["data"]["rows"]
            .to_string()
            .contains("agent-a")
    );

    server.cleanup().await;
}

// ─── superuser ───────────────────────────────────────────────────────────────

#[tokio::test]
#[serial]
async fn superuser_sees_both() {
    let server = common::TestServer::start().await;
    let w = seed_world(&server).await;

    let v = fetch_views(&server, &w, &w.admin_token()).await;
    let both = COST_A + COST_B;

    assert_close(v.dashboard_total(), both, "dashboard total");
    assert_close(v.timeseries_total(), both, "timeseries total");
    assert_close(v.calendar_total(), both, "calendar total");
    assert_close(v.day_total(), both, "day total");
    assert_close(v.attributions_total(), both, "attributions total");
    assert_eq!(v.dashboard["data"]["summary"]["unpriced_calls"], 1);

    server.cleanup().await;
}

// ─── agent_id filter ─────────────────────────────────────────────────────────

#[tokio::test]
#[serial]
async fn inaccessible_agent_filter_returns_404() {
    let server = common::TestServer::start().await;
    let w = seed_world(&server).await;
    let token = w.alice_token();
    let b = w.agent_b;

    let paths = [
        format!("/api/observability/finops/dashboard?range=7d&agent_id={b}"),
        format!("/api/observability/finops/spend-timeseries?range=7d&agent_id={b}"),
        format!(
            "/api/observability/finops/spend-calendar?month={}&agent_id={b}",
            w.month
        ),
        format!(
            "/api/observability/finops/spend-calendar/day?date={}&agent_id={b}",
            w.day
        ),
        format!("/api/observability/finops/attributions?range=7d&agent_id={b}"),
    ];
    for path in paths {
        let (status, body) = get(&server, &path, &token).await;
        assert_eq!(status, 404, "{path}: {body}");
    }

    // The accessible agent stays reachable through the same filter.
    let (status, _) = get(
        &server,
        &format!(
            "/api/observability/finops/spend-timeseries?range=7d&agent_id={}",
            w.agent_a
        ),
        &token,
    )
    .await;
    assert_eq!(status, 200);

    server.cleanup().await;
}

// ─── explicit grant ──────────────────────────────────────────────────────────

#[tokio::test]
#[serial]
async fn explicit_grant_extends_visibility() {
    let server = common::TestServer::start().await;
    let w = seed_world(&server).await;
    grant(&server, w.agent_b, w.alice).await;

    let v = fetch_views(&server, &w, &w.alice_token()).await;
    let both = COST_A + COST_B;

    assert_close(v.dashboard_total(), both, "dashboard total");
    assert_close(v.timeseries_total(), both, "timeseries total");
    assert_close(v.calendar_total(), both, "calendar total");
    assert_close(v.day_total(), both, "day total");
    assert_close(v.attributions_total(), both, "attributions total");

    server.cleanup().await;
}

// ─── same-named agents ───────────────────────────────────────────────────────

/// Agent names are unique per owner only, and `trace_usage` is keyed by name.
/// A name is visible only when every live agent carrying it is accessible.
#[tokio::test]
#[serial]
async fn same_name_agents_do_not_leak() {
    let server = common::TestServer::start().await;
    let _ = init_admin(&server).await;
    let alice = seed_user(&server, "alice").await;
    let bob = seed_user(&server, "bob").await;
    let alice_helper = seed_agent(&server, alice, "helper").await;
    let bob_helper = seed_agent(&server, bob, "helper").await;
    seed_spend(
        &server,
        alice_helper,
        "helper",
        alice,
        "claude-x",
        COST_A,
        0,
    )
    .await;
    seed_spend(&server, bob_helper, "helper", bob, "claude-x", COST_B, 0).await;
    let token = common::sign_token(&alice.to_string(), "alice", false, "member");

    let (s, ts) = get(
        &server,
        "/api/observability/finops/spend-timeseries?range=7d",
        &token,
    )
    .await;
    assert_eq!(s, 200);
    assert_eq!(
        ts["data"]["points"],
        json!([]),
        "same-name spend leaked: {ts}"
    );

    let (s, attr) = get(
        &server,
        "/api/observability/finops/attributions?range=7d",
        &token,
    )
    .await;
    assert_eq!(s, 200);
    // alice's own "helper" is still listed, but carries none of the spend.
    assert_close(
        sum_f64(&attr["data"]["rows"], "total_cost"),
        0.0,
        "same-name attributions cost",
    );
    assert_eq!(
        sum_f64(&attr["data"]["rows"], "operations"),
        0.0,
        "same-name attributions operations leaked: {attr}"
    );

    server.cleanup().await;
}

// ─── workflow view ───────────────────────────────────────────────────────────

// By decision (01-CONTEXT post-research decisions) the workflow view is
// owner-only for non-superusers (`maf_executions.user_id = caller`), not agent
// grant based, and a non-UUID caller fails closed to `Uuid::nil()` (no rows).
#[tokio::test]
#[serial]
async fn workflow_view_scoped_to_caller() {
    let server = common::TestServer::start().await;
    let w = seed_world(&server).await;
    seed_workflow_execution(&server, w.bob, 2.0).await;

    let path = "/api/observability/finops/attributions?range=7d&view=workflow";

    let (s, alice_body) = get(&server, path, &w.alice_token()).await;
    assert_eq!(s, 200, "{alice_body}");
    assert_eq!(alice_body["data"]["view"], "workflow");
    assert_eq!(
        alice_body["data"]["rows"],
        json!([]),
        "bob's workflow leaked to alice"
    );

    let (s, admin_body) = get(&server, path, &w.admin_token()).await;
    assert_eq!(s, 200, "{admin_body}");
    assert_close(
        sum_f64(&admin_body["data"]["rows"], "total_cost"),
        2.0,
        "admin workflow total",
    );

    // Bob sees his own execution.
    let (s, bob_body) = get(&server, path, &w.token(w.bob, "bob")).await;
    assert_eq!(s, 200, "{bob_body}");
    assert_close(
        sum_f64(&bob_body["data"]["rows"], "total_cost"),
        2.0,
        "bob workflow total",
    );

    // Dashboard workflow view is scoped the same way.
    let (s, dash) = get(
        &server,
        "/api/observability/finops/dashboard?range=7d&view=workflow",
        &w.alice_token(),
    )
    .await;
    assert_eq!(s, 200, "{dash}");
    assert_eq!(dash["data"]["attributions"]["rows"], json!([]));

    server.cleanup().await;
}

// ─── insights ────────────────────────────────────────────────────────────────

async fn post_insights(server: &common::TestServer, token: &str) -> (u16, Value) {
    let res = server
        .client
        .post(server.url("/api/observability/finops/insights"))
        .bearer_auth(token)
        .json(&json!({"kpi": {}, "agent_costs": []}))
        .send()
        .await
        .unwrap();
    let status = res.status().as_u16();
    let text = res.text().await.unwrap();
    let body = serde_json::from_str(&text).unwrap_or(Value::String(text));
    (status, body)
}

#[tokio::test]
#[serial]
async fn insights_forbidden_for_non_superuser() {
    let server = common::TestServer::start().await;
    let w = seed_world(&server).await;

    let (s, body) = post_insights(&server, &w.alice_token()).await;
    assert_eq!(s, 403, "{body}");
    assert_eq!(body["code"], "superuser_required");

    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn insights_not_configured_returns_available_false_for_superuser() {
    let server = common::TestServer::start_with(|c| {
        c.openai_api_key = None;
    })
    .await;
    let w = seed_world(&server).await;

    let (s, body) = post_insights(&server, &w.admin_token()).await;
    assert_eq!(s, 200, "{body}");
    assert_eq!(body["data"]["available"], false);
    assert_eq!(body["data"]["reason"], "llm_not_configured");
    assert_eq!(body["data"]["insights"], json!([]));
    assert_eq!(body["status_code"], 200);

    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn insights_empty_key_is_not_configured() {
    let server = common::TestServer::start_with(|c| {
        c.openai_api_key = Some(String::new());
    })
    .await;
    let w = seed_world(&server).await;

    let (s, body) = post_insights(&server, &w.admin_token()).await;
    assert_eq!(s, 200, "{body}");
    assert_eq!(body["data"]["available"], false);

    server.cleanup().await;
}
