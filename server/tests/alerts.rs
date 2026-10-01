//! Phase 3 alerting integration tests.
//!
//! Covers the alert engine (dedup, escalation, route-matched outbox enqueue),
//! the `budget_events` consumer, the resolver sweep and the admin alerts API.
//! Every `alerts.*_secs` interval is 0 in the test config, so no background
//! worker races the tests: they drive `tick_*` directly.
//!
//! The harness pool has `max_connections(1)`, so a test that holds a pooled
//! connection must not touch `server.db` (or the HTTP surface) until it
//! releases it; concurrency scenarios open extra connections from `db_url`.
//!
//! Requires infra (Postgres, Redis, S3 emulator):
//!   cargo test -p nasiko-server --test alerts -- --test-threads=1

mod common;

use std::sync::atomic::Ordering;
use std::time::Duration;

use chrono::{DateTime, Datelike, SecondsFormat, TimeZone, Utc};
use common::TestServer;
use nasiko_server::alerts::engine::{RESOLVE_BETWEEN_RAISE_STEPS, raise, resolve};
use nasiko_server::alerts::models::{AlertKind, AlertScope, NewAlert, RaiseOutcome, Severity};
use nasiko_server::alerts::{tick_budget_events, tick_resolve_sweep};
use serde_json::{Value, json};
use serial_test::serial;
use sqlx::Connection;
use uuid::Uuid;

// ─── helpers ─────────────────────────────────────────────────────────────────

async fn seed_user(server: &TestServer, name: &str, role: &str) -> Uuid {
    sqlx::query_scalar(
        "INSERT INTO users (username, email, role) VALUES ($1, $2, $3::user_role) RETURNING id",
    )
    .bind(name)
    .bind(format!("{name}@alerts.test"))
    .bind(role)
    .fetch_one(&server.db)
    .await
    .expect("seed user")
}

/// `require_auth` resolves the caller from `users`, so the superuser needs a row.
async fn seed_root(server: &TestServer) -> Uuid {
    sqlx::query_scalar(
        "INSERT INTO users (username, email, is_superuser) VALUES ('alerts-root', \
         'alerts-root@alerts.test', true) RETURNING id",
    )
    .fetch_one(&server.db)
    .await
    .expect("seed root")
}

fn admin(root: Uuid, rb: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
    common::as_superuser(rb, &root.to_string(), "alerts-root")
}

fn member(rb: reqwest::RequestBuilder, id: Uuid, name: &str) -> reqwest::RequestBuilder {
    common::as_member(rb, &id.to_string(), name)
}

