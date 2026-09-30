//! Budget CRUD, admin gate, `/api/budgets/me` scoping and live status.
//!
//! Covers the `/api/budgets` REST surface and the shared budget engine it reads
//! through: spend comes from Redis counters that are rebuilt from router-metered
//! `token_usage` (`operation_type IN ('direct_llm','embedding')`) when a key is
//! missing, and a counter first materialized at or above a threshold writes its
//! `budget_events` row exactly once. Router enforcement sections are appended
//! under their own banners by later plans.
//!
//! Requires infra (Postgres, Redis, S3 emulator):
//!   cargo test -p nasiko-server --test budgets -- --test-threads=1

mod common;

use chrono::{DateTime, Datelike, TimeZone, Utc};
use common::TestServer;
use nasiko_llm_router::budget::keys::spend_key;
use nasiko_llm_router::budget::period::{Period, period_bounds};
use redis::AsyncCommands;
use serde_json::{Value, json};
use serial_test::serial;
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
