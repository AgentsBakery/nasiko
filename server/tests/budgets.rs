//! Budget CRUD, admin gate, `/api/budgets/me` scoping and live status.
//!
//! Covers the `/api/budgets` REST surface and the shared budget engine it reads
//! through: spend comes from Redis counters that are rebuilt from router-metered
//! `token_usage` (`operation_type IN ('direct_llm','embedding')`) when a key is
//! missing, and a counter first materialized at or above a threshold writes its
//! `budget_events` row exactly once. Router enforcement sections are appended
//! under their own banners by later plans.
//!
//! The `end-to-end` section chains router block -> alert -> signed webhook.
//!
//! Requires infra (Postgres, Redis, S3 emulator):
//!   cargo test -p nasiko-server --test budgets -- --test-threads=1

mod common;

use chrono::{DateTime, Datelike, TimeZone, Utc};
use common::TestServer;
use hmac::{Hmac, Mac};
use nasiko_config::AlertsConfig;
use nasiko_llm_router::budget::keys::spend_key;
use nasiko_llm_router::budget::period::{Period, period_bounds};
use nasiko_server::alerts::tick_budget_events;
use nasiko_server::notifications::dispatch::{DispatchDeps, tick_outbox_dispatch};
use redis::AsyncCommands;
use serde_json::{Value, json};
use serial_test::serial;
use sha2::Sha256;
use uuid::Uuid;

const EPS: f64 = 1e-6;

// ─── helpers ─────────────────────────────────────────────────────────────────

async fn seed_user(server: &TestServer, name: &str, role: &str) -> Uuid {
    sqlx::query_scalar(
        "INSERT INTO users (username, email, role) VALUES ($1, $2, $3::user_role) RETURNING id",
    )
    .bind(name)
    .bind(format!("{name}@budgets.test"))
    .bind(role)
    .fetch_one(&server.db)
    .await
    .expect("seed user")
}

async fn seed_agent(server: &TestServer, owner_id: Uuid, name: &str) -> Uuid {
    sqlx::query_scalar("INSERT INTO agents (name, owner_id) VALUES ($1, $2) RETURNING id")
        .bind(name)
        .bind(owner_id)
        .fetch_one(&server.db)
        .await
        .expect("seed agent")
}

async fn seed_usage(
    server: &TestServer,
    user_id: Uuid,
    agent_id: Option<Uuid>,
    operation_type: &str,
    cost_usd: f64,
    created_at: DateTime<Utc>,
) {
    sqlx::query(
        "INSERT INTO token_usage (user_id, agent_id, operation_type, provider, model, cost_usd, created_at) \
         VALUES ($1, $2, $3, 'openai', 'gpt-4o-mini', $4::float8::numeric, $5)",
    )
    .bind(user_id)
    .bind(agent_id)
    .bind(operation_type)
    .bind(cost_usd)
    .bind(created_at)
    .execute(&server.db)
    .await
    .expect("seed usage");
}

fn redis_client() -> redis::Client {
    let url = std::env::var("TEST_REDIS_URL").unwrap_or_else(|_| "redis://localhost:6379".into());
    redis::Client::open(url).expect("redis client")
}

async fn del_key(key: &str) {
    let mut conn = redis_client()
        .get_multiplexed_async_connection()
        .await
        .expect("redis conn");
    let _: () = conn.del(key).await.expect("del");
}

async fn get_key(key: &str) -> Option<i64> {
    let mut conn = redis_client()
        .get_multiplexed_async_connection()
        .await
        .expect("redis conn");
    conn.get(key).await.expect("get")
}

/// `require_auth` resolves the caller from `users`, so the superuser needs a row.
async fn seed_root(server: &TestServer) -> Uuid {
    sqlx::query_scalar(
        "INSERT INTO users (username, email, is_superuser) VALUES ('budgets-root', \
         'budgets-root@budgets.test', true) RETURNING id",
    )
    .fetch_one(&server.db)
    .await
    .expect("seed root")
}

fn admin(root: Uuid, rb: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
    common::as_superuser(rb, &root.to_string(), "budgets-root")
}

fn member(rb: reqwest::RequestBuilder, id: Uuid, name: &str) -> reqwest::RequestBuilder {
    common::as_member(rb, &id.to_string(), name)
}

/// Create a budget as a superuser; returns the raw response.
async fn post_budget(server: &TestServer, root: Uuid, body: Value) -> reqwest::Response {
    admin(root, server.client.post(server.url("/api/budgets")))
        .json(&body)
        .send()
        .await
        .expect("post budget")
}

async fn create_budget(server: &TestServer, root: Uuid, body: Value) -> Value {
    let resp = post_budget(server, root, body).await;
    assert_eq!(resp.status(), 201, "create budget");
    resp.json::<Value>().await.unwrap()["data"].clone()
}

async fn get_budget(server: &TestServer, root: Uuid, id: &str) -> Value {
    let resp = admin(
        root,
        server.client.get(server.url(&format!("/api/budgets/{id}"))),
    )
    .send()
    .await
    .unwrap();
    assert_eq!(resp.status(), 200, "get budget");
    resp.json::<Value>().await.unwrap()["data"].clone()
}

fn user_budget(name: &str, target: Uuid, limit: f64) -> Value {
    json!({
        "name": name, "scope": "user", "target_id": target,
        "period": "monthly", "limit_usd": limit, "action": "block",
    })
}

fn month_start(now: DateTime<Utc>) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(now.year(), now.month(), 1, 0, 0, 0)
        .unwrap()
}

async fn event_kinds(server: &TestServer, budget_id: &str) -> Vec<String> {
    sqlx::query_scalar("SELECT kind FROM budget_events WHERE budget_id = $1::uuid ORDER BY kind")
        .bind(budget_id)
        .fetch_all(&server.db)
        .await
        .expect("events")
}

// ─── budget CRUD & RBAC ──────────────────────────────────────────────────────