async fn create_budget(server: &TestServer, root: Uuid, body: Value) -> Value {
    let resp = admin(root, server.client.post(server.url("/api/budgets")))
        .json(&body)
        .send()
        .await
        .expect("post budget");
    assert_eq!(resp.status(), 201, "create budget");
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

fn next_month_start(now: DateTime<Utc>) -> DateTime<Utc> {
    let (y, m) = if now.month() == 12 {
        (now.year() + 1, 1)
    } else {
        (now.year(), now.month() + 1)
    };
    Utc.with_ymd_and_hms(y, m, 1, 0, 0, 0).unwrap()
}

async fn insert_event(
    server: &TestServer,
    budget_id: &str,
    period_start: DateTime<Utc>,
    kind: &str,
    spend: f64,
    limit: f64,
) {
    sqlx::query(
        "INSERT INTO budget_events (budget_id, period_start, kind, spend_usd, limit_usd) \
         VALUES ($1::uuid, $2, $3, $4::float8::numeric, $5::float8::numeric)",
    )
    .bind(budget_id)
    .bind(period_start)
    .bind(kind)
    .bind(spend)
    .bind(limit)
    .execute(&server.db)
    .await
    .expect("insert budget event");
}

async fn alert_rows(server: &TestServer) -> Vec<Value> {
    sqlx::query_scalar::<_, Value>("SELECT to_jsonb(a) FROM alerts a ORDER BY first_seen_at, id")
        .fetch_all(&server.db)
        .await
        .expect("alerts")
}

async fn alert_by_key(server: &TestServer, key: &str) -> Vec<Value> {
    sqlx::query_scalar::<_, Value>(
        "SELECT to_jsonb(a) FROM alerts a WHERE dedup_key = $1 ORDER BY first_seen_at, id",
    )
    .bind(key)
    .fetch_all(&server.db)
    .await
    .expect("alerts by key")
}

fn new_alert(key: &str, kind: AlertKind, severity: Severity) -> NewAlert {
    NewAlert {
        kind,
        severity,
        scope: AlertScope::Platform,
        scope_ref: None,
        dedup_key: key.to_owned(),
        title: format!("title {key}"),
        message: format!("message {key}"),
        link: "/budgets".to_owned(),
        details: json!({"k": key}),
    }
}

async fn seed_channel(server: &TestServer, name: &str, enabled: bool) -> Uuid {
    sqlx::query_scalar(
        "INSERT INTO notification_channels (name, kind, config_encrypted, url_hint, enabled) \
         VALUES ($1, 'webhook', 'x', 'h', $2) RETURNING id",
    )
    .bind(name)
    .bind(enabled)
    .fetch_one(&server.db)
    .await
    .expect("seed channel")
}

async fn seed_route(server: &TestServer, channel: Uuid, kind: Option<&str>, min: &str) {
    sqlx::query(
        "INSERT INTO notification_routes (channel_id, alert_kind, min_severity) \
         VALUES ($1, $2, $3)",
    )
    .bind(channel)
    .bind(kind)
    .bind(min)
    .execute(&server.db)
    .await
    .expect("INSERT INTO notification_routes");
}

async fn outbox(server: &TestServer) -> Vec<Value> {
    sqlx::query_scalar::<_, Value>(
        "SELECT to_jsonb(o) FROM notification_outbox o ORDER BY created_at, id",
    )
    .fetch_all(&server.db)
    .await
    .expect("outbox")
}

// ─── budget alerts ───────────────────────────────────────────────────────────

#[tokio::test]
#[serial]
async fn budget_soft_event_raises_one_warning_alert() {
    let server = TestServer::start().await;
    let root = seed_root(&server).await;
    let user = seed_user(&server, "soft-user", "member").await;
    let budget = create_budget(&server, root, user_budget("soft budget", user, 10.0)).await;
    let budget_id = budget["id"].as_str().unwrap().to_owned();

    let now = Utc::now();
    let start = month_start(now);
    insert_event(&server, &budget_id, start, "soft_threshold", 8.5, 10.0).await;

    assert_eq!(tick_budget_events(&server.db, now).await.unwrap(), 1);

    let rows = alert_rows(&server).await;
    assert_eq!(rows.len(), 1);
    let a = &rows[0];
    assert_eq!(a["kind"], "budget_soft");
    assert_eq!(a["severity"], "warning");
    assert_eq!(a["scope"], "user");
    assert_eq!(a["scope_ref"], user.to_string());
    assert_eq!(a["status"], "open");
    assert_eq!(a["link"], "/budgets");
    assert_eq!(a["occurrences"], 1);
    assert_eq!(
        a["dedup_key"],
        format!("budget:{budget_id}:{}:soft", start.to_rfc3339())
    );
    let d = &a["details"];
    assert_eq!(d["budget_id"], budget_id);
    assert!(d["period_start"].is_string());
    assert_eq!(
        DateTime::parse_from_rfc3339(d["period_end"].as_str().unwrap()).unwrap(),
        next_month_start(now)
    );
    assert!((d["limit_usd"].as_f64().unwrap() - 10.0).abs() < 1e-6);
    assert!((d["spend_usd"].as_f64().unwrap() - 8.5).abs() < 1e-6);

    let stamped: bool = sqlx::query_scalar("SELECT alerted_at IS NOT NULL FROM budget_events")
        .fetch_one(&server.db)
        .await
        .unwrap();
    assert!(stamped, "event must be stamped alerted_at");

    assert_eq!(tick_budget_events(&server.db, now).await.unwrap(), 0);
    assert_eq!(
        alert_rows(&server).await,
        rows,
        "second tick changes nothing"
    );
    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn budget_hard_event_raises_critical() {
    let server = TestServer::start().await;
    let root = seed_root(&server).await;
    let user = seed_user(&server, "hard-user", "member").await;
    let budget = create_budget(&server, root, user_budget("hard budget", user, 10.0)).await;
    let budget_id = budget["id"].as_str().unwrap().to_owned();

    let now = Utc::now();
    let start = month_start(now);
    insert_event(&server, &budget_id, start, "hard_limit", 10.0, 10.0).await;

    assert_eq!(tick_budget_events(&server.db, now).await.unwrap(), 1);
    let rows = alert_rows(&server).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["kind"], "budget_hard");
    assert_eq!(rows[0]["severity"], "critical");
    assert_eq!(
        rows[0]["dedup_key"],
        format!("budget:{budget_id}:{}:hard", start.to_rfc3339())
    );
    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn expired_period_event_is_stamped_not_raised() {
    let server = TestServer::start().await;
    let root = seed_root(&server).await;
    let user = seed_user(&server, "old-user", "member").await;
    let budget = create_budget(&server, root, user_budget("old budget", user, 10.0)).await;
    let budget_id = budget["id"].as_str().unwrap().to_owned();

    let now = Utc::now();
    let last_month = month_start(month_start(now) - chrono::Duration::days(1));
    insert_event(&server, &budget_id, last_month, "hard_limit", 10.0, 10.0).await;

    assert_eq!(tick_budget_events(&server.db, now).await.unwrap(), 0);
    assert!(alert_rows(&server).await.is_empty());
    let stamped: bool = sqlx::query_scalar("SELECT alerted_at IS NOT NULL FROM budget_events")
        .fetch_one(&server.db)
        .await
        .unwrap();
    assert!(stamped);
    server.cleanup().await;
}

// ─── engine: dedup, escalation, concurrency ──────────────────────────────────

#[tokio::test]
#[serial]
async fn raise_dedups_and_escalates() {
    let server = TestServer::start().await;
    let key = "dedup:one";
    let mut conn = server.db.acquire().await.unwrap();

    let first = raise(
        &mut conn,
        &new_alert(key, AlertKind::BudgetSoft, Severity::Warning),
    )
    .await
    .unwrap();
    let RaiseOutcome::Opened(id) = first else {
        panic!("expected Opened, got {first:?}");
    };
    let second = raise(
        &mut conn,
        &new_alert(key, AlertKind::BudgetSoft, Severity::Warning),
    )
    .await
    .unwrap();
    assert_eq!(second, RaiseOutcome::Repeated(id));

    let (occ, rows): (i32, i64) =
        sqlx::query_as("SELECT max(occurrences), count(*) FROM alerts WHERE dedup_key = $1")
            .bind(key)
            .fetch_one(&mut *conn)
            .await
            .unwrap();
    assert_eq!((occ, rows), (2, 1));

    let up = raise(
        &mut conn,
        &new_alert(key, AlertKind::BudgetSoft, Severity::Critical),
    )
    .await
    .unwrap();
    assert_eq!(up, RaiseOutcome::Escalated(id));
    let sev: String = sqlx::query_scalar("SELECT severity FROM alerts WHERE id = $1")
        .bind(id)
        .fetch_one(&mut *conn)
        .await
        .unwrap();
    assert_eq!(sev, "critical");

    let down = raise(
        &mut conn,
        &new_alert(key, AlertKind::BudgetSoft, Severity::Info),
    )
    .await
    .unwrap();
    assert_eq!(down, RaiseOutcome::Repeated(id));
    let sev: String = sqlx::query_scalar("SELECT severity FROM alerts WHERE id = $1")
        .bind(id)
        .fetch_one(&mut *conn)
        .await
        .unwrap();
    assert_eq!(sev, "critical", "severity never moves downward");

    assert_eq!(resolve(&mut conn, key).await.unwrap(), Some(id));
    let reopened = raise(
        &mut conn,
        &new_alert(key, AlertKind::BudgetSoft, Severity::Warning),
    )
    .await
    .unwrap();
    let RaiseOutcome::Opened(new_id) = reopened else {
        panic!("expected Opened after resolve, got {reopened:?}");
    };
    assert_ne!(new_id, id);
    drop(conn);
    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn raise_retry_once_after_concurrent_resolve() {
    let server = TestServer::start().await;
    let key = "retry:one";
    let mut conn = server.db.acquire().await.unwrap();

    let RaiseOutcome::Opened(old_id) = raise(
        &mut conn,
        &new_alert(key, AlertKind::MonitorBreach, Severity::Warning),
    )
    .await
    .unwrap() else {
        panic!("expected Opened");
    };

    RESOLVE_BETWEEN_RAISE_STEPS.store(true, Ordering::SeqCst);
    let outcome = raise(
        &mut conn,
        &new_alert(key, AlertKind::MonitorBreach, Severity::Warning),
    )
    .await
    .unwrap();
    let RaiseOutcome::Opened(new_id) = outcome else {
        panic!("expected Opened after retry, got {outcome:?}");
    };
    assert_ne!(new_id, old_id);
    assert!(
        !RESOLVE_BETWEEN_RAISE_STEPS.load(Ordering::SeqCst),
        "hook resets itself"
    );

    let old_status: String = sqlx::query_scalar("SELECT status FROM alerts WHERE id = $1")
        .bind(old_id)
        .fetch_one(&mut *conn)
        .await
        .unwrap();
    assert_eq!(old_status, "resolved");
    let live: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM alerts WHERE dedup_key = $1 AND status <> 'resolved'",
    )
    .bind(key)
    .fetch_one(&mut *conn)
    .await
    .unwrap();
    assert_eq!(live, 1);

    let again = raise(
        &mut conn,
        &new_alert(key, AlertKind::MonitorBreach, Severity::Warning),
    )
    .await
    .unwrap();
    assert_eq!(again, RaiseOutcome::Repeated(new_id));
    drop(conn);
    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn concurrent_raise_creates_one_alert() {
    let server = TestServer::start().await;
    let channel = seed_channel(&server, "concurrent", true).await;
    seed_route(&server, channel, None, "info").await;
    let key = "concurrent:one";

    let mut a = sqlx::PgConnection::connect(&server.db_url).await.unwrap();
    let mut b = sqlx::PgConnection::connect(&server.db_url).await.unwrap();
    let alert = new_alert(key, AlertKind::SpendSpike, Severity::Warning);
    let (ra, rb) = tokio::join!(raise(&mut a, &alert), raise(&mut b, &alert));
    let outcomes = [ra.unwrap(), rb.unwrap()];

    let opened = outcomes
        .iter()
        .filter(|o| matches!(o, RaiseOutcome::Opened(_)))
        .count();
    let repeated = outcomes
        .iter()
        .filter(|o| matches!(o, RaiseOutcome::Repeated(_)))
        .count();
    assert_eq!((opened, repeated), (1, 1), "outcomes: {outcomes:?}");

    let rows = alert_by_key(&server, key).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["occurrences"], 2);
    let ob = outbox(&server).await;
    assert_eq!(ob.len(), 1);
    assert_eq!(ob[0]["event"], "opened");
    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn raise_waits_for_inflight_resolve() {
    let server = TestServer::start().await;
    let key = "inflight:one";
    let mut seed = sqlx::PgConnection::connect(&server.db_url).await.unwrap();
    let RaiseOutcome::Opened(old_id) = raise(
        &mut seed,
        &new_alert(key, AlertKind::BudgetHard, Severity::Critical),
    )
    .await
    .unwrap() else {
        panic!("expected Opened");
    };

    // A: uncommitted resolve of the open row.
    let mut a = sqlx::PgConnection::connect(&server.db_url).await.unwrap();
    sqlx::query("BEGIN").execute(&mut a).await.unwrap();
    sqlx::query(
        "UPDATE alerts SET status = 'resolved', resolved_at = now() \
         WHERE dedup_key = $1 AND status <> 'resolved'",
    )
    .bind(key)
    .execute(&mut a)
    .await
    .unwrap();

    // B: raise must wait on A, then open a fresh row.
    let url = server.db_url.clone();
    let b = tokio::spawn(async move {
        let mut conn = sqlx::PgConnection::connect(&url).await.unwrap();
        raise(
            &mut conn,
            &new_alert(key, AlertKind::BudgetHard, Severity::Critical),
        )
        .await
    });
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        !b.is_finished(),
        "raise must wait for the in-flight resolve"
    );
    sqlx::query("COMMIT").execute(&mut a).await.unwrap();

    let outcome = b.await.unwrap().expect("no unique violation");
    let RaiseOutcome::Opened(new_id) = outcome else {
        panic!("expected Opened, got {outcome:?}");
    };
    assert_ne!(new_id, old_id);

    let rows = alert_by_key(&server, key).await;
    assert_eq!(rows.len(), 2);
    let open = rows.iter().filter(|r| r["status"] != "resolved").count();
    assert_eq!(open, 1);
    let old = rows.iter().find(|r| r["id"] == old_id.to_string()).unwrap();
    assert_eq!(old["status"], "resolved");
    server.cleanup().await;
}

// ─── engine: outbox routing ──────────────────────────────────────────────────

#[tokio::test]
#[serial]
async fn outbox_rows_follow_routes() {
    let server = TestServer::start().await;
    let chan_a = seed_channel(&server, "chan-a", true).await;
    let chan_b = seed_channel(&server, "chan-b", true).await;
    let chan_off = seed_channel(&server, "chan-off", false).await;
    seed_route(&server, chan_a, None, "warning").await;
    seed_route(&server, chan_a, Some("budget_hard"), "critical").await;
    seed_route(&server, chan_b, Some("budget_soft"), "critical").await;
    seed_route(&server, chan_off, None, "info").await;

    let mut conn = server.db.acquire().await.unwrap();
    let k1 = "route:k1";
    let RaiseOutcome::Opened(id1) = raise(
        &mut conn,
        &new_alert(k1, AlertKind::BudgetHard, Severity::Critical),
    )
    .await
    .unwrap() else {
        panic!("expected Opened");
    };
    let rows: Vec<Value> = sqlx::query_scalar("SELECT to_jsonb(o) FROM notification_outbox o")
        .fetch_all(&mut *conn)
        .await
        .unwrap();
    assert_eq!(
        rows.len(),
        1,
        "one row for channel A despite two matching routes"
    );
    assert_eq!(rows[0]["channel_id"], chan_a.to_string());
    assert_eq!(rows[0]["event"], "opened");
    assert_eq!(rows[0]["status"], "pending");
    assert_eq!(rows[0]["payload"]["event"], "opened");
    assert_eq!(rows[0]["payload"]["alert"]["kind"], "budget_hard");
    assert_eq!(rows[0]["payload"]["alert"]["id"], id1.to_string());

    // Repeated writes nothing.
    let repeat = raise(
        &mut conn,
        &new_alert(k1, AlertKind::BudgetHard, Severity::Critical),
    )
    .await
    .unwrap();
    assert_eq!(repeat, RaiseOutcome::Repeated(id1));
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM notification_outbox")
        .fetch_one(&mut *conn)
        .await
        .unwrap();
    assert_eq!(n, 1);

    // Escalation writes an `escalated` row.
    let k2 = "route:k2";
    let RaiseOutcome::Opened(id2) = raise(
        &mut conn,
        &new_alert(k2, AlertKind::BudgetHard, Severity::Warning),
    )
    .await
    .unwrap() else {
        panic!("expected Opened");
    };
    let up = raise(
        &mut conn,
        &new_alert(k2, AlertKind::BudgetHard, Severity::Critical),
    )
    .await
    .unwrap();
    assert_eq!(up, RaiseOutcome::Escalated(id2));
    let events: Vec<String> = sqlx::query_scalar(
        "SELECT event FROM notification_outbox WHERE alert_id = $1 ORDER BY created_at, event",
    )
    .bind(id2)
    .fetch_all(&mut *conn)
    .await
    .unwrap();
    assert_eq!(events.len(), 2);
    assert!(events.contains(&"opened".to_owned()) && events.contains(&"escalated".to_owned()));

    // Resolve writes a `resolved` row for A only.
    assert_eq!(resolve(&mut conn, k1).await.unwrap(), Some(id1));
    let resolved: Vec<Value> = sqlx::query_scalar(
        "SELECT to_jsonb(o) FROM notification_outbox o WHERE alert_id = $1 AND event = 'resolved'",
    )
    .bind(id1)
    .fetch_all(&mut *conn)
    .await
    .unwrap();
    assert_eq!(resolved.len(), 1);
    assert_eq!(resolved[0]["channel_id"], chan_a.to_string());
    assert_eq!(resolved[0]["payload"]["alert"]["status"], "resolved");
    drop(conn);
    server.cleanup().await;
}

// ─── resolver sweep ──────────────────────────────────────────────────────────

#[tokio::test]
#[serial]
async fn sweep_resolves_budget_alerts() {
    let server = TestServer::start().await;
    let root = seed_root(&server).await;
    let now = Utc::now();
    let start = month_start(now);

    let mut ids = Vec::new();
    for i in 0..3 {
        let user = seed_user(&server, &format!("sweep-user-{i}"), "member").await;
        let b = create_budget(
            &server,
            root,
            user_budget(&format!("sweep budget {i}"), user, 10.0),
        )
        .await;
        let id = b["id"].as_str().unwrap().to_owned();
        insert_event(&server, &id, start, "soft_threshold", 8.5, 10.0).await;
        ids.push(id);
    }
    assert_eq!(tick_budget_events(&server.db, now).await.unwrap(), 3);

    // Unrelated open alert must survive the sweep.
    {
        let mut conn = server.db.acquire().await.unwrap();
        raise(
            &mut conn,
            &new_alert("unrelated:one", AlertKind::SpendSpike, Severity::Warning),
        )
        .await
        .unwrap();
    }

    // (a) period already over: rewrite stored period_end into the past.
    sqlx::query(
        "UPDATE alerts SET details = jsonb_set(details, '{period_end}', \
         to_jsonb('2020-01-01T00:00:00+00:00'::text)) WHERE details->>'budget_id' = $1",
    )
    .bind(&ids[0])
    .execute(&server.db)
    .await
    .unwrap();
    // (b) budget disabled.
    let resp = admin(
        root,
        server
            .client
            .put(server.url(&format!("/api/budgets/{}", ids[1]))),
    )
    .json(&json!({"enabled": false}))
    .send()
    .await
    .unwrap();
    assert_eq!(resp.status(), 200);
    // (c) budget deleted.
    let resp = admin(
        root,
        server
            .client
            .delete(server.url(&format!("/api/budgets/{}", ids[2]))),
    )
    .send()
    .await
    .unwrap();
    assert!(resp.status().is_success());

    assert_eq!(tick_resolve_sweep(&server.db, now).await.unwrap(), 3);

    let rows = alert_rows(&server).await;
    for r in rows.iter().filter(|r| r["kind"] == "budget_soft") {
        assert_eq!(r["status"], "resolved");
        assert!(r["resolved_at"].is_string());
    }
    assert_eq!(
        rows.iter().filter(|r| r["kind"] == "budget_soft").count(),
        3
    );
    let unrelated = rows
        .iter()
        .find(|r| r["dedup_key"] == "unrelated:one")
        .unwrap();
    assert_eq!(unrelated["status"], "open");

    assert_eq!(tick_resolve_sweep(&server.db, now).await.unwrap(), 0);
    server.cleanup().await;
}

// ─── alerts API ──────────────────────────────────────────────────────────────

fn ts(t: DateTime<Utc>) -> String {
    t.to_rfc3339_opts(SecondsFormat::Secs, true)
}

async fn list(server: &TestServer, root: Uuid, query: &[(&str, String)]) -> reqwest::Response {
    admin(root, server.client.get(server.url("/api/alerts")))
        .query(query)
        .send()
        .await
        .expect("list alerts")
}

async fn list_ids(server: &TestServer, root: Uuid, query: &[(&str, String)]) -> Vec<String> {
    let resp = list(server, root, query).await;
    assert_eq!(resp.status(), 200);
    resp.json::<Value>().await.unwrap()["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["id"].as_str().unwrap().to_owned())
        .collect()
}

#[tokio::test]
#[serial]
async fn alerts_api_lists_filters_and_acknowledges() {
    let server = TestServer::start().await;
    let root = seed_root(&server).await;
    let base = Utc::now();

    // (key, kind, severity, scope, minutes ago)
    let specs = [
        (
            "api:1",
            AlertKind::BudgetSoft,
            Severity::Warning,
            AlertScope::User,
            40,
        ),
        (
            "api:2",
            AlertKind::BudgetHard,
            Severity::Critical,
            AlertScope::User,
            30,
        ),
        (
            "api:3",
            AlertKind::SpendSpike,
            Severity::Warning,
            AlertScope::Platform,
            20,
        ),
        (
            "api:4",
            AlertKind::MonitorBreach,
            Severity::Critical,
            AlertScope::Agent,
            10,
        ),
    ];
    let mut ids = Vec::new();
    {
        let mut conn = server.db.acquire().await.unwrap();
        for (key, kind, sev, scope, mins) in specs {
            let mut a = new_alert(key, kind, sev);
            a.scope = scope;
            let RaiseOutcome::Opened(id) = raise(&mut conn, &a).await.unwrap() else {
                panic!("expected Opened");
            };
            sqlx::query("UPDATE alerts SET first_seen_at = $2 WHERE id = $1")
                .bind(id)
                .bind(base - chrono::Duration::minutes(mins))
                .execute(&mut *conn)
                .await
                .unwrap();
            ids.push(id.to_string());
        }
    }

    let resp = list(&server, root, &[]).await;
    assert_eq!(resp.status(), 200);
    let body = resp.json::<Value>().await.unwrap();
    assert_eq!(body["has_more"], false);
    assert!(body["prev_cursor"].is_null());
    let got: Vec<String> = body["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["id"].as_str().unwrap().to_owned())
        .collect();
    let newest_first: Vec<String> = ids.iter().rev().cloned().collect();
    assert_eq!(got, newest_first);

    assert_eq!(
        list_ids(&server, root, &[("kind", "budget_hard".into())]).await,
        vec![ids[1].clone()]
    );
    assert_eq!(
        list_ids(&server, root, &[("severity", "critical".into())])
            .await
            .len(),
        2
    );
    assert_eq!(
        list_ids(&server, root, &[("scope", "platform".into())]).await,
        vec![ids[2].clone()]
    );
    assert_eq!(
        list_ids(&server, root, &[("status", "open".into())])
            .await
            .len(),
        4
    );
    assert!(
        list_ids(&server, root, &[("status", "resolved".into())])
            .await
            .is_empty()
    );
    let cut = base - chrono::Duration::minutes(25);
    assert_eq!(
        list_ids(&server, root, &[("since", ts(cut))]).await,
        vec![ids[3].clone(), ids[2].clone()]
    );
    assert_eq!(
        list_ids(&server, root, &[("until", ts(cut))]).await,
        vec![ids[1].clone(), ids[0].clone()]
    );

    // Paging with limit=1 walks every row exactly once.
    let mut walked = Vec::new();
    let mut cursor: Option<String> = None;
    for _ in 0..10 {
        let mut q = vec![("limit", "1".to_owned())];
        if let Some(c) = &cursor {
            q.push(("cursor", c.clone()));
        }
        let body = list(&server, root, &q).await.json::<Value>().await.unwrap();
        for a in body["data"].as_array().unwrap() {
            walked.push(a["id"].as_str().unwrap().to_owned());
        }
        if body["has_more"] == true {
            cursor = Some(body["next_cursor"].as_str().unwrap().to_owned());
        } else {
            break;
        }
    }
    assert_eq!(walked, newest_first);

    let resp = list(&server, root, &[("kind", "nope".into())]).await;
    assert_eq!(resp.status(), 400);
    assert_eq!(
        resp.json::<Value>().await.unwrap()["code"],
        "invalid_filter"
    );

    // Acknowledge.
    let ack = |id: String| {
        admin(
            root,
            server
                .client
                .post(server.url(&format!("/api/alerts/{id}/acknowledge"))),
        )
        .send()
    };
    let resp = ack(ids[0].clone()).await.unwrap();
    assert_eq!(resp.status(), 200);
    let data = resp.json::<Value>().await.unwrap()["data"].clone();
    assert_eq!(data["status"], "acknowledged");
    assert_eq!(data["acknowledged_by"], root.to_string());
    assert!(data["acknowledged_at"].is_string());

    let resp = ack(ids[0].clone()).await.unwrap();
    assert_eq!(resp.status(), 200);
    let again = resp.json::<Value>().await.unwrap()["data"].clone();
    assert_eq!(again["status"], "acknowledged");
    assert_eq!(again["acknowledged_at"], data["acknowledged_at"]);

    let resp = ack(Uuid::new_v4().to_string()).await.unwrap();
    assert_eq!(resp.status(), 404);
    assert_eq!(resp.json::<Value>().await.unwrap()["code"], "not_found");

    // An acknowledged alert still dedups; a resolved one cannot be acknowledged.
    {
        let mut conn = server.db.acquire().await.unwrap();
        let outcome = raise(
            &mut conn,
            &new_alert("api:1", AlertKind::BudgetSoft, Severity::Warning),
        )
        .await
        .unwrap();
        assert!(matches!(outcome, RaiseOutcome::Repeated(_)), "{outcome:?}");
        resolve(&mut conn, "api:2").await.unwrap();
    }
    let resp = ack(ids[1].clone()).await.unwrap();
    assert_eq!(resp.status(), 409);
    assert_eq!(
        resp.json::<Value>().await.unwrap()["code"],
        "alert_resolved"
    );
    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn member_gets_403_on_alert_routes() {
    let server = TestServer::start().await;
    let _root = seed_root(&server).await;
    let user = seed_user(&server, "plain-member", "member").await;

    let resp = member(
        server.client.get(server.url("/api/alerts")),
        user,
        "plain-member",
    )
    .send()
    .await
    .unwrap();
    assert_eq!(resp.status(), 403);
    assert_eq!(
        resp.json::<Value>().await.unwrap()["code"],
        "admin_required"
    );

    let resp = member(
        server
            .client
            .post(server.url(&format!("/api/alerts/{}/acknowledge", Uuid::new_v4()))),
        user,
        "plain-member",
    )
    .send()
    .await
    .unwrap();
    assert_eq!(resp.status(), 403);
    assert_eq!(
        resp.json::<Value>().await.unwrap()["code"],
        "admin_required"
    );
    server.cleanup().await;
}