#[tokio::test]
#[serial]
async fn admin_can_crud_budgets() {
    let server = TestServer::start().await;
    let root = seed_root(&server).await;
    let user = seed_user(&server, "crud-user", "member").await;

    let resp = post_budget(&server, root, user_budget("crud budget", user, 10.0)).await;
    assert_eq!(resp.status(), 201);
    let created = resp.json::<Value>().await.unwrap()["data"].clone();
    let id = created["id"].as_str().expect("id").to_owned();
    assert_eq!(created["soft_threshold_pct"], 80);
    assert_eq!(created["downgrade_ceiling_pct"], 125);
    assert_eq!(created["enabled"], true);

    let fetched = get_budget(&server, root, &id).await;
    assert_eq!(fetched["name"], "crud budget");
    assert!((fetched["limit_usd"].as_f64().unwrap() - 10.0).abs() < EPS);

    let resp = admin(
        root,
        server.client.put(server.url(&format!("/api/budgets/{id}"))),
    )
    .json(&json!({"limit_usd": 20.0, "enabled": false}))
    .send()
    .await
    .unwrap();
    assert_eq!(resp.status(), 200);
    let updated = resp.json::<Value>().await.unwrap()["data"].clone();
    assert!((updated["limit_usd"].as_f64().unwrap() - 20.0).abs() < EPS);
    assert_eq!(updated["enabled"], false);

    let resp = admin(
        root,
        server
            .client
            .delete(server.url(&format!("/api/budgets/{id}"))),
    )
    .send()
    .await
    .unwrap();
    assert_eq!(resp.status(), 204);

    let resp = admin(
        root,
        server.client.get(server.url(&format!("/api/budgets/{id}"))),
    )
    .send()
    .await
    .unwrap();
    assert_eq!(resp.status(), 404);
    assert_eq!(resp.json::<Value>().await.unwrap()["code"], "not_found");
    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn role_admin_non_superuser_can_crud() {
    let server = TestServer::start().await;
    let target = seed_user(&server, "roleadmin-target", "member").await;
    let role_admin = seed_user(&server, "role-admin", "admin").await;

    let resp = member(
        server.client.post(server.url("/api/budgets")),
        role_admin,
        "role-admin",
    )
    .json(&user_budget("role admin budget", target, 5.0))
    .send()
    .await
    .unwrap();
    assert_eq!(resp.status(), 201);
    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn member_gets_403_on_admin_routes() {
    let server = TestServer::start().await;
    let root = seed_root(&server).await;
    let m = seed_user(&server, "plain-member", "member").await;
    let created = create_budget(&server, root, user_budget("member-gate", m, 5.0)).await;
    let id = created["id"].as_str().unwrap();

    let calls = [
        member(
            server.client.get(server.url("/api/budgets")),
            m,
            "plain-member",
        ),
        member(
            server.client.post(server.url("/api/budgets")),
            m,
            "plain-member",
        )
        .json(&user_budget("nope", m, 1.0)),
        member(
            server.client.get(server.url(&format!("/api/budgets/{id}"))),
            m,
            "plain-member",
        ),
        member(
            server.client.put(server.url(&format!("/api/budgets/{id}"))),
            m,
            "plain-member",
        )
        .json(&json!({"limit_usd": 99.0})),
        member(
            server
                .client
                .delete(server.url(&format!("/api/budgets/{id}"))),
            m,
            "plain-member",
        ),
    ];
    for rb in calls {
        let resp = rb.send().await.unwrap();
        assert_eq!(resp.status(), 403);
        assert_eq!(
            resp.json::<Value>().await.unwrap()["code"],
            "admin_required"
        );
    }

    let resp = member(
        server.client.get(server.url("/api/budgets/me")),
        m,
        "plain-member",
    )
    .send()
    .await
    .unwrap();
    assert_eq!(resp.status(), 200);
    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn validation_rejects_bad_input() {
    let server = TestServer::start().await;
    let root = seed_root(&server).await;
    let u = seed_user(&server, "valid-user", "member").await;
    let base = user_budget("v", u, 5.0);

    let with = |k: &str, v: Value| {
        let mut b = base.clone();
        b[k] = v;
        b
    };
    let cases: Vec<(Value, &str)> = vec![
        (with("scope", json!("team")), "invalid_scope"),
        (with("period", json!("yearly")), "invalid_period"),
        (with("action", json!("notify")), "invalid_action"),
        (with("limit_usd", json!(0)), "invalid_limit"),
        (with("limit_usd", json!(-1)), "invalid_limit"),
        (with("soft_threshold_pct", json!(0)), "invalid_threshold"),
        (with("soft_threshold_pct", json!(101)), "invalid_threshold"),
        (with("downgrade_ceiling_pct", json!(99)), "invalid_ceiling"),
        (
            json!({"name": "v", "scope": "user", "period": "monthly", "limit_usd": 5, "action": "block"}),
            "target_required",
        ),
        (
            json!({"name": "v", "scope": "platform", "target_id": u, "period": "monthly", "limit_usd": 5, "action": "block"}),
            "target_not_allowed",
        ),
        (
            json!({"name": "v", "scope": "agent", "target_id": Uuid::new_v4(), "period": "monthly", "limit_usd": 5, "action": "block"}),
            "target_not_found",
        ),
    ];
    for (body, code) in cases {
        let resp = post_budget(&server, root, body.clone()).await;
        assert_eq!(resp.status(), 400, "{body}");
        assert_eq!(resp.json::<Value>().await.unwrap()["code"], code, "{body}");
    }

    let created = create_budget(&server, root, base.clone()).await;
    let id = created["id"].as_str().unwrap();
    for body in [
        json!({"scope": "agent"}),
        json!({"target_id": Uuid::new_v4()}),
    ] {
        let resp = admin(
            root,
            server.client.put(server.url(&format!("/api/budgets/{id}"))),
        )
        .json(&body)
        .send()
        .await
        .unwrap();
        assert_eq!(resp.status(), 400);
        assert_eq!(
            resp.json::<Value>().await.unwrap()["code"],
            "scope_immutable"
        );
    }
    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn me_is_scoped_to_caller() {
    let server = TestServer::start().await;
    let root = seed_root(&server).await;
    let alice = seed_user(&server, "me-alice", "member").await;
    let bob = seed_user(&server, "me-bob", "member").await;
    let alice_agent = seed_agent(&server, alice, "me-alice-agent").await;
    let bob_agent = seed_agent(&server, bob, "me-bob-agent").await;

    create_budget(&server, root, user_budget("alice-user", alice, 10.0)).await;
    create_budget(&server, root, user_budget("bob-user", bob, 10.0)).await;
    for (name, agent) in [("alice-agent", alice_agent), ("bob-agent", bob_agent)] {
        create_budget(
            &server,
            root,
            json!({"name": name, "scope": "agent", "target_id": agent,
                   "period": "daily", "limit_usd": 3.0, "action": "downgrade"}),
        )
        .await;
    }
    create_budget(
        &server,
        root,
        json!({"name": "platform-wide", "scope": "platform",
               "period": "weekly", "limit_usd": 1000.0, "action": "block"}),
    )
    .await;

    let resp = member(
        server.client.get(server.url("/api/budgets/me")),
        alice,
        "me-alice",
    )
    .send()
    .await
    .unwrap();
    assert_eq!(resp.status(), 200);
    let rows = resp.json::<Value>().await.unwrap()["data"]
        .as_array()
        .unwrap()
        .clone();
    let mut names: Vec<&str> = rows.iter().map(|r| r["name"].as_str().unwrap()).collect();
    names.sort();
    assert_eq!(names, ["alice-agent", "alice-user", "platform-wide"]);

    for row in &rows {
        if row["name"] == "platform-wide" {
            for k in ["limit_usd", "spend_usd", "projected_usd"] {
                assert!(row.get(k).is_none(), "platform row leaks {k}");
            }
            for k in ["period", "pct_used", "state", "resets_at"] {
                assert!(row.get(k).is_some(), "platform row missing {k}");
            }
        } else {
            assert!(row.get("limit_usd").is_some());
            assert!(row.get("spend_usd").is_some());
        }
    }
    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn me_includes_accessible_agent_budgets_redacted() {
    let server = TestServer::start().await;
    let root = seed_root(&server).await;
    let alice = seed_user(&server, "acc-alice", "member").await;
    let bob = seed_user(&server, "acc-bob", "member").await;
    let carol = seed_user(&server, "acc-carol", "member").await;
    let agent = seed_agent(&server, alice, "acc-shared-agent").await;
    sqlx::query(
        "INSERT INTO agent_grants (agent_id, grant_type, grantee_id) VALUES ($1, 'user', $2)",
    )
    .bind(agent)
    .bind(bob.to_string())
    .execute(&server.db)
    .await
    .expect("seed grant");
    create_budget(
        &server,
        root,
        json!({"name": "shared-agent", "scope": "agent", "target_id": agent,
               "period": "daily", "limit_usd": 3.0, "action": "block"}),
    )
    .await;

    let me_rows = |id: Uuid, name: &'static str| {
        let server = &server;
        async move {
            let resp = member(server.client.get(server.url("/api/budgets/me")), id, name)
                .send()
                .await
                .unwrap();
            assert_eq!(resp.status(), 200);
            resp.json::<Value>().await.unwrap()["data"]
                .as_array()
                .unwrap()
                .clone()
        }
    };

    let bob_rows = me_rows(bob, "acc-bob").await;
    let row = bob_rows
        .iter()
        .find(|r| r["name"] == "shared-agent")
        .expect("grantee sees the agent budget");
    for k in ["limit_usd", "spend_usd", "projected_usd"] {
        assert!(row.get(k).is_none(), "grantee row leaks {k}");
    }
    for k in ["period", "pct_used", "state", "resets_at"] {
        assert!(row.get(k).is_some(), "grantee row missing {k}");
    }

    let alice_rows = me_rows(alice, "acc-alice").await;
    let row = alice_rows
        .iter()
        .find(|r| r["name"] == "shared-agent")
        .expect("owner sees the agent budget");
    assert!(row.get("limit_usd").is_some());
    assert!(row.get("spend_usd").is_some());

    let carol_rows = me_rows(carol, "acc-carol").await;
    assert!(
        carol_rows.iter().all(|r| r["name"] != "shared-agent"),
        "unrelated member must not see the agent budget"
    );
    server.cleanup().await;
}

// ─── live status & rebuild ───────────────────────────────────────────────────

#[tokio::test]
#[serial]
async fn status_reflects_token_usage_and_rebuilds_on_missing_key() {
    let server = TestServer::start().await;
    let root = seed_root(&server).await;
    let user = seed_user(&server, "status-user", "member").await;
    let now = Utc::now();
    let start = month_start(now);
    let in_period = start + chrono::Duration::seconds(5);
    let last_month = start - chrono::Duration::days(3);

    seed_usage(&server, user, None, "direct_llm", 1.5, in_period).await;
    seed_usage(&server, user, None, "embedding", 0.5, in_period).await;
    seed_usage(&server, user, None, "orchestrator", 50.0, in_period).await;
    seed_usage(&server, user, None, "direct_llm", 50.0, last_month).await;

    let created = create_budget(&server, root, user_budget("status budget", user, 4.0)).await;
    let id = created["id"].as_str().unwrap().to_owned();
    let key = spend_key(id.parse().unwrap(), period_bounds(Period::Monthly, now).0);
    del_key(&key).await;

    let view = get_budget(&server, root, &id).await;
    assert!((view["spend_usd"].as_f64().unwrap() - 2.0).abs() < EPS);
    assert!((view["pct_used"].as_f64().unwrap() - 50.0).abs() < EPS);
    assert_eq!(view["state"], "ok");
    let period_start: DateTime<Utc> = view["period_start"].as_str().unwrap().parse().unwrap();
    assert_eq!(period_start, start);
    let resets_at: DateTime<Utc> = view["resets_at"].as_str().unwrap().parse().unwrap();
    assert_eq!(resets_at, period_bounds(Period::Monthly, now).1);
    assert_eq!(get_key(&key).await, Some(2_000_000));
    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn rebuild_emits_threshold_events() {
    let server = TestServer::start().await;
    let root = seed_root(&server).await;
    let soft_user = seed_user(&server, "events-soft", "member").await;
    let hard_user = seed_user(&server, "events-hard", "member").await;
    let now = Utc::now();
    let start = month_start(now);
    let in_period = start + chrono::Duration::seconds(5);

    seed_usage(&server, soft_user, None, "direct_llm", 3.5, in_period).await;
    seed_usage(&server, hard_user, None, "direct_llm", 5.0, in_period).await;

    let soft = create_budget(&server, root, user_budget("events soft", soft_user, 4.0)).await;
    let soft_id = soft["id"].as_str().unwrap().to_owned();
    let hard = create_budget(&server, root, user_budget("events hard", hard_user, 4.0)).await;
    let hard_id = hard["id"].as_str().unwrap().to_owned();
    for id in [&soft_id, &hard_id] {
        del_key(&spend_key(id.parse().unwrap(), start)).await;
    }

    get_budget(&server, root, &soft_id).await;
    assert_eq!(event_kinds(&server, &soft_id).await, ["soft_threshold"]);
    let event_start: DateTime<Utc> =
        sqlx::query_scalar("SELECT period_start FROM budget_events WHERE budget_id = $1::uuid")
            .bind(&soft_id)
            .fetch_one(&server.db)
            .await
            .unwrap();
    assert_eq!(event_start, start);

    // Key now present: a second read must not add anything.
    get_budget(&server, root, &soft_id).await;
    assert_eq!(event_kinds(&server, &soft_id).await, ["soft_threshold"]);

    get_budget(&server, root, &hard_id).await;
    assert_eq!(
        event_kinds(&server, &hard_id).await,
        ["hard_limit", "soft_threshold"]
    );
    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn status_states() {
    let server = TestServer::start().await;
    let root = seed_root(&server).await;
    let now = Utc::now();
    let start = month_start(now);
    let in_period = start + chrono::Duration::seconds(5);

    // (name, action, spend, enabled, expected state)
    let cases = [
        ("st-soft", "block", 3.5, true, "soft"),
        ("st-blocked", "block", 4.0, true, "blocked"),
        ("st-downgrading", "downgrade", 4.5, true, "downgrading"),
        ("st-ceiling", "downgrade", 5.0, true, "blocked"),
        ("st-disabled", "block", 1.0, false, "disabled"),
    ];
    for (name, action, spend, enabled, expected) in cases {
        let user = seed_user(&server, name, "member").await;
        seed_usage(&server, user, None, "direct_llm", spend, in_period).await;
        let created = create_budget(
            &server,
            root,
            json!({"name": name, "scope": "user", "target_id": user,
                   "period": "monthly", "limit_usd": 4.0, "action": action,
                   "enabled": enabled}),
        )
        .await;
        let id = created["id"].as_str().unwrap();
        del_key(&spend_key(id.parse().unwrap(), start)).await;
        let view = get_budget(&server, root, id).await;
        assert_eq!(view["state"], expected, "{name}");
    }
    server.cleanup().await;
}

// ─── router enforcement ──────────────────────────────────────────────────────
//
// The router reads `GatewayConfig::from_env()` once when the app boots, so every
// test here is `#[serial]` and points the environment at its stub upstream
// before `TestServer::start()`. Agent identity is a real agent JWT; the billed
// user comes from a live flow (`common::open_flow`) except for coding agents.

const ROUTER_JWT_SECRET: &str = "budgets-router-test-secret";
const BIG_PROMPT_TOKENS: u64 = 100_000;

fn set_router_env(upstream_url: &str) {
    // SAFETY: serialized by #[serial] within this test binary.
    unsafe {
        std::env::set_var("OPENAI_API_BASE", upstream_url);
        std::env::set_var("AGENT_JWT_SECRET", ROUTER_JWT_SECRET);
        std::env::set_var("PLATFORM_OPENAI_API_KEY", "sk-platform-test");
        std::env::set_var("PLATFORM_ANTHROPIC_API_KEY", "sk-ant-platform-test");
        std::env::set_var("PLATFORM_GEMINI_API_KEY", "gem-platform-test");
    }
}

/// Upstream whose every response carries a large usage block, so one call costs
/// far more than the tiny limits the tests use. Streaming requests get an SSE
/// body ending in a usage chunk.
async fn stub_upstream_big_usage() -> mockito::ServerGuard {
    let mut upstream = mockito::Server::new_async().await;
    // mockito prefers the most recently created matching mock, so the catch-all
    // JSON mock is registered first and the stricter streaming mock last.
    let sse = format!(
        "data: {{\"id\":\"x\",\"object\":\"chat.completion.chunk\",\"model\":\"gpt-4o-mini\",\
         \"choices\":[{{\"index\":0,\"delta\":{{\"role\":\"assistant\",\"content\":\"hi\"}}}}]}}\n\n\
         data: {{\"id\":\"x\",\"object\":\"chat.completion.chunk\",\"model\":\"gpt-4o-mini\",\
         \"choices\":[{{\"index\":0,\"delta\":{{}},\"finish_reason\":\"stop\"}}],\
         \"usage\":{{\"prompt_tokens\":{BIG_PROMPT_TOKENS},\"completion_tokens\":7,\
         \"total_tokens\":{}}}}}\n\ndata: [DONE]\n\n",
        BIG_PROMPT_TOKENS + 7
    );
    upstream
        .mock("POST", "/chat/completions")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(format!(
            r#"{{"id":"chatcmpl-1","object":"chat.completion","created":1,
                "model":"gpt-4o-mini",
                "choices":[{{"index":0,"message":{{"role":"assistant","content":"hi"}},
                            "finish_reason":"stop"}}],
                "usage":{{"prompt_tokens":{BIG_PROMPT_TOKENS},"completion_tokens":7,
                          "total_tokens":{}}}}}"#,
            BIG_PROMPT_TOKENS + 7
        ))
        .expect_at_least(0)
        .create_async()
        .await;
    upstream
        .mock("POST", "/chat/completions")
        .match_body(mockito::Matcher::PartialJson(json!({"stream": true})))
        .with_status(200)
        .with_header("content-type", "text/event-stream")
        .with_body(sse)
        .expect_at_least(0)
        .create_async()
        .await;
    upstream
        .mock("POST", "/embeddings")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(format!(
            r#"{{"object":"list","model":"text-embedding-3-small",
                "data":[{{"object":"embedding","index":0,"embedding":[0.1,0.2]}}],
                "usage":{{"prompt_tokens":{BIG_PROMPT_TOKENS},"total_tokens":{BIG_PROMPT_TOKENS}}}}}"#
        ))
        .expect_at_least(0)
        .create_async()
        .await;
    set_router_env(&upstream.url());
    upstream
}

fn router_agent_jwt(agent_id: Uuid, owner_id: Uuid) -> String {
    nasiko_llm_router::auth::mint_agent_token(
        &agent_id.to_string(),
        &owner_id.to_string(),
        ROUTER_JWT_SECRET,
        3600,
        jsonwebtoken::Algorithm::HS256,
    )
    .expect("mint agent token")
}

async fn post_llm(
    server: &TestServer,
    path: &str,
    bearer: &str,
    traceparent: Option<&str>,
    body: &Value,
) -> reqwest::Response {
    post_llm_at(
        &server.base_url,
        &server.client,
        path,
        bearer,
        traceparent,
        body,
    )
    .await
}

async fn post_llm_at(
    base_url: &str,
    client: &reqwest::Client,
    path: &str,
    bearer: &str,
    traceparent: Option<&str>,
    body: &Value,
) -> reqwest::Response {
    let mut req = client
        .post(format!("{base_url}{path}"))
        .bearer_auth(bearer)
        .json(body);
    if let Some(tp) = traceparent {
        req = req.header("traceparent", tp);
    }
    req.send().await.expect("llm request")
}

fn chat_body() -> Value {
    json!({"model": "gpt-4o-mini", "messages": [{"role": "user", "content": "hello"}]})
}

fn stream_chat_body() -> Value {
    json!({"model": "gpt-4o-mini", "stream": true,
           "messages": [{"role": "user", "content": "hello"}]})
}

fn embeddings_body() -> Value {
    json!({"model": "text-embedding-3-small", "input": "hello"})
}

/// A user with an agent, a live flow, and an agent JWT: everything a router call needs.
struct Caller {
    user: Uuid,
    agent: Uuid,
    jwt: String,
    traceparent: String,
}

async fn caller(server: &TestServer, name: &str) -> Caller {
    let user = seed_user(server, name, "member").await;
    let agent = seed_agent(server, user, &format!("{name}-agent")).await;
    let (_flow, traceparent) = common::open_flow(&server.db, user, agent).await;
    Caller {
        user,
        agent,
        jwt: router_agent_jwt(agent, user),
        traceparent,
    }
}

async fn chat(server: &TestServer, c: &Caller) -> reqwest::Response {
    post_llm(
        server,
        "/v1/chat/completions",
        &c.jwt,
        Some(&c.traceparent),
        &chat_body(),
    )
    .await
}

fn current_key(budget_id: &str) -> String {
    spend_key(
        budget_id.parse().unwrap(),
        period_bounds(Period::Monthly, Utc::now()).0,
    )
}

fn in_current_period() -> DateTime<Utc> {
    month_start(Utc::now()) + chrono::Duration::seconds(5)
}

/// Poll `key` until `pred` holds or `within` elapses; returns the last value.
async fn wait_for_key(
    key: &str,
    within: std::time::Duration,
    pred: impl Fn(Option<i64>) -> bool,
) -> Option<i64> {
    let deadline = std::time::Instant::now() + within;
    loop {
        let v = get_key(key).await;
        if pred(v) || std::time::Instant::now() >= deadline {
            return v;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
}

fn retry_after(resp: &reqwest::Response) -> u64 {
    resp.headers()
        .get("retry-after")
        .expect("Retry-After header")
        .to_str()
        .unwrap()
        .parse()
        .expect("numeric Retry-After")
}

fn assert_nasiko_budget(body: &Value, budget_id: &str, scope: &str, period: &str) {
    let nb = &body["nasiko_budget"];
    assert_eq!(nb["budget_id"], budget_id, "{body}");
    assert_eq!(nb["scope"], scope, "{body}");
    assert_eq!(nb["period"], period, "{body}");
    assert!(nb["limit_usd"].as_f64().is_some(), "{body}");
    assert!(nb["spend_usd"].as_f64().is_some(), "{body}");
    nb["resets_at"]
        .as_str()
        .and_then(|s| s.parse::<DateTime<Utc>>().ok())
        .unwrap_or_else(|| panic!("resets_at not RFC 3339: {body}"));
}

#[tokio::test]
#[serial]
async fn block_budget_rejects_every_dialect_before_upstream() {
    let mut upstream = mockito::Server::new_async().await;
    let chat_mock = upstream
        .mock("POST", "/chat/completions")
        .expect(0)
        .create_async()
        .await;
    let embed_mock = upstream
        .mock("POST", "/embeddings")
        .expect(0)
        .create_async()
        .await;
    set_router_env(&upstream.url());

    let server = TestServer::start().await;
    let root = seed_root(&server).await;
    let c = caller(&server, "dialect-user").await;
    seed_usage(
        &server,
        c.user,
        None,
        "direct_llm",
        0.001,
        in_current_period(),
    )
    .await;
    let budget = create_budget(&server, root, user_budget("dialects", c.user, 0.0001)).await;
    let id = budget["id"].as_str().unwrap().to_owned();
    del_key(&current_key(&id)).await;

    let anthropic = json!({"model": "claude-3-5-sonnet-latest", "max_tokens": 16,
                           "messages": [{"role": "user", "content": "hi"}]});
    let gemini = json!({"contents": [{"role": "user", "parts": [{"text": "hi"}]}]});
    let responses = json!({"model": "gpt-4o-mini", "input": "hi"});
    let cases: [(&str, Value); 5] = [
        ("/v1/chat/completions", chat_body()),
        ("/v1/messages", anthropic),
        ("/v1beta/models/gemini-2.0-flash:generateContent", gemini),
        ("/v1/responses", responses),
        ("/v1/embeddings", embeddings_body()),
    ];
    for (path, body) in cases {
        // Taken before the call so the bound below can only be looser than the
        // server's own clock reading (Retry-After rounds up).
        let sent_at = Utc::now();
        let resp = post_llm(&server, path, &c.jwt, Some(&c.traceparent), &body).await;
        assert_eq!(resp.status(), 429, "{path}");
        let secs = retry_after(&resp);
        let json = resp.json::<Value>().await.unwrap();
        assert_nasiko_budget(&json, &id, "user", "monthly");
        let resets: DateTime<Utc> = json["nasiko_budget"]["resets_at"]
            .as_str()
            .unwrap()
            .parse()
            .unwrap();
        let until_reset = (resets - sent_at).num_milliseconds().div_euclid(1000) + 1;
        assert!(
            secs > 0 && (secs as i64) <= until_reset + 1,
            "{path}: Retry-After {secs} vs {until_reset}"
        );
        match path {
            "/v1/messages" => {
                assert_eq!(json["type"], "error", "{json}");
                assert_eq!(json["error"]["type"], "rate_limit_error", "{json}");
            }
            p if p.contains("generateContent") => {
                assert_eq!(json["error"]["code"], 429, "{json}");
                assert_eq!(json["error"]["status"], "RESOURCE_EXHAUSTED", "{json}");
            }
            _ => {
                assert_eq!(json["error"]["type"], "insufficient_quota", "{json}");
                assert_eq!(json["error"]["code"], "budget_exceeded", "{json}");
                assert!(json["error"]["param"].is_null(), "{json}");
            }
        }
    }
    chat_mock.assert_async().await;
    embed_mock.assert_async().await;
    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn agent_and_platform_budgets_block() {
    let _upstream = stub_upstream_big_usage().await;
    let server = TestServer::start().await;
    let root = seed_root(&server).await;
    let a = caller(&server, "scope-a").await;
    let b = caller(&server, "scope-b").await;

    let agent_budget = create_budget(
        &server,
        root,
        json!({"name": "agent cap", "scope": "agent", "target_id": a.agent,
               "period": "monthly", "limit_usd": 0.0001, "action": "block"}),
    )
    .await;
    let agent_id = agent_budget["id"].as_str().unwrap().to_owned();
    seed_usage(
        &server,
        a.user,
        Some(a.agent),
        "direct_llm",
        0.001,
        in_current_period(),
    )
    .await;
    del_key(&current_key(&agent_id)).await;

    let resp = chat(&server, &a).await;
    assert_eq!(resp.status(), 429);
    let body = resp.json::<Value>().await.unwrap();
    assert_nasiko_budget(&body, &agent_id, "agent", "monthly");
    assert_eq!(
        chat(&server, &b).await.status(),
        200,
        "other agent unaffected"
    );

    let platform = create_budget(
        &server,
        root,
        json!({"name": "platform cap", "scope": "platform",
               "period": "monthly", "limit_usd": 0.0001, "action": "block"}),
    )
    .await;
    let platform_id = platform["id"].as_str().unwrap().to_owned();
    del_key(&current_key(&platform_id)).await;
    for c in [&a, &b] {
        let resp = chat(&server, c).await;
        assert_eq!(resp.status(), 429);
    }
    let resp = chat(&server, &b).await;
    let body = resp.json::<Value>().await.unwrap();
    assert_nasiko_budget(&body, &platform_id, "platform", "monthly");
    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn disabled_budget_is_ignored() {
    let _upstream = stub_upstream_big_usage().await;
    let server = TestServer::start().await;
    let root = seed_root(&server).await;
    let c = caller(&server, "disabled-user").await;
    seed_usage(
        &server,
        c.user,
        None,
        "direct_llm",
        0.001,
        in_current_period(),
    )
    .await;
    let mut body = user_budget("disabled", c.user, 0.0001);
    body["enabled"] = json!(false);
    let budget = create_budget(&server, root, body).await;
    del_key(&current_key(budget["id"].as_str().unwrap())).await;

    assert_eq!(chat(&server, &c).await.status(), 200);
    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn sequential_calls_never_pass_after_exhaustion() {
    let _upstream = stub_upstream_big_usage().await;
    let server = TestServer::start().await;
    let root = seed_root(&server).await;
    let c = caller(&server, "enf06-user").await;
    let budget = create_budget(&server, root, user_budget("enf06", c.user, 0.0001)).await;
    let id = budget["id"].as_str().unwrap().to_owned();

    assert_eq!(
        chat(&server, &c).await.status(),
        200,
        "first call is within budget"
    );

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
    loop {
        let view = get_budget(&server, root, &id).await;
        let spend = view["spend_usd"].as_f64().unwrap_or(0.0);
        if spend >= view["limit_usd"].as_f64().unwrap() {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "counter did not reflect the call within 1s (BUDG-03)"
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    for n in 0..3 {
        assert_eq!(
            chat(&server, &c).await.status(),
            429,
            "call {n} after exhaustion"
        );
    }
    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn counter_increments_within_one_second() {
    let _upstream = stub_upstream_big_usage().await;
    let server = TestServer::start().await;
    let root = seed_root(&server).await;
    let c = caller(&server, "incr-user").await;
    let budget = create_budget(&server, root, user_budget("incr", c.user, 1000.0)).await;
    let id = budget["id"].as_str().unwrap().to_owned();
    // The status read materializes the counter; increments never create keys.
    get_budget(&server, root, &id).await;
    let key = current_key(&id);
    let mut last = get_key(&key).await.expect("counter materialized");

    let calls: [(&str, &str, Value); 3] = [
        ("non-stream chat", "/v1/chat/completions", chat_body()),
        ("stream chat", "/v1/chat/completions", stream_chat_body()),
        ("embeddings", "/v1/embeddings", embeddings_body()),
    ];
    for (label, path, body) in calls {
        let resp = post_llm(&server, path, &c.jwt, Some(&c.traceparent), &body).await;
        assert_eq!(resp.status(), 200, "{label}");
        let _ = resp.bytes().await.expect("drain body");
        let before = last;
        let now = wait_for_key(&key, std::time::Duration::from_secs(1), |v| {
            v.is_some_and(|v| v > before)
        })
        .await
        .unwrap_or(before);
        assert!(now > before, "{label}: counter stayed at {before}");
        last = now;
    }
    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn flush_rebuilds_and_still_blocks() {
    let _upstream = stub_upstream_big_usage().await;
    let server = TestServer::start().await;
    let root = seed_root(&server).await;
    let c = caller(&server, "flush-user").await;
    seed_usage(
        &server,
        c.user,
        None,
        "direct_llm",
        0.002,
        in_current_period(),
    )
    .await;
    let budget = create_budget(&server, root, user_budget("flush", c.user, 0.001)).await;
    let id = budget["id"].as_str().unwrap().to_owned();
    let key = current_key(&id);

    get_budget(&server, root, &id).await;
    assert_eq!(get_key(&key).await, Some(2_000));
    del_key(&key).await;

    assert_eq!(chat(&server, &c).await.status(), 429);
    assert_eq!(get_key(&key).await, Some(2_000), "rebuilt from token_usage");
    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn store_down_fails_closed() {
    let _upstream = stub_upstream_big_usage().await;
    let server = TestServer::start().await;
    let root = seed_root(&server).await;
    let a = caller(&server, "down-a").await;
    let b = caller(&server, "down-b").await;
    create_budget(&server, root, user_budget("down", a.user, 1000.0)).await;

    // A second router over the same database whose counter store is unreachable.
    let engine = nasiko_llm_router::budget::BudgetEngine::new(
        server.db.clone(),
        Some(redis::Client::open("redis://127.0.0.1:1").expect("redis url")),
    );
    let ctx =
        nasiko_llm_router::LlmRouterCtx::from_shared(server.db.clone(), reqwest::Client::new())
            .with_budgets(std::sync::Arc::new(engine));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    tokio::spawn(async move {
        axum::serve(listener, nasiko_llm_router::router(ctx))
            .await
            .unwrap();
    });
    let client = reqwest::Client::new();

    let started = std::time::Instant::now();
    let resp = post_llm_at(
        &base,
        &client,
        "/v1/chat/completions",
        &a.jwt,
        Some(&a.traceparent),
        &chat_body(),
    )
    .await;
    assert_eq!(resp.status(), 503);
    assert!(
        started.elapsed() < std::time::Duration::from_secs(1),
        "fails fast"
    );
    let body = resp.json::<Value>().await.unwrap();
    assert_eq!(body["error"]["code"], "budget_store_unavailable", "{body}");
    assert!(body.get("nasiko_budget").is_none(), "{body}");

    let resp = post_llm_at(
        &base,
        &client,
        "/v1/chat/completions",
        &b.jwt,
        Some(&b.traceparent),
        &chat_body(),
    )
    .await;
    assert_eq!(
        resp.status(),
        200,
        "no applicable budget: store not consulted"
    );
    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn check_makes_one_mget() {
    use nasiko_llm_router::budget::{BudgetEngine, BudgetSubject, DowngradePolicy};

    let server = TestServer::start().await;
    let root = seed_root(&server).await;
    let user = seed_user(&server, "mget-user", "member").await;
    let other = seed_user(&server, "mget-other", "member").await;
    let agent = seed_agent(&server, user, "mget-agent").await;
    let engine = BudgetEngine::new(server.db.clone(), Some(redis_client()));
    let subject = |user_id| BudgetSubject {
        user_id: Some(user_id),
        agent_id: Some(agent),
        downgrade: DowngradePolicy::Allowed,
    };
    let now = Utc::now();

    // Only a user budget for `user` exists: a subject matching nothing makes no Redis call.
    create_budget(&server, root, user_budget("mget user", user, 1000.0)).await;
    let before = engine.store_stats().mget_calls;
    engine.check(&subject(other), now).await.expect("check");
    assert_eq!(
        engine.store_stats().mget_calls,
        before,
        "unbudgeted subject"
    );

    create_budget(
        &server,
        root,
        json!({"name": "mget agent", "scope": "agent", "target_id": agent,
               "period": "monthly", "limit_usd": 1000.0, "action": "block"}),
    )
    .await;
    create_budget(
        &server,
        root,
        json!({"name": "mget platform", "scope": "platform",
               "period": "monthly", "limit_usd": 1000.0, "action": "block"}),
    )
    .await;
    engine.invalidate().await;
    let defs = engine.definitions().await.unwrap();
    let all: Vec<&_> = defs.iter().collect();
    assert_eq!(all.len(), 3);
    engine.spend_micros(&all, now).await.expect("prime keys");

    let before = engine.store_stats().mget_calls;
    engine.check(&subject(user), now).await.expect("check");
    assert_eq!(
        engine.store_stats().mget_calls,
        before + 1,
        "one MGET for 3 budgets"
    );
    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn coding_agent_call_is_blocked() {
    let _upstream = stub_upstream_big_usage().await;
    let server = TestServer::start().await;
    let root = seed_root(&server).await;
    let user = seed_user(&server, "coding-owner", "member").await;
    let agent = seed_agent(&server, user, "coding-agent").await;
    sqlx::query("UPDATE agents SET coding_agent_integration_id = 'claude' WHERE id = $1")
        .bind(agent)
        .execute(&server.db)
        .await
        .expect("mark coding agent");
    let jwt = router_agent_jwt(agent, user);
    let budget = create_budget(&server, root, user_budget("coding", user, 1000.0)).await;
    let id = budget["id"].as_str().unwrap().to_owned();
    get_budget(&server, root, &id).await;
    let key = current_key(&id);
    let before = get_key(&key).await.expect("counter materialized");

    let resp = post_llm(&server, "/v1/chat/completions", &jwt, None, &chat_body()).await;
    assert_eq!(resp.status(), 200, "coding agent needs no traceparent");
    let after = wait_for_key(&key, std::time::Duration::from_secs(1), |v| {
        v.is_some_and(|v| v > before)
    })
    .await;
    assert!(after.is_some_and(|v| v > before), "billed to the JWT owner");

    // Exhaust the budget: the next call without a traceparent is blocked.
    let mut tight = json!({"limit_usd": 0.0001});
    tight["enabled"] = json!(true);
    let resp = admin(
        root,
        server.client.put(server.url(&format!("/api/budgets/{id}"))),
    )
    .json(&tight)
    .send()
    .await
    .unwrap();
    assert_eq!(resp.status(), 200);
    let resp = post_llm(&server, "/v1/chat/completions", &jwt, None, &chat_body()).await;
    assert_eq!(resp.status(), 429);
    let body = resp.json::<Value>().await.unwrap();
    assert_nasiko_budget(&body, &id, "user", "monthly");
    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn block_ensures_hard_limit_event() {
    let _upstream = stub_upstream_big_usage().await;
    let server = TestServer::start().await;
    let root = seed_root(&server).await;
    let c = caller(&server, "event-user").await;
    let budget = create_budget(&server, root, user_budget("event", c.user, 0.001)).await;
    let id = budget["id"].as_str().unwrap().to_owned();
    let key = current_key(&id);
    // Set directly: no rebuild and no crossing, so nothing has written an event yet.
    let mut conn = redis_client()
        .get_multiplexed_async_connection()
        .await
        .expect("redis conn");
    let _: () = conn
        .set_ex(&key, 1_001_i64, 3600)
        .await
        .expect("set counter");
    assert!(event_kinds(&server, &id).await.is_empty());

    assert_eq!(chat(&server, &c).await.status(), 429);
    let hard = |kinds: Vec<String>| kinds.iter().filter(|k| *k == "hard_limit").count();
    assert_eq!(hard(event_kinds(&server, &id).await), 1);
    for _ in 0..2 {
        assert_eq!(chat(&server, &c).await.status(), 429);
    }
    assert_eq!(hard(event_kinds(&server, &id).await), 1);
    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn increment_failure_forces_rebuild() {
    use nasiko_llm_router::budget::BudgetEngine;

    let server = TestServer::start().await;
    let root = seed_root(&server).await;
    let user = seed_user(&server, "incr-fail-user", "member").await;
    let budget = create_budget(&server, root, user_budget("incr fail", user, 100.0)).await;
    let id = budget["id"].as_str().unwrap().to_owned();
    let key = current_key(&id);
    let engine = BudgetEngine::new(server.db.clone(), Some(redis_client()));
    let now = Utc::now();
    let defs = engine.definitions().await.unwrap();
    let all: Vec<&_> = defs.iter().collect();
    del_key(&key).await;
    assert_eq!(engine.spend_micros(&all, now).await.unwrap(), vec![0]);

    seed_usage(&server, user, None, "direct_llm", 0.5, in_current_period()).await;
    engine.fail_next_increment();
    let outcome = engine.record(user, None, 500_000, now).await;
    drop(outcome);

    assert_eq!(
        engine.spend_micros(&all, now).await.unwrap(),
        vec![500_000],
        "rebuilt from token_usage, not the stale 0"
    );
    server.cleanup().await;
}

// ─── downgrade (ENF-03) ──────────────────────────────────────────────────────

const DOWNGRADED_HEADER: &str = "x-nasiko-budget-downgraded";
const ORIGINAL_MODEL_HEADER: &str = "x-nasiko-original-model";
const PRICEY_MODEL: &str = "gpt-4o";
const CHEAP_MODEL: &str = "gpt-4o-mini";

/// Operator Tier3 override for `provider` (what the router downgrades to).
async fn seed_tier3(server: &TestServer, provider: &str, model: &str) {
    sqlx::query(
        "INSERT INTO model_registry (provider, tier, model) VALUES ($1, 3, $2) \
         ON CONFLICT (provider, tier) DO UPDATE SET model = EXCLUDED.model",
    )
    .bind(provider)
    .bind(model)
    .execute(&server.db)
    .await
    .expect("seed tier3");
}

async fn set_counter(key: &str, micros: i64) {
    let mut conn = redis_client()
        .get_multiplexed_async_connection()
        .await
        .expect("redis conn");
    let _: () = conn.set_ex(key, micros, 3600).await.expect("set counter");
}

async fn event_count(server: &TestServer, budget_id: &str, kind: &str) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*) FROM budget_events WHERE budget_id = $1::uuid AND kind = $2",
    )
    .bind(budget_id)
    .bind(kind)
    .fetch_one(&server.db)
    .await
    .expect("event count")
}

/// Poll until `kind` has `want` rows for the budget (or 2 s pass); returns the count.
async fn wait_event_count(server: &TestServer, budget_id: &str, kind: &str, want: i64) -> i64 {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        let n = event_count(server, budget_id, kind).await;
        if n >= want || std::time::Instant::now() >= deadline {
            return n;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
}

/// Poll until `n` router usage rows written after `since` exist for `user`.
async fn wait_usage_rows(server: &TestServer, user: Uuid, since: DateTime<Utc>, n: i64) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let got: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM token_usage WHERE user_id = $1 AND created_at >= $2",
        )
        .bind(user)
        .bind(since)
        .fetch_one(&server.db)
        .await
        .expect("usage count");
        if got >= n || std::time::Instant::now() >= deadline {
            assert!(got >= n, "expected {n} usage rows, saw {got}");
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
}

/// `(model, metadata)` of the newest usage row written after `since`.
async fn newest_usage_row(
    server: &TestServer,
    user: Uuid,
    since: DateTime<Utc>,
) -> (String, Value) {
    wait_usage_rows(server, user, since, 1).await;
    let (model, metadata): (String, String) = sqlx::query_as(
        "SELECT model, metadata::text FROM token_usage WHERE user_id = $1 AND created_at >= $2 \
         ORDER BY created_at DESC LIMIT 1",
    )
    .bind(user)
    .bind(since)
    .fetch_one(&server.db)
    .await
    .expect("usage row");
    (
        model,
        serde_json::from_str(&metadata).expect("metadata json"),
    )
}

/// Upstream that serves the cheap model with a large usage block and must never
/// see the pricey model. Mocks are registered pricey-last-irrelevant: the body
/// matchers are disjoint, so ordering does not matter.
struct DowngradeUpstream {
    /// Held only to keep the stub server alive for the test's duration.
    _server: mockito::ServerGuard,
    pricey_chat: mockito::Mock,
    pricey_responses: mockito::Mock,
    cheap_chat: mockito::Mock,
    cheap_stream: mockito::Mock,
    cheap_responses: mockito::Mock,
}

async fn downgrade_upstream() -> DowngradeUpstream {
    let mut upstream = mockito::Server::new_async().await;
    let model_is = |m: &str| mockito::Matcher::PartialJson(json!({"model": m}));
    let pricey_chat = upstream
        .mock("POST", "/chat/completions")
        .match_body(model_is(PRICEY_MODEL))
        .expect(0)
        .create_async()
        .await;
    let pricey_responses = upstream
        .mock("POST", "/responses")
        .match_body(model_is(PRICEY_MODEL))
        .expect(0)
        .create_async()
        .await;
    let cheap_chat = upstream
        .mock("POST", "/chat/completions")
        .match_body(model_is(CHEAP_MODEL))
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(format!(
            r#"{{"id":"chatcmpl-1","object":"chat.completion","created":1,"model":"{CHEAP_MODEL}",
                "choices":[{{"index":0,"message":{{"role":"assistant","content":"hi"}},
                            "finish_reason":"stop"}}],
                "usage":{{"prompt_tokens":{BIG_PROMPT_TOKENS},"completion_tokens":7,
                          "total_tokens":{}}}}}"#,
            BIG_PROMPT_TOKENS + 7
        ))
        .expect_at_least(0)
        .create_async()
        .await;
    let sse = format!(
        "data: {{\"id\":\"x\",\"object\":\"chat.completion.chunk\",\"model\":\"{CHEAP_MODEL}\",\
         \"choices\":[{{\"index\":0,\"delta\":{{\"role\":\"assistant\",\"content\":\"hi\"}}}}]}}\n\n\
         data: {{\"id\":\"x\",\"object\":\"chat.completion.chunk\",\"model\":\"{CHEAP_MODEL}\",\
         \"choices\":[{{\"index\":0,\"delta\":{{}},\"finish_reason\":\"stop\"}}],\
         \"usage\":{{\"prompt_tokens\":{BIG_PROMPT_TOKENS},\"completion_tokens\":7,\
         \"total_tokens\":{}}}}}\n\ndata: [DONE]\n\n",
        BIG_PROMPT_TOKENS + 7
    );
    let cheap_stream = upstream
        .mock("POST", "/chat/completions")
        .match_body(mockito::Matcher::PartialJson(
            json!({"model": CHEAP_MODEL, "stream": true}),
        ))
        .with_status(200)
        .with_header("content-type", "text/event-stream")
        .with_body(sse)
        .expect_at_least(0)
        .create_async()
        .await;
    let cheap_responses = upstream
        .mock("POST", "/responses")
        .match_body(model_is(CHEAP_MODEL))
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(format!(
            r#"{{"id":"resp_1","object":"response","status":"completed","model":"{CHEAP_MODEL}",
                "output":[{{"type":"message","role":"assistant","status":"completed",
                            "content":[{{"type":"output_text","text":"hi"}}]}}],
                "usage":{{"input_tokens":{BIG_PROMPT_TOKENS},"output_tokens":7,
                          "total_tokens":{}}}}}"#,
            BIG_PROMPT_TOKENS + 7
        ))
        .expect_at_least(0)
        .create_async()
        .await;
    upstream
        .mock("POST", "/embeddings")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(format!(
            r#"{{"object":"list","model":"text-embedding-3-small",
                "data":[{{"object":"embedding","index":0,"embedding":[0.1,0.2]}}],
                "usage":{{"prompt_tokens":{BIG_PROMPT_TOKENS},"total_tokens":{BIG_PROMPT_TOKENS}}}}}"#
        ))
        .expect_at_least(0)
        .create_async()
        .await;
    set_router_env(&upstream.url());
    DowngradeUpstream {
        _server: upstream,
        pricey_chat,
        pricey_responses,
        cheap_chat,
        cheap_stream,
        cheap_responses,
    }
}

fn downgrade_budget(name: &str, target: Uuid, limit: f64) -> Value {
    json!({
        "name": name, "scope": "user", "target_id": target, "period": "monthly",
        "limit_usd": limit, "action": "downgrade", "downgrade_ceiling_pct": 125,
    })
}

fn pricey_chat_body() -> Value {
    json!({"model": PRICEY_MODEL, "messages": [{"role": "user", "content": "hello"}]})
}

fn assert_downgrade_headers(resp: &reqwest::Response, budget_id: &str) {
    assert_eq!(
        resp.headers()
            .get(DOWNGRADED_HEADER)
            .and_then(|v| v.to_str().ok()),
        Some(budget_id),
        "x-nasiko-budget-downgraded"
    );
    assert_eq!(
        resp.headers()
            .get(ORIGINAL_MODEL_HEADER)
            .and_then(|v| v.to_str().ok()),
        Some(PRICEY_MODEL),
        "x-nasiko-original-model"
    );
}

/// A downgrade budget with `spend_usd` already in `token_usage` for the caller.
async fn exhausted_downgrade(
    server: &TestServer,
    root: Uuid,
    c: &Caller,
    spend_usd: f64,
) -> String {
    seed_usage(
        server,
        c.user,
        None,
        "direct_llm",
        spend_usd,
        in_current_period(),
    )
    .await;
    let budget = create_budget(server, root, downgrade_budget("downgrade", c.user, 1.0)).await;
    budget["id"].as_str().unwrap().to_owned()
}

#[tokio::test]
#[serial]
async fn downgrade_serves_cheapest_model() {
    let upstream = downgrade_upstream().await;
    let server = TestServer::start().await;
    let root = seed_root(&server).await;
    seed_tier3(&server, "openai", CHEAP_MODEL).await;
    let c = caller(&server, "dg-chat").await;
    let id = exhausted_downgrade(&server, root, &c, 1.1).await;
    let since = Utc::now();

    let resp = post_llm(
        &server,
        "/v1/chat/completions",
        &c.jwt,
        Some(&c.traceparent),
        &pricey_chat_body(),
    )
    .await;
    assert_eq!(resp.status(), 200);
    assert_downgrade_headers(&resp, &id);

    upstream.pricey_chat.assert_async().await;
    upstream.cheap_chat.assert_async().await;
    let (model, metadata) = newest_usage_row(&server, c.user, since).await;
    assert_eq!(model, CHEAP_MODEL);
    assert_eq!(
        metadata["budget_downgrade"],
        json!({"budget_id": id, "from_model": PRICEY_MODEL, "to_model": CHEAP_MODEL})
    );
    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn downgrade_stream_sets_headers() {
    let upstream = downgrade_upstream().await;
    let server = TestServer::start().await;
    let root = seed_root(&server).await;
    seed_tier3(&server, "openai", CHEAP_MODEL).await;
    let c = caller(&server, "dg-stream").await;
    let id = exhausted_downgrade(&server, root, &c, 1.1).await;
    let since = Utc::now();

    let mut body = pricey_chat_body();
    body["stream"] = json!(true);
    let resp = post_llm(
        &server,
        "/v1/chat/completions",
        &c.jwt,
        Some(&c.traceparent),
        &body,
    )
    .await;
    assert_eq!(resp.status(), 200);
    assert_downgrade_headers(&resp, &id);
    let text = resp.text().await.unwrap();
    assert!(text.contains("[DONE]"), "{text}");

    upstream.pricey_chat.assert_async().await;
    upstream.cheap_stream.assert_async().await;
    let (model, metadata) = newest_usage_row(&server, c.user, since).await;
    assert_eq!(model, CHEAP_MODEL);
    assert_eq!(metadata["budget_downgrade"]["budget_id"], id);
    assert_eq!(metadata["budget_downgrade"]["from_model"], PRICEY_MODEL);
    assert_eq!(metadata["budget_downgrade"]["to_model"], CHEAP_MODEL);
    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn downgrade_responses_surface() {
    let upstream = downgrade_upstream().await;
    let server = TestServer::start().await;
    let root = seed_root(&server).await;
    seed_tier3(&server, "openai", CHEAP_MODEL).await;
    let c = caller(&server, "dg-responses").await;
    let id = exhausted_downgrade(&server, root, &c, 1.1).await;
    let since = Utc::now();

    let resp = post_llm(
        &server,
        "/v1/responses",
        &c.jwt,
        Some(&c.traceparent),
        &json!({"model": PRICEY_MODEL, "input": [{"role": "user", "content": "hello"}]}),
    )
    .await;
    assert_eq!(resp.status(), 200);
    assert_downgrade_headers(&resp, &id);

    upstream.pricey_responses.assert_async().await;
    upstream.cheap_responses.assert_async().await;
    let (model, metadata) = newest_usage_row(&server, c.user, since).await;
    assert_eq!(model, CHEAP_MODEL);
    assert_eq!(metadata["budget_downgrade"]["budget_id"], id);
    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn downgrade_ceiling_blocks() {
    let _upstream = downgrade_upstream().await;
    let server = TestServer::start().await;
    let root = seed_root(&server).await;
    seed_tier3(&server, "openai", CHEAP_MODEL).await;
    let c = caller(&server, "dg-ceiling").await;
    let id = exhausted_downgrade(&server, root, &c, 1.3).await;

    let resp = post_llm(
        &server,
        "/v1/chat/completions",
        &c.jwt,
        Some(&c.traceparent),
        &pricey_chat_body(),
    )
    .await;
    assert_eq!(resp.status(), 429);
    assert!(resp.headers().get(DOWNGRADED_HEADER).is_none());
    let body = resp.json::<Value>().await.unwrap();
    assert_nasiko_budget(&body, &id, "user", "monthly");
    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn pinned_agent_downgrade_blocks() {
    let upstream = downgrade_upstream().await;
    let server = TestServer::start().await;
    let root = seed_root(&server).await;
    seed_tier3(&server, "openai", CHEAP_MODEL).await;
    let c = caller(&server, "dg-pinned").await;
    sqlx::query("UPDATE agents SET pinned_model = $2 WHERE id = $1")
        .bind(c.agent)
        .bind(PRICEY_MODEL)
        .execute(&server.db)
        .await
        .expect("pin agent");
    let id = exhausted_downgrade(&server, root, &c, 1.1).await;

    let resp = post_llm(
        &server,
        "/v1/chat/completions",
        &c.jwt,
        Some(&c.traceparent),
        &pricey_chat_body(),
    )
    .await;
    assert_eq!(
        resp.status(),
        429,
        "compliance-locked agents are blocked, not downgraded"
    );
    assert!(resp.headers().get(DOWNGRADED_HEADER).is_none());
    let body = resp.json::<Value>().await.unwrap();
    assert_nasiko_budget(&body, &id, "user", "monthly");
    upstream.pricey_chat.assert_async().await;
    upstream.cheap_chat.assert_async().await;
    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn embeddings_not_downgraded() {
    let _upstream = downgrade_upstream().await;
    let server = TestServer::start().await;
    let root = seed_root(&server).await;
    seed_tier3(&server, "openai", CHEAP_MODEL).await;
    let c = caller(&server, "dg-embed").await;
    let id = exhausted_downgrade(&server, root, &c, 1.1).await;
    let key = current_key(&id);

    let body = embeddings_body();
    let embed = || {
        post_llm(
            &server,
            "/v1/embeddings",
            &c.jwt,
            Some(&c.traceparent),
            &body,
        )
    };
    let resp = embed().await;
    assert_eq!(resp.status(), 200, "served as-is between limit and ceiling");
    assert!(resp.headers().get(DOWNGRADED_HEADER).is_none());

    // Push the counter to the ceiling (limit 1.0 x 125%).
    set_counter(&key, 1_300_000).await;
    let resp = embed().await;
    assert_eq!(resp.status(), 429);
    server.cleanup().await;
}

// ─── budget events (ENF-05) ──────────────────────────────────────────────────

#[tokio::test]
#[serial]
async fn soft_event_exactly_once() {
    let _upstream = stub_upstream_big_usage().await;
    let server = TestServer::start().await;
    let root = seed_root(&server).await;
    let c = caller(&server, "soft-once").await;
    let mut body = user_budget("soft once", c.user, 10.0);
    body["soft_threshold_pct"] = json!(50);
    let id = create_budget(&server, root, body).await["id"]
        .as_str()
        .unwrap()
        .to_owned();
    // 1 micro-USD below the soft threshold (50% of 10 USD): the first call's
    // increment crosses it; set directly so no rebuild event is involved.
    set_counter(&current_key(&id), 4_999_999).await;
    let since = Utc::now();

    let calls = (0..5).map(|_| chat(&server, &c));
    for resp in futures::future::join_all(calls).await {
        assert_eq!(resp.status(), 200, "soft never blocks");
    }
    for _ in 0..2 {
        assert_eq!(chat(&server, &c).await.status(), 200);
    }
    wait_usage_rows(&server, c.user, since, 7).await;
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    assert_eq!(wait_event_count(&server, &id, "soft_threshold", 1).await, 1);
    assert_eq!(event_count(&server, &id, "soft_threshold").await, 1);
    assert_eq!(event_count(&server, &id, "hard_limit").await, 0);
    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn hard_limit_event_once() {
    let _upstream = stub_upstream_big_usage().await;
    let server = TestServer::start().await;
    let root = seed_root(&server).await;
    let c = caller(&server, "hard-once").await;
    let id = create_budget(&server, root, user_budget("hard once", c.user, 1.0)).await["id"]
        .as_str()
        .unwrap()
        .to_owned();
    set_counter(&current_key(&id), 999_999).await;

    assert_eq!(chat(&server, &c).await.status(), 200);
    assert_eq!(wait_event_count(&server, &id, "hard_limit", 1).await, 1);
    assert_eq!(chat(&server, &c).await.status(), 429);
    assert_eq!(event_count(&server, &id, "hard_limit").await, 1);
    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn events_on_rebuild_path() {
    let _upstream = stub_upstream_big_usage().await;
    let server = TestServer::start().await;
    let root = seed_root(&server).await;
    let c = caller(&server, "rebuild-events").await;
    // Default soft threshold is 80%: 8.5 of 10 USD is above it, below the limit.
    seed_usage(
        &server,
        c.user,
        None,
        "direct_llm",
        8.5,
        in_current_period(),
    )
    .await;
    let id = create_budget(&server, root, user_budget("rebuild", c.user, 10.0)).await["id"]
        .as_str()
        .unwrap()
        .to_owned();
    del_key(&current_key(&id)).await;
    let since = Utc::now();

    assert_eq!(chat(&server, &c).await.status(), 200);
    assert_eq!(wait_event_count(&server, &id, "soft_threshold", 1).await, 1);
    for _ in 0..2 {
        assert_eq!(chat(&server, &c).await.status(), 200);
    }
    wait_usage_rows(&server, c.user, since, 3).await;
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert_eq!(event_count(&server, &id, "soft_threshold").await, 1);
    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn budget_created_over_threshold_emits_events() {
    let _upstream = downgrade_upstream().await;
    let server = TestServer::start().await;
    let root = seed_root(&server).await;
    seed_tier3(&server, "openai", CHEAP_MODEL).await;
    let c = caller(&server, "created-over").await;
    // Spend is 1.2x the limit before the budget exists, so its counter is first
    // created already past both levels.
    let id = exhausted_downgrade(&server, root, &c, 1.2).await;
    let since = Utc::now();

    let body = pricey_chat_body();
    let post = || {
        post_llm(
            &server,
            "/v1/chat/completions",
            &c.jwt,
            Some(&c.traceparent),
            &body,
        )
    };
    let resp = post().await;
    assert_eq!(resp.status(), 200, "downgraded, below the ceiling");
    assert_downgrade_headers(&resp, &id);
    assert_eq!(wait_event_count(&server, &id, "soft_threshold", 1).await, 1);
    assert_eq!(wait_event_count(&server, &id, "hard_limit", 1).await, 1);

    for _ in 0..2 {
        post().await;
    }
    wait_usage_rows(&server, c.user, since, 1).await;
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert_eq!(event_count(&server, &id, "soft_threshold").await, 1);
    assert_eq!(event_count(&server, &id, "hard_limit").await, 1);
    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn event_period_start() {
    let _upstream = stub_upstream_big_usage().await;
    let server = TestServer::start().await;
    let root = seed_root(&server).await;
    let c = caller(&server, "event-period").await;
    let id = create_budget(&server, root, user_budget("period", c.user, 1.0)).await["id"]
        .as_str()
        .unwrap()
        .to_owned();
    // 799_999 is 1 micro-USD below the default 80% soft threshold.
    set_counter(&current_key(&id), 799_999).await;

    assert_eq!(chat(&server, &c).await.status(), 200);
    assert_eq!(wait_event_count(&server, &id, "soft_threshold", 1).await, 1);
    let period_start: DateTime<Utc> =
        sqlx::query_scalar("SELECT period_start FROM budget_events WHERE budget_id = $1::uuid")
            .bind(&id)
            .fetch_one(&server.db)
            .await
            .expect("event row");
    assert_eq!(period_start, period_bounds(Period::Monthly, Utc::now()).0);
    server.cleanup().await;
}

// ─── end-to-end: block -> alert -> signed webhook ────────────────────────────

const E2E_HMAC_SECRET: &str = "e2e-secret";

type CapturedRequest = (Vec<(String, String)>, Vec<u8>);

/// Headers and body of the last request the webhook mock matched.
fn capture_slot() -> std::sync::Arc<std::sync::Mutex<Option<CapturedRequest>>> {
    std::sync::Arc::new(std::sync::Mutex::new(None))
}

#[tokio::test]
#[serial]
async fn block_to_signed_webhook_end_to_end() {
    let _upstream = stub_upstream_big_usage().await;
    let server = TestServer::start_with(|c| c.alerts.allow_private_urls = true).await;
    let root = seed_root(&server).await;

    let slot = capture_slot();
    let matcher_slot = slot.clone();
    let mut receiver = mockito::Server::new_async().await;
    let hook = receiver
        .mock("POST", "/hook")
        .match_request(move |req| {
            let headers = req
                .headers()
                .iter()
                .map(|(k, v)| (k.as_str().to_owned(), v.to_str().unwrap_or("").to_owned()))
                .collect();
            *matcher_slot.lock().unwrap() =
                Some((headers, req.body().cloned().unwrap_or_default()));
            true
        })
        .with_status(200)
        .expect(1)
        .create_async()
        .await;

    let resp = admin(
        root,
        server.client.post(server.url("/api/notification-channels")),
    )
    .json(&json!({
        "name": "e2e-hook", "kind": "webhook",
        "url": format!("{}/hook", receiver.url()), "hmac_secret": E2E_HMAC_SECRET,
    }))
    .send()
    .await
    .unwrap();
    assert_eq!(resp.status(), 201, "create channel");
    let channel: Uuid = resp.json::<Value>().await.unwrap()["data"]["id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    // Only budget_hard is routed so a soft_threshold event cannot add a second POST.
    let resp = admin(
        root,
        server
            .client
            .put(server.url(&format!("/api/notification-channels/{channel}/routes"))),
    )
    .json(&json!({"routes": [{"alert_kind": "budget_hard", "min_severity": "info"}]}))
    .send()
    .await
    .unwrap();
    assert_eq!(resp.status(), 200, "put routes");

    let c = caller(&server, "e2e-user").await;
    let id = create_budget(&server, root, user_budget("e2e", c.user, 0.0001)).await["id"]
        .as_str()
        .unwrap()
        .to_owned();

    assert_eq!(chat(&server, &c).await.status(), 200, "first call passes");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
    loop {
        let view = get_budget(&server, root, &id).await;
        if view["spend_usd"].as_f64().unwrap_or(0.0) >= view["limit_usd"].as_f64().unwrap() {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "counter did not reflect the call within 1s"
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert_eq!(chat(&server, &c).await.status(), 429, "next call blocked");
    assert_eq!(wait_event_count(&server, &id, "hard_limit", 1).await, 1);

    assert!(tick_budget_events(&server.db, Utc::now()).await.unwrap() >= 1);
    let alerts: Vec<Value> =
        sqlx::query_scalar("SELECT to_jsonb(a) FROM alerts a WHERE kind = 'budget_hard'")
            .fetch_all(&server.db)
            .await
            .unwrap();
    assert_eq!(alerts.len(), 1);
    let alert = &alerts[0];
    assert_eq!(alert["status"], "open");
    assert!(alert["dedup_key"].as_str().unwrap().ends_with(":hard"));
    assert_eq!(alert["link"], "/tokenops?range=30d");
    assert_eq!(alert["details"]["budget_url"], "/budgets");

    let outbox_ids: Vec<Uuid> =
        sqlx::query_scalar("SELECT id FROM notification_outbox WHERE channel_id = $1")
            .bind(channel)
            .fetch_all(&server.db)
            .await
            .unwrap();
    assert_eq!(outbox_ids.len(), 1, "exactly one outbox row");

    let mut cfg = AlertsConfig::disabled();
    cfg.allow_private_urls = true;
    let stats = tick_outbox_dispatch(&server.db, &DispatchDeps::from_config(&cfg))
        .await
        .unwrap();
    assert_eq!((stats.claimed, stats.delivered), (1, 1));
    hook.assert_async().await;

    let (headers, body) = slot.lock().unwrap().clone().expect("webhook captured");
    let header = |name: &str| {
        headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.clone())
    };
    let ts: i64 = header("x-nasiko-timestamp").unwrap().parse().unwrap();
    let mut mac = Hmac::<Sha256>::new_from_slice(E2E_HMAC_SECRET.as_bytes()).unwrap();
    mac.update(format!("{ts}.").as_bytes());
    mac.update(&body);
    let expected = format!("sha256={}", hex::encode(mac.finalize().into_bytes()));
    assert_eq!(header("x-nasiko-signature"), Some(expected));

    let v: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["alert"]["kind"], "budget_hard");
    assert_eq!(v["alert"]["link"], "/tokenops?range=30d");
    let status: String = sqlx::query_scalar("SELECT status FROM notification_outbox WHERE id = $1")
        .bind(outbox_ids[0])
        .fetch_one(&server.db)
        .await
        .unwrap();
    assert_eq!(status, "delivered");
    server.cleanup().await;
}
