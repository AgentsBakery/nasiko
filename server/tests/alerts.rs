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

use chrono::{DateTime, Datelike, SecondsFormat, TimeZone, Timelike, Utc};
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

// ─── notifications ───────────────────────────────────────────────────────────

use hmac::{Hmac, Mac};
use nasiko_config::AlertsConfig;
use nasiko_server::notifications::dispatch::{
    DispatchDeps, SendResult, finish_row, tick_outbox_dispatch,
};
use sha2::Sha256;

fn dispatch_deps(allow_private: bool) -> DispatchDeps {
    let mut cfg = AlertsConfig::disabled();
    cfg.allow_private_urls = allow_private;
    DispatchDeps::from_config(&cfg)
}

async fn private_server() -> TestServer {
    TestServer::start_with(|c| c.alerts.allow_private_urls = true).await
}

/// Headers and body of the most recent request a mock matched.
#[derive(Clone, Default)]
struct Captured(std::sync::Arc<std::sync::Mutex<Option<CapturedRequest>>>);

type CapturedRequest = (Vec<(String, String)>, Vec<u8>);

impl Captured {
    fn matcher(&self) -> impl Fn(&mockito::Request) -> bool + Send + Sync + 'static {
        let slot = self.0.clone();
        move |req| {
            let headers = req
                .headers()
                .iter()
                .map(|(k, v)| (k.as_str().to_owned(), v.to_str().unwrap_or("").to_owned()))
                .collect();
            let body = req.body().cloned().unwrap_or_default();
            *slot.lock().unwrap() = Some((headers, body));
            true
        }
    }

    fn header(&self, name: &str) -> Option<String> {
        let guard = self.0.lock().unwrap();
        let (headers, _) = guard.as_ref()?;
        headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.clone())
    }

    fn body(&self) -> Vec<u8> {
        self.0
            .lock()
            .unwrap()
            .as_ref()
            .map(|(_, b)| b.clone())
            .unwrap_or_default()
    }
}

async fn post_channel(server: &TestServer, root: Uuid, body: Value) -> reqwest::Response {
    admin(
        root,
        server.client.post(server.url("/api/notification-channels")),
    )
    .json(&body)
    .send()
    .await
    .expect("post channel")
}

async fn make_channel(server: &TestServer, root: Uuid, body: Value) -> Uuid {
    let resp = post_channel(server, root, body).await;
    assert_eq!(resp.status(), 201, "create channel");
    let v = resp.json::<Value>().await.unwrap();
    v["data"]["id"].as_str().unwrap().parse().unwrap()
}

fn webhook_body(name: &str, url: &str, hmac_secret: Option<&str>) -> Value {
    let mut b = json!({"name": name, "kind": "webhook", "url": url});
    if let Some(s) = hmac_secret {
        b["hmac_secret"] = json!(s);
    }
    b
}

async fn put_routes(server: &TestServer, root: Uuid, channel: Uuid, routes: Value) -> Value {
    let resp = admin(
        root,
        server
            .client
            .put(server.url(&format!("/api/notification-channels/{channel}/routes"))),
    )
    .json(&json!({ "routes": routes }))
    .send()
    .await
    .expect("put routes");
    assert_eq!(resp.status(), 200, "put routes");
    resp.json::<Value>().await.unwrap()
}

async fn insert_outbox(server: &TestServer, channel: Uuid, title: &str) -> Uuid {
    let payload = json!({
        "event": "opened",
        "alert": {
            "id": Uuid::new_v4(), "kind": "budget_soft", "severity": "warning",
            "scope": "platform", "scope_ref": null, "title": title, "message": "m",
            "link": "/budgets", "first_seen_at": "2026-01-01T00:00:00.000000Z",
            "last_seen_at": "2026-01-01T00:00:00.000000Z", "occurrences": 1, "status": "open",
        },
    });
    sqlx::query_scalar(
        "INSERT INTO notification_outbox (channel_id, event, payload) \
         VALUES ($1, 'opened', $2) RETURNING id",
    )
    .bind(channel)
    .bind(payload)
    .fetch_one(&server.db)
    .await
    .expect("insert outbox")
}

async fn outbox_row(server: &TestServer, id: Uuid) -> Value {
    sqlx::query_scalar::<_, Value>("SELECT to_jsonb(o) FROM notification_outbox o WHERE id = $1")
        .bind(id)
        .fetch_one(&server.db)
        .await
        .expect("outbox row")
}

async fn raise_budget_event(server: &TestServer, root: Uuid, name: &str, kind: &str) {
    let user = seed_user(server, &format!("u-{}", Uuid::new_v4().simple()), "member").await;
    let budget = create_budget(server, root, user_budget(name, user, 10.0)).await;
    let now = Utc::now();
    insert_event(
        server,
        budget["id"].as_str().unwrap(),
        month_start(now),
        kind,
        10.0,
        10.0,
    )
    .await;
    assert_eq!(tick_budget_events(&server.db, now).await.unwrap(), 1);
}

#[tokio::test]
#[serial]
async fn channel_crud_never_returns_secrets() {
    let server = private_server().await;
    let root = seed_root(&server).await;
    let mut receiver = mockito::Server::new_async().await;
    let hook = receiver
        .mock("POST", "/hook/SECRETPATH1234")
        .with_status(200)
        .expect(1)
        .create_async()
        .await;
    let url = format!("{}/hook/SECRETPATH1234", receiver.url());

    let resp = post_channel(&server, root, webhook_body("crud", &url, Some("s3cr3t"))).await;
    assert_eq!(resp.status(), 201);
    let text = resp.text().await.unwrap();
    let v: Value = serde_json::from_str(&text).unwrap();
    let id = v["data"]["id"].as_str().unwrap().to_owned();
    assert_eq!(v["data"]["has_hmac_secret"], true);
    assert!(v["data"]["url_hint"].is_string());
    for forbidden in ["url", "hmac_secret", "config_encrypted"] {
        assert!(v["data"].get(forbidden).is_none(), "{forbidden} leaked");
    }
    assert!(!text.contains("SECRETPATH") && !text.contains("s3cr3t"));

    let stored: String = sqlx::query_scalar(
        "SELECT config_encrypted FROM notification_channels WHERE id = $1::uuid",
    )
    .bind(&id)
    .fetch_one(&server.db)
    .await
    .unwrap();
    assert!(!stored.contains("SECRETPATH") && !stored.contains("s3cr3t"));

    let list = admin(
        root,
        server.client.get(server.url("/api/notification-channels")),
    )
    .send()
    .await
    .unwrap()
    .text()
    .await
    .unwrap();
    assert!(!list.contains("SECRETPATH") && !list.contains("s3cr3t"));
    assert!(list.contains(&id));

    let one_url = server.url(&format!("/api/notification-channels/{id}"));
    let one = admin(root, server.client.get(&one_url))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(!one.contains("SECRETPATH") && !one.contains("s3cr3t"));

    let put = admin(root, server.client.put(&one_url))
        .json(&json!({"name": "renamed"}))
        .send()
        .await
        .unwrap();
    assert_eq!(put.status(), 200);
    let put_text = put.text().await.unwrap();
    assert!(!put_text.contains("SECRETPATH") && !put_text.contains("s3cr3t"));
    let pv: Value = serde_json::from_str(&put_text).unwrap();
    assert_eq!(pv["data"]["name"], "renamed");
    assert_eq!(pv["data"]["has_hmac_secret"], true);

    // The URL survived the unrelated update: a test send still reaches the path.
    let test = admin(root, server.client.post(format!("{one_url}/test")))
        .send()
        .await
        .unwrap();
    assert_eq!(test.status(), 200);
    hook.assert_async().await;

    let cleared = admin(root, server.client.put(&one_url))
        .json(&json!({"hmac_secret": ""}))
        .send()
        .await
        .unwrap();
    assert_eq!(cleared.status(), 200);
    let cv = cleared.json::<Value>().await.unwrap();
    assert_eq!(cv["data"]["has_hmac_secret"], false);

    let del = admin(root, server.client.delete(&one_url))
        .send()
        .await
        .unwrap();
    assert!(del.status().is_success(), "delete: {}", del.status());
    let gone = admin(root, server.client.get(&one_url))
        .send()
        .await
        .unwrap();
    assert_eq!(gone.status(), 404);
    assert_eq!(gone.json::<Value>().await.unwrap()["code"], "not_found");

    let slack = post_channel(
        &server,
        root,
        json!({"name": "s", "kind": "slack", "url": format!("{}/services/T/B/X", receiver.url()),
               "hmac_secret": "nope"}),
    )
    .await;
    assert_eq!(slack.status(), 400);
    assert_eq!(
        slack.json::<Value>().await.unwrap()["code"],
        "hmac_not_supported"
    );
    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn channel_url_policy_default_server() {
    let server = TestServer::start().await;
    let root = seed_root(&server).await;
    let cases = [
        ("webhook", "http://example.com/x"),
        ("webhook", "https://127.0.0.1/x"),
        ("webhook", "https://[::1]/x"),
        ("webhook", "https://169.254.169.254/latest"),
        ("webhook", "https://[::ffff:127.0.0.1]/x"),
        ("webhook", "https://localhost/x"),
        ("webhook", "https://hooks.slack.com@127.0.0.1/x"),
        ("slack", "https://example.com/services/x"),
    ];
    for (kind, url) in cases {
        let resp = post_channel(
            &server,
            root,
            json!({"name": "bad", "kind": kind, "url": url}),
        )
        .await;
        assert_eq!(resp.status(), 400, "{url}");
        let text = resp.text().await.unwrap();
        let v: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["code"], "invalid_channel_url", "{url}");
        assert!(
            !text.contains("example.com") && !text.contains("127.0.0.1"),
            "{text}"
        );
    }
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM notification_channels")
        .fetch_one(&server.db)
        .await
        .unwrap();
    assert_eq!(count, 0);
    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn routes_replace_and_validate() {
    let server = private_server().await;
    let root = seed_root(&server).await;
    let channel = make_channel(
        &server,
        root,
        webhook_body("r", "http://127.0.0.1:9/x", None),
    )
    .await;
    let routes_url = server.url(&format!("/api/notification-channels/{channel}/routes"));

    put_routes(
        &server,
        root,
        channel,
        json!([
            {"alert_kind": null, "min_severity": "warning"},
            {"alert_kind": "budget_hard", "min_severity": "critical"},
        ]),
    )
    .await;
    let got = admin(root, server.client.get(&routes_url))
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    assert_eq!(got["data"].as_array().unwrap().len(), 2);

    put_routes(
        &server,
        root,
        channel,
        json!([{"alert_kind": "spend_spike", "min_severity": "info"}]),
    )
    .await;
    let got = admin(root, server.client.get(&routes_url))
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    let rows = got["data"].as_array().unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["alert_kind"], "spend_spike");
    assert_eq!(rows[0]["min_severity"], "info");

    for bad in [
        json!([{"alert_kind": "nope", "min_severity": "info"}]),
        json!([{"alert_kind": null, "min_severity": "loud"}]),
    ] {
        let resp = admin(root, server.client.put(&routes_url))
            .json(&json!({ "routes": bad }))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 400);
        assert_eq!(resp.json::<Value>().await.unwrap()["code"], "invalid_route");
    }
    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn budget_alert_delivered_with_hmac() {
    let server = private_server().await;
    let root = seed_root(&server).await;
    let captured = Captured::default();
    let mut receiver = mockito::Server::new_async().await;
    let hook = receiver
        .mock("POST", "/hook")
        .match_request(captured.matcher())
        .with_status(200)
        .expect(1)
        .create_async()
        .await;
    let channel = make_channel(
        &server,
        root,
        webhook_body(
            "hmac",
            &format!("{}/hook", receiver.url()),
            Some("topsecret"),
        ),
    )
    .await;
    put_routes(
        &server,
        root,
        channel,
        json!([{"alert_kind": null, "min_severity": "info"}]),
    )
    .await;

    raise_budget_event(&server, root, "hmac budget", "soft_threshold").await;
    let stats = tick_outbox_dispatch(&server.db, &dispatch_deps(true))
        .await
        .unwrap();
    assert_eq!((stats.claimed, stats.delivered), (1, 1));
    hook.assert_async().await;

    assert_eq!(
        captured.header("content-type").as_deref(),
        Some("application/json")
    );
    assert_eq!(captured.header("x-nasiko-event").as_deref(), Some("opened"));
    let row = outbox(&server).await.remove(0);
    assert_eq!(
        captured.header("x-nasiko-delivery").as_deref(),
        row["id"].as_str()
    );
    let ts: i64 = captured
        .header("x-nasiko-timestamp")
        .unwrap()
        .parse()
        .unwrap();
    assert!((Utc::now().timestamp() - ts).abs() < 60);
    let body = captured.body();
    let mut mac = Hmac::<Sha256>::new_from_slice(b"topsecret").unwrap();
    mac.update(format!("{ts}.").as_bytes());
    mac.update(&body);
    let expected = format!("sha256={}", hex::encode(mac.finalize().into_bytes()));
    assert_eq!(captured.header("x-nasiko-signature"), Some(expected));

    let v: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["event"], "opened");
    assert_eq!(v["alert"]["kind"], "budget_soft");
    assert_eq!(v["alert"]["link"], "/budgets");
    assert!(v["sent_at"].is_string());

    assert_eq!(row["status"], "delivered");
    assert!(row["delivered_at"].is_string());
    assert_eq!(row["attempts"], 1);
    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn slack_delivery_shape() {
    let server = private_server().await;
    let root = seed_root(&server).await;
    let captured = Captured::default();
    let mut receiver = mockito::Server::new_async().await;
    let hook = receiver
        .mock("POST", "/services/T/B/X")
        .match_request(captured.matcher())
        .with_status(200)
        .expect(1)
        .create_async()
        .await;
    let channel = make_channel(
        &server,
        root,
        json!({"name": "slack", "kind": "slack", "url": format!("{}/services/T/B/X", receiver.url())}),
    )
    .await;
    put_routes(
        &server,
        root,
        channel,
        json!([{"alert_kind": null, "min_severity": "info"}]),
    )
    .await;

    raise_budget_event(&server, root, "a<b>&c", "hard_limit").await;
    tick_outbox_dispatch(&server.db, &dispatch_deps(true))
        .await
        .unwrap();
    hook.assert_async().await;

    let v: Value = serde_json::from_slice(&captured.body()).unwrap();
    let text = v["text"].as_str().expect("text");
    assert!(v["blocks"].is_array());
    assert!(text.contains("a&lt;b&gt;&amp;c"), "{text}");
    assert!(!text.contains("a<b>"), "{text}");
    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn failing_receiver_backs_off_then_fails() {
    let server = private_server().await;
    let root = seed_root(&server).await;
    let mut receiver = mockito::Server::new_async().await;
    let hook = receiver
        .mock("POST", "/fail")
        .with_status(500)
        .expect(6)
        .create_async()
        .await;
    let channel = make_channel(
        &server,
        root,
        webhook_body("fail", &format!("{}/fail", receiver.url()), None),
    )
    .await;
    let id = insert_outbox(&server, channel, "t").await;
    let deps = dispatch_deps(true);

    tick_outbox_dispatch(&server.db, &deps).await.unwrap();
    let row = outbox_row(&server, id).await;
    assert_eq!(row["status"], "pending");
    assert_eq!(row["attempts"], 1);
    assert_eq!(row["last_error"], "http_5xx");
    let next: DateTime<Utc> = row["next_attempt_at"].as_str().unwrap().parse().unwrap();
    let delta = (next - Utc::now()).num_seconds();
    assert!((25..=35).contains(&delta), "first backoff was {delta}s");

    for _ in 0..5 {
        sqlx::query("UPDATE notification_outbox SET next_attempt_at = now() WHERE id = $1")
            .bind(id)
            .execute(&server.db)
            .await
            .unwrap();
        tick_outbox_dispatch(&server.db, &deps).await.unwrap();
    }
    let row = outbox_row(&server, id).await;
    assert_eq!(row["status"], "failed");
    assert_eq!(row["attempts"], 6);
    let err = row["last_error"].as_str().unwrap();
    assert!(
        !err.contains("http://") && !err.contains("127.0.0.1"),
        "{err}"
    );

    tick_outbox_dispatch(&server.db, &deps).await.unwrap();
    hook.assert_async().await;
    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn concurrent_dispatchers_send_each_row_once() {
    let server = private_server().await;
    let root = seed_root(&server).await;
    let mut receiver = mockito::Server::new_async().await;
    let hook = receiver
        .mock("POST", "/once")
        .with_status(200)
        .expect(10)
        .create_async()
        .await;
    let channel = make_channel(
        &server,
        root,
        webhook_body("once", &format!("{}/once", receiver.url()), None),
    )
    .await;
    let mut ids = Vec::new();
    for i in 0..10 {
        ids.push(insert_outbox(&server, channel, &format!("t{i}")).await);
    }

    let pool_a = sqlx::postgres::PgPoolOptions::new()
        .max_connections(3)
        .connect(&server.db_url)
        .await
        .unwrap();
    let pool_b = sqlx::postgres::PgPoolOptions::new()
        .max_connections(3)
        .connect(&server.db_url)
        .await
        .unwrap();
    let deps = dispatch_deps(true);
    let (a, b) = tokio::join!(
        tick_outbox_dispatch(&pool_a, &deps),
        tick_outbox_dispatch(&pool_b, &deps)
    );
    let (a, b) = (a.unwrap(), b.unwrap());
    let mut claimed = a.claimed + b.claimed;
    for _ in 0..3 {
        let left: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM notification_outbox WHERE status <> 'delivered'",
        )
        .fetch_one(&server.db)
        .await
        .unwrap();
        if left == 0 {
            break;
        }
        claimed += tick_outbox_dispatch(&pool_a, &deps).await.unwrap().claimed;
    }
    assert_eq!(claimed, 10);
    hook.assert_async().await;
    for id in ids {
        let row = outbox_row(&server, id).await;
        assert_eq!(row["status"], "delivered");
        assert_eq!(row["attempts"], 1);
    }
    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn stale_reclaim_does_not_double_finish() {
    let server = private_server().await;
    let root = seed_root(&server).await;
    let channel = make_channel(
        &server,
        root,
        webhook_body("stale", "http://127.0.0.1:9/x", None),
    )
    .await;
    let id = insert_outbox(&server, channel, "t").await;

    let t1: DateTime<Utc> = sqlx::query_scalar(
        "UPDATE notification_outbox SET status = 'sending', attempts = 1, \
         claimed_at = now() - interval '5 minutes' WHERE id = $1 RETURNING claimed_at",
    )
    .bind(id)
    .fetch_one(&server.db)
    .await
    .unwrap();
    sqlx::query(
        "UPDATE notification_outbox SET claimed_at = now(), attempts = attempts + 1 WHERE id = $1",
    )
    .bind(id)
    .execute(&server.db)
    .await
    .unwrap();

    let result = SendResult {
        delivered: true,
        status_code: Some(200),
        error: None,
    };
    let applied = finish_row(&server.db, id, t1, &result, 1).await.unwrap();
    assert!(!applied, "stale worker must not apply its result");
    let row = outbox_row(&server, id).await;
    assert_eq!(row["status"], "sending");
    assert_eq!(row["attempts"], 2);
    assert!(row["delivered_at"].is_null());
    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn stale_sending_row_is_reclaimed() {
    let server = private_server().await;
    let root = seed_root(&server).await;
    let mut receiver = mockito::Server::new_async().await;
    let hook = receiver
        .mock("POST", "/stale")
        .with_status(200)
        .expect(1)
        .create_async()
        .await;
    let channel = make_channel(
        &server,
        root,
        webhook_body("stale", &format!("{}/stale", receiver.url()), None),
    )
    .await;
    let stale = insert_outbox(&server, channel, "stale").await;
    let fresh = insert_outbox(&server, channel, "fresh").await;
    for (id, age) in [(stale, "3 minutes"), (fresh, "30 seconds")] {
        sqlx::query(&format!(
            "UPDATE notification_outbox SET status = 'sending', attempts = 1, \
             claimed_at = now() - interval '{age}' WHERE id = $1"
        ))
        .bind(id)
        .execute(&server.db)
        .await
        .unwrap();
    }
    tick_outbox_dispatch(&server.db, &dispatch_deps(true))
        .await
        .unwrap();
    hook.assert_async().await;
    assert_eq!(outbox_row(&server, stale).await["status"], "delivered");
    assert_eq!(outbox_row(&server, stale).await["attempts"], 2);
    assert_eq!(outbox_row(&server, fresh).await["status"], "sending");
    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn disabled_channel_marks_failed() {
    let server = private_server().await;
    let root = seed_root(&server).await;
    let mut receiver = mockito::Server::new_async().await;
    let hook = receiver
        .mock("POST", "/off")
        .with_status(200)
        .expect(0)
        .create_async()
        .await;
    let channel = make_channel(
        &server,
        root,
        webhook_body("off", &format!("{}/off", receiver.url()), None),
    )
    .await;
    let id = insert_outbox(&server, channel, "t").await;
    let resp = admin(
        root,
        server
            .client
            .put(server.url(&format!("/api/notification-channels/{channel}"))),
    )
    .json(&json!({"enabled": false}))
    .send()
    .await
    .unwrap();
    assert_eq!(resp.status(), 200);

    tick_outbox_dispatch(&server.db, &dispatch_deps(true))
        .await
        .unwrap();
    let row = outbox_row(&server, id).await;
    assert_eq!(row["status"], "failed");
    assert_eq!(row["last_error"], "channel_disabled");
    hook.assert_async().await;
    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn send_time_ssrf_recheck() {
    let server = TestServer::start().await;
    let root = seed_root(&server).await;
    let _ = root;
    let crypto = nasiko_secrets::SecretsCrypto::try_for_system().expect("test master key");
    let channel: Uuid = sqlx::query_scalar(
        "INSERT INTO notification_channels (name, kind, config_encrypted, url_hint) \
         VALUES ('ssrf', 'webhook', $1, 'h') RETURNING id",
    )
    .bind(crypto.encrypt(r#"{"url":"http://127.0.0.1:9/x"}"#))
    .fetch_one(&server.db)
    .await
    .unwrap();
    seed_route(&server, channel, None, "info").await;
    let id = insert_outbox(&server, channel, "t").await;

    tick_outbox_dispatch(&server.db, &dispatch_deps(false))
        .await
        .unwrap();
    let row = outbox_row(&server, id).await;
    let err = row["last_error"].as_str().unwrap();
    assert!(
        err == "blocked_address" || err == "invalid_url" || err == "scheme_not_allowed",
        "unexpected error {err}"
    );
    assert_ne!(row["status"], "delivered");
    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn test_endpoint_reports_result() {
    let server = private_server().await;
    let root = seed_root(&server).await;
    let mut receiver = mockito::Server::new_async().await;
    let _ok = receiver
        .mock("POST", "/ok")
        .with_status(200)
        .create_async()
        .await;
    let _bad = receiver
        .mock("POST", "/bad")
        .with_status(500)
        .create_async()
        .await;
    let ok = make_channel(
        &server,
        root,
        webhook_body("ok", &format!("{}/ok", receiver.url()), None),
    )
    .await;
    let bad = make_channel(
        &server,
        root,
        webhook_body("bad", &format!("{}/bad", receiver.url()), None),
    )
    .await;

    let call = |id: Uuid| {
        admin(
            root,
            server
                .client
                .post(server.url(&format!("/api/notification-channels/{id}/test"))),
        )
        .send()
    };
    let resp = call(ok).await.unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.json::<Value>().await.unwrap()["data"],
        json!({"delivered": true, "status_code": 200, "error": null})
    );
    let resp = call(bad).await.unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.json::<Value>().await.unwrap()["data"],
        json!({"delivered": false, "status_code": 500, "error": "http_5xx"})
    );

    let rows = outbox(&server).await;
    assert_eq!(rows.len(), 2);
    for r in &rows {
        assert_eq!(r["event"], "test");
        assert!(r["alert_id"].is_null());
    }
    assert!(rows.iter().any(|r| r["status"] == "delivered"));
    assert!(rows.iter().any(|r| r["status"] == "failed"));

    for _ in 0..8 {
        assert_eq!(call(ok).await.unwrap().status(), 200);
    }
    let limited = call(ok).await.unwrap();
    assert_eq!(limited.status(), 429);
    assert!(limited.headers().contains_key("retry-after"));
    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn deliveries_api_lists_history() {
    let server = TestServer::start().await;
    let root = seed_root(&server).await;
    let channel = seed_channel(&server, "hist", true).await;
    seed_route(&server, channel, None, "info").await;
    let mut conn = server.db.acquire().await.unwrap();
    let key = "hist:one";
    raise(
        &mut conn,
        &new_alert(key, AlertKind::BudgetSoft, Severity::Warning),
    )
    .await
    .unwrap();
    raise(
        &mut conn,
        &new_alert(key, AlertKind::BudgetSoft, Severity::Critical),
    )
    .await
    .unwrap();
    drop(conn);
    let alert_id: Uuid = sqlx::query_scalar("SELECT id FROM alerts WHERE dedup_key = $1")
        .bind(key)
        .fetch_one(&server.db)
        .await
        .unwrap();

    let resp = admin(
        root,
        server
            .client
            .get(server.url(&format!("/api/notification-deliveries?alert_id={alert_id}"))),
    )
    .send()
    .await
    .unwrap();
    assert_eq!(resp.status(), 200);
    let v = resp.json::<Value>().await.unwrap();
    let rows = v["data"].as_array().unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0]["event"], "escalated");
    assert_eq!(rows[1]["event"], "opened");
    for r in rows {
        assert!(r.get("payload").is_none());
        for f in [
            "id",
            "alert_id",
            "channel_id",
            "event",
            "status",
            "attempts",
            "next_attempt_at",
            "last_error",
            "delivered_at",
            "created_at",
        ] {
            assert!(r.get(f).is_some(), "missing {f}");
        }
    }
    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn member_gets_403_on_channel_routes() {
    let server = TestServer::start().await;
    let user = seed_user(&server, "chan-member", "member").await;
    let id = Uuid::new_v4();
    let base = "/api/notification-channels";
    let calls: Vec<(&str, String)> = vec![
        ("GET", base.to_owned()),
        ("POST", base.to_owned()),
        ("GET", format!("{base}/{id}")),
        ("PUT", format!("{base}/{id}")),
        ("DELETE", format!("{base}/{id}")),
        ("GET", format!("{base}/{id}/routes")),
        ("PUT", format!("{base}/{id}/routes")),
        ("POST", format!("{base}/{id}/test")),
        ("GET", "/api/notification-deliveries".to_owned()),
    ];
    for (method, path) in calls {
        // Typed bodies are parsed before the handler runs, so send valid ones.
        let body = match (method, path.as_str()) {
            ("POST", p) if p == base => {
                json!({"name": "n", "kind": "webhook", "url": "https://example.com/x"})
            }
            ("PUT", p) if p.ends_with("/routes") => json!({"routes": []}),
            _ => json!({}),
        };
        let rb = server
            .client
            .request(method.parse().unwrap(), server.url(&path));
        let resp = member(rb, user, "chan-member")
            .json(&body)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 403, "{method} {path}");
    }
    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn sweep_purges_only_old_finished_outbox_rows() {
    let server = TestServer::start().await;
    let channel = seed_channel(&server, "retention", true).await;
    let mut ids = Vec::new();
    for (status, age) in [
        ("delivered", "31 days"),
        ("failed", "31 days"),
        ("pending", "31 days"),
        ("delivered", "1 day"),
    ] {
        let id: Uuid = sqlx::query_scalar(&format!(
            "INSERT INTO notification_outbox (channel_id, event, payload, status, created_at) \
             VALUES ($1, 'opened', '{{}}'::jsonb, '{status}', now() - interval '{age}') \
             RETURNING id"
        ))
        .bind(channel)
        .fetch_one(&server.db)
        .await
        .unwrap();
        ids.push(id);
    }
    tick_resolve_sweep(&server.db, Utc::now()).await.unwrap();
    let left: Vec<Uuid> = sqlx::query_scalar("SELECT id FROM notification_outbox")
        .fetch_all(&server.db)
        .await
        .unwrap();
    assert_eq!(left.len(), 2);
    assert!(left.contains(&ids[2]) && left.contains(&ids[3]));
    server.cleanup().await;
}

// ─── llm call failures ───────────────────────────────────────────────────────
//
// The router reads `GatewayConfig::from_env()` once at boot, so these tests set
// the environment before `TestServer::start()` and are `#[serial]`. The agent is
// identified by a real agent JWT and the billed user comes from a live flow.

const ROUTER_JWT_SECRET: &str = "alerts-router-test-secret";
const FAILURE_POLL: Duration = Duration::from_secs(2);

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

async fn post_llm(server: &TestServer, path: &str, c: &Caller, body: &Value) -> reqwest::Response {
    server
        .client
        .post(format!("{}{path}", server.base_url))
        .bearer_auth(&c.jwt)
        .header("traceparent", &c.traceparent)
        .json(body)
        .send()
        .await
        .expect("llm request")
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

fn responses_body(stream: bool) -> Value {
    json!({"model": "gpt-4o-mini", "input": "hello", "stream": stream})
}

/// A user with an agent, a live flow and an agent JWT: everything a router call needs.
struct Caller {
    user: Uuid,
    agent: Uuid,
    jwt: String,
    traceparent: String,
}

async fn seed_agent(server: &TestServer, owner: Uuid, name: &str) -> Uuid {
    sqlx::query_scalar("INSERT INTO agents (name, owner_id) VALUES ($1, $2) RETURNING id")
        .bind(name)
        .bind(owner)
        .fetch_one(&server.db)
        .await
        .expect("seed agent")
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

async fn failure_rows(server: &TestServer, agent: Uuid) -> Vec<Value> {
    sqlx::query_scalar::<_, Value>(
        "SELECT to_jsonb(f) FROM llm_call_failures f WHERE agent_id = $1 ORDER BY created_at",
    )
    .bind(agent)
    .fetch_all(&server.db)
    .await
    .expect("failure rows")
}

/// The failure write is a detached best-effort task: poll until `want` rows exist
/// (or the deadline passes), then settle briefly so an unexpected second row would
/// be visible to the caller's exact-count assertion.
async fn wait_failures(server: &TestServer, agent: Uuid, want: usize) -> Vec<Value> {
    let deadline = std::time::Instant::now() + FAILURE_POLL;
    loop {
        let rows = failure_rows(server, agent).await;
        if rows.len() >= want || std::time::Instant::now() >= deadline {
            if want > 0 {
                tokio::time::sleep(Duration::from_millis(200)).await;
                return failure_rows(server, agent).await;
            }
            return rows;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn token_usage_count(server: &TestServer, agent: Uuid) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM token_usage WHERE agent_id = $1")
        .bind(agent)
        .fetch_one(&server.db)
        .await
        .expect("token_usage count")
}

async fn upstream_failing(path: &str, status: usize) -> mockito::ServerGuard {
    let mut upstream = mockito::Server::new_async().await;
    upstream
        .mock("POST", path)
        .with_status(status)
        .with_header("content-type", "application/json")
        .with_body(r#"{"error":{"message":"boom","type":"server_error"}}"#)
        .expect_at_least(1)
        .create_async()
        .await;
    set_router_env(&upstream.url());
    upstream
}

#[tokio::test]
#[serial]
async fn chat_upstream_500_records_one_failure() {
    let _upstream = upstream_failing("/chat/completions", 500).await;
    let server = TestServer::start().await;
    let c = caller(&server, "fail-chat").await;

    let resp = post_llm(&server, "/v1/chat/completions", &c, &chat_body()).await;
    assert!(!resp.status().is_success(), "{}", resp.status());
    let body = resp.json::<Value>().await.unwrap();
    assert!(body.get("detail").is_some(), "wire body unchanged: {body}");

    let rows = wait_failures(&server, c.agent, 1).await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    let r = &rows[0];
    assert_eq!(r["user_id"], c.user.to_string());
    assert_eq!(r["provider"], "openai");
    assert_eq!(r["model"], "gpt-4o-mini");
    assert_eq!(r["status_code"], 500);
    assert_eq!(r["error_kind"], "upstream_5xx");
    assert_eq!(r["streaming"], false);
    assert_eq!(r["operation_type"], "direct_llm");
    assert_eq!(token_usage_count(&server, c.agent).await, 0);
    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn chat_upstream_400_and_429_kinds() {
    for (status, kind) in [(400usize, "upstream_4xx"), (429, "rate_limited")] {
        let _upstream = upstream_failing("/chat/completions", status).await;
        let server = TestServer::start().await;
        let c = caller(&server, &format!("fail-kind-{status}")).await;
        let resp = post_llm(&server, "/v1/chat/completions", &c, &chat_body()).await;
        assert!(!resp.status().is_success());
        let rows = wait_failures(&server, c.agent, 1).await;
        assert_eq!(rows.len(), 1, "{status}: {rows:?}");
        assert_eq!(rows[0]["status_code"], status as i64);
        assert_eq!(rows[0]["error_kind"], kind);
        server.cleanup().await;
    }
}

#[tokio::test]
#[serial]
async fn stream_connect_failure_records_streaming() {
    let _upstream = upstream_failing("/chat/completions", 503).await;
    let server = TestServer::start().await;
    let c = caller(&server, "fail-stream").await;

    let resp = post_llm(&server, "/v1/chat/completions", &c, &stream_chat_body()).await;
    assert!(!resp.status().is_success());
    let rows = wait_failures(&server, c.agent, 1).await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0]["streaming"], true);
    assert_eq!(rows[0]["error_kind"], "upstream_5xx");
    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn embeddings_failure_records_embedding() {
    let _upstream = upstream_failing("/embeddings", 500).await;
    let server = TestServer::start().await;
    let c = caller(&server, "fail-embed").await;

    let resp = post_llm(&server, "/v1/embeddings", &c, &embeddings_body()).await;
    assert!(!resp.status().is_success());
    let rows = wait_failures(&server, c.agent, 1).await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0]["operation_type"], "embedding");
    assert_eq!(rows[0]["error_kind"], "upstream_5xx");
    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn responses_failure_records_one_row() {
    let _upstream = upstream_failing("/responses", 500).await;
    let server = TestServer::start().await;
    let c = caller(&server, "fail-responses").await;

    let resp = post_llm(&server, "/v1/responses", &c, &responses_body(false)).await;
    assert_eq!(resp.status(), 500);
    let rows = wait_failures(&server, c.agent, 1).await;
    assert_eq!(rows.len(), 1, "final outcome, not per attempt: {rows:?}");
    assert_eq!(rows[0]["status_code"], 500);
    assert_eq!(rows[0]["error_kind"], "upstream_5xx");
    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn budget_denied_records_nothing() {
    let mut upstream = mockito::Server::new_async().await;
    let chat_mock = upstream
        .mock("POST", "/chat/completions")
        .expect(0)
        .create_async()
        .await;
    set_router_env(&upstream.url());
    let server = TestServer::start().await;
    let root = seed_root(&server).await;
    let c = caller(&server, "fail-budget").await;
    sqlx::query(
        "INSERT INTO token_usage (user_id, operation_type, provider, model, cost_usd, created_at) \
         VALUES ($1, 'direct_llm', 'openai', 'gpt-4o-mini', 0.001, now())",
    )
    .bind(c.user)
    .execute(&server.db)
    .await
    .expect("seed usage");
    let budget = create_budget(&server, root, user_budget("fail-budget", c.user, 0.0001)).await;
    let key = nasiko_llm_router::budget::keys::spend_key(
        budget["id"].as_str().unwrap().parse().unwrap(),
        nasiko_llm_router::budget::period::period_bounds(
            nasiko_llm_router::budget::period::Period::Monthly,
            Utc::now(),
        )
        .0,
    );
    let redis = redis::Client::open(
        std::env::var("TEST_REDIS_URL").unwrap_or_else(|_| "redis://localhost:6379".into()),
    )
    .expect("redis client");
    let mut conn = redis
        .get_multiplexed_async_connection()
        .await
        .expect("redis conn");
    let _: () = redis::AsyncCommands::del(&mut conn, &key)
        .await
        .expect("del");

    let resp = post_llm(&server, "/v1/chat/completions", &c, &chat_body()).await;
    assert_eq!(resp.status(), 429);
    let rows = wait_failures(&server, c.agent, 0).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(rows.is_empty() && failure_rows(&server, c.agent).await.is_empty());
    chat_mock.assert_async().await;
    server.cleanup().await;
}

/// One valid SSE chunk, then the connection is aborted so the provider's byte
/// stream yields an error mid-flight.
fn broken_sse(w: &mut dyn std::io::Write, first: &str) -> std::io::Result<()> {
    w.write_all(first.as_bytes())?;
    w.flush()?;
    std::thread::sleep(Duration::from_millis(100));
    Err(std::io::Error::other("upstream stream aborted"))
}

#[tokio::test]
#[serial]
async fn mid_stream_error_records_stream_error() {
    let mut upstream = mockito::Server::new_async().await;
    let chunk = "data: {\"id\":\"x\",\"object\":\"chat.completion.chunk\",\"model\":\"gpt-4o-mini\",\
                 \"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"hi\"}}]}\n\n";
    upstream
        .mock("POST", "/chat/completions")
        .with_status(200)
        .with_header("content-type", "text/event-stream")
        .with_chunked_body(move |w| broken_sse(w, chunk))
        .expect_at_least(1)
        .create_async()
        .await;
    set_router_env(&upstream.url());
    let server = TestServer::start().await;
    let c = caller(&server, "fail-midstream").await;

    let resp = post_llm(&server, "/v1/chat/completions", &c, &stream_chat_body()).await;
    // Drain whatever the client receives; the stream may end early or error.
    let _ = resp.bytes().await;

    let rows = wait_failures(&server, c.agent, 1).await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0]["error_kind"], "stream_error");
    assert_eq!(rows[0]["streaming"], true);
    assert!(rows[0]["status_code"].is_null());

    let deadline = std::time::Instant::now() + FAILURE_POLL;
    let finish = loop {
        let f: Option<Option<String>> =
            sqlx::query_scalar("SELECT finish_reason FROM token_usage WHERE agent_id = $1")
                .bind(c.agent)
                .fetch_optional(&server.db)
                .await
                .unwrap();
        if let Some(f) = f {
            break f;
        }
        assert!(std::time::Instant::now() < deadline, "no token_usage row");
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    assert_eq!(finish.as_deref(), Some("failed:stream"));
    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn responses_mid_stream_error_records_stream_error() {
    let mut upstream = mockito::Server::new_async().await;
    let frame = "event: response.output_text.delta\n\
                 data: {\"type\":\"response.output_text.delta\",\"delta\":\"hi\"}\n\n";
    upstream
        .mock("POST", "/responses")
        .with_status(200)
        .with_header("content-type", "text/event-stream")
        .with_chunked_body(move |w| broken_sse(w, frame))
        .expect_at_least(1)
        .create_async()
        .await;
    set_router_env(&upstream.url());
    let server = TestServer::start().await;
    let c = caller(&server, "fail-resp-midstream").await;

    let resp = post_llm(&server, "/v1/responses", &c, &responses_body(true)).await;
    let _ = resp.bytes().await;

    let rows = wait_failures(&server, c.agent, 1).await;
    assert_eq!(rows.len(), 1, "one per request, not per frame: {rows:?}");
    assert_eq!(rows[0]["error_kind"], "stream_error");
    assert_eq!(rows[0]["streaming"], true);
    server.cleanup().await;
}

// ─── monitors ────────────────────────────────────────────────────────────────

use nasiko_server::alerts::monitors::{MAX_ENABLED_MONITORS, RESOLVE_CLEAR_EVALS, tick_monitors};

async fn post_monitor(server: &TestServer, root: Uuid, body: Value) -> reqwest::Response {
    admin(root, server.client.post(server.url("/api/monitors")))
        .json(&body)
        .send()
        .await
        .expect("post monitor")
}

async fn make_monitor(server: &TestServer, root: Uuid, body: Value) -> Value {
    let resp = post_monitor(server, root, body).await;
    assert_eq!(resp.status(), 201, "create monitor");
    resp.json::<Value>().await.unwrap()["data"].clone()
}

fn agent_monitor(agent: Uuid) -> Value {
    json!({"name": "agent errors", "metric": "error_rate", "scope": "agent",
           "scope_ref": agent.to_string(), "window_minutes": 15, "threshold": 10.0})
}

async fn assert_monitor_400(server: &TestServer, root: Uuid, body: Value, code: &str) {
    let resp = post_monitor(server, root, body.clone()).await;
    assert_eq!(resp.status(), 400, "{body}");
    let json = resp.json::<Value>().await.unwrap();
    assert_eq!(json["code"], code, "{body} -> {json}");
}

/// Insert `n` router-metered rows one minute old (inside every window).
async fn seed_calls(
    server: &TestServer,
    user: Uuid,
    agent: Uuid,
    model: &str,
    n: usize,
    finish_reason: Option<&str>,
    latency_ms: impl Fn(usize) -> i32,
) {
    for i in 0..n {
        sqlx::query(
            "INSERT INTO token_usage (user_id, agent_id, operation_type, provider, model, \
             cost_usd, latency_ms, finish_reason, created_at) \
             VALUES ($1, $2, 'direct_llm', 'openai', $3, 0.001, $4, $5, \
                     now() - interval '1 minute')",
        )
        .bind(user)
        .bind(agent)
        .bind(model)
        .bind(latency_ms(i))
        .bind(finish_reason)
        .execute(&server.db)
        .await
        .expect("seed call");
    }
}

async fn seed_failures(server: &TestServer, agent: Uuid, model: &str, n: usize) {
    for _ in 0..n {
        sqlx::query(
            "INSERT INTO llm_call_failures (agent_id, provider, model, status_code, error_kind, \
             operation_type, created_at) \
             VALUES ($1, 'openai', $2, 500, 'upstream_5xx', 'direct_llm', \
                     now() - interval '1 minute')",
        )
        .bind(agent)
        .bind(model)
        .execute(&server.db)
        .await
        .expect("seed failure");
    }
}

async fn monitor_alert(server: &TestServer, monitor_id: &str) -> Vec<Value> {
    alert_by_key(server, &format!("monitor:{monitor_id}")).await
}

async fn member_agent(server: &TestServer, name: &str) -> (Uuid, Uuid) {
    let user = seed_user(server, name, "member").await;
    let agent = seed_agent(server, user, &format!("{name}-agent")).await;
    (user, agent)
}

#[tokio::test]
#[serial]
async fn monitor_crud_and_validation() {
    let server = TestServer::start().await;
    let root = seed_root(&server).await;
    let (user, agent) = member_agent(&server, "mon-crud").await;

    let created = make_monitor(&server, root, agent_monitor(agent)).await;
    let id = created["id"].as_str().unwrap().to_owned();
    assert_eq!(created["min_samples"], 20);
    assert_eq!(created["severity"], "warning");
    assert_eq!(created["enabled"], true);
    assert_eq!(created["scope_ref"], agent.to_string());
    assert_eq!(created["metric"], "error_rate");
    assert_eq!(created["window_minutes"], 15);
    assert_eq!(created["threshold"].as_f64(), Some(10.0));

    let resp = admin(root, server.client.get(server.url("/api/monitors")))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let list = resp.json::<Value>().await.unwrap();
    assert_eq!(list["data"].as_array().unwrap().len(), 1);

    let resp = admin(
        root,
        server
            .client
            .get(server.url(&format!("/api/monitors/{id}"))),
    )
    .send()
    .await
    .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.json::<Value>().await.unwrap()["data"]["id"], id);

    let resp = admin(
        root,
        server
            .client
            .put(server.url(&format!("/api/monitors/{id}"))),
    )
    .json(&json!({"threshold": 25.5, "enabled": false, "severity": "critical"}))
    .send()
    .await
    .unwrap();
    assert_eq!(resp.status(), 200);
    let updated = resp.json::<Value>().await.unwrap()["data"].clone();
    assert_eq!(updated["threshold"].as_f64(), Some(25.5));
    assert_eq!(updated["enabled"], false);
    assert_eq!(updated["severity"], "critical");
    assert_eq!(updated["window_minutes"], 15);

    // PUT validates the same way.
    let resp = admin(
        root,
        server
            .client
            .put(server.url(&format!("/api/monitors/{id}"))),
    )
    .json(&json!({"window_minutes": 4}))
    .send()
    .await
    .unwrap();
    assert_eq!(resp.status(), 400);
    assert_eq!(
        resp.json::<Value>().await.unwrap()["code"],
        "invalid_window"
    );

    let resp = admin(
        root,
        server
            .client
            .delete(server.url(&format!("/api/monitors/{id}"))),
    )
    .send()
    .await
    .unwrap();
    assert!(resp.status().is_success());
    let resp = admin(
        root,
        server
            .client
            .get(server.url(&format!("/api/monitors/{id}"))),
    )
    .send()
    .await
    .unwrap();
    assert_eq!(resp.status(), 404);

    let base = agent_monitor(agent);
    let with = |k: &str, v: Value| {
        let mut b = base.clone();
        b[k] = v;
        b
    };
    assert_monitor_400(&server, root, with("name", json!("")), "invalid_name").await;
    assert_monitor_400(
        &server,
        root,
        with("name", json!("n".repeat(121))),
        "invalid_name",
    )
    .await;
    assert_monitor_400(
        &server,
        root,
        with("metric", json!("rps")),
        "invalid_metric",
    )
    .await;
    assert_monitor_400(&server, root, with("scope", json!("user")), "invalid_scope").await;
    assert_monitor_400(
        &server,
        root,
        with("scope_ref", json!("not-a-uuid")),
        "invalid_scope_ref",
    )
    .await;
    assert_monitor_400(
        &server,
        root,
        with("scope_ref", json!(Uuid::new_v4().to_string())),
        "invalid_scope_ref",
    )
    .await;
    assert_monitor_400(
        &server,
        root,
        json!({"name": "m", "metric": "error_rate", "scope": "model", "scope_ref": "",
               "window_minutes": 15, "threshold": 10}),
        "invalid_scope_ref",
    )
    .await;
    assert_monitor_400(
        &server,
        root,
        json!({"name": "m", "metric": "error_rate", "scope": "platform",
               "scope_ref": "x", "window_minutes": 15, "threshold": 10}),
        "invalid_scope_ref",
    )
    .await;
    assert_monitor_400(
        &server,
        root,
        with("window_minutes", json!(4)),
        "invalid_window",
    )
    .await;
    assert_monitor_400(
        &server,
        root,
        with("window_minutes", json!(1441)),
        "invalid_window",
    )
    .await;
    assert_monitor_400(
        &server,
        root,
        with("threshold", json!(0)),
        "invalid_threshold",
    )
    .await;
    assert_monitor_400(
        &server,
        root,
        with("threshold", json!(-1)),
        "invalid_threshold",
    )
    .await;
    assert_monitor_400(
        &server,
        root,
        with("threshold", json!(100.5)),
        "invalid_threshold",
    )
    .await;
    assert_monitor_400(
        &server,
        root,
        with("min_samples", json!(0)),
        "invalid_min_samples",
    )
    .await;
    assert_monitor_400(
        &server,
        root,
        with("severity", json!("fatal")),
        "invalid_severity",
    )
    .await;

    // Cap: MAX_ENABLED_MONITORS enabled monitors; the next enabled one is refused,
    // a disabled one is accepted and cannot be enabled past the cap.
    for i in 0..MAX_ENABLED_MONITORS {
        sqlx::query(
            "INSERT INTO monitors (name, metric, scope, window_minutes, threshold) \
             VALUES ($1, 'error_rate', 'platform', 15, 10)",
        )
        .bind(format!("bulk {i}"))
        .execute(&server.db)
        .await
        .unwrap();
    }
    let platform = json!({"name": "over", "metric": "p95_latency_ms", "scope": "platform",
                          "window_minutes": 15, "threshold": 1000});
    assert_monitor_400(&server, root, platform.clone(), "monitor_limit_reached").await;
    let mut disabled = platform.clone();
    disabled["enabled"] = json!(false);
    let spare = make_monitor(&server, root, disabled).await;
    let resp = admin(
        root,
        server
            .client
            .put(server.url(&format!("/api/monitors/{}", spare["id"].as_str().unwrap()))),
    )
    .json(&json!({"enabled": true}))
    .send()
    .await
    .unwrap();
    assert_eq!(resp.status(), 400);
    assert_eq!(
        resp.json::<Value>().await.unwrap()["code"],
        "monitor_limit_reached"
    );

    // Members get 403 on all five routes.
    let m = member(
        server.client.get(server.url("/api/monitors")),
        user,
        "mon-crud",
    );
    assert_eq!(m.send().await.unwrap().status(), 403);
    let body = agent_monitor(agent);
    let m = member(
        server.client.post(server.url("/api/monitors")),
        user,
        "mon-crud",
    );
    assert_eq!(m.json(&body).send().await.unwrap().status(), 403);
    let url = server.url(&format!("/api/monitors/{id}"));
    for method in [
        reqwest::Method::GET,
        reqwest::Method::PUT,
        reqwest::Method::DELETE,
    ] {
        let m = member(
            server.client.request(method.clone(), &url),
            user,
            "mon-crud",
        );
        let m = if method == reqwest::Method::PUT {
            m.json(&json!({"enabled": false}))
        } else {
            m
        };
        assert_eq!(m.send().await.unwrap().status(), 403, "{method}");
    }
    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn error_rate_breach_and_hysteresis() {
    let server = TestServer::start().await;
    let root = seed_root(&server).await;
    let (user, agent) = member_agent(&server, "mon-rate").await;
    let channel = seed_channel(&server, "mon-rate-ch", true).await;
    seed_route(&server, channel, None, "info").await;
    let m = make_monitor(&server, root, agent_monitor(agent)).await;
    let id = m["id"].as_str().unwrap().to_owned();

    seed_calls(&server, user, agent, "gpt-4o-mini", 18, None, |_| 100).await;
    seed_failures(&server, agent, "gpt-4o-mini", 4).await;
    // Responses failed-attempt rows are neither successes nor samples.
    seed_calls(
        &server,
        user,
        agent,
        "gpt-4o-mini",
        5,
        Some("failed:upstream"),
        |_| 1,
    )
    .await;
    seed_calls(
        &server,
        user,
        agent,
        "gpt-4o-mini",
        5,
        Some("http:500"),
        |_| 1,
    )
    .await;

    assert!(tick_monitors(&server.db, Utc::now()).await.unwrap() >= 1);
    let alerts = monitor_alert(&server, &id).await;
    assert_eq!(alerts.len(), 1, "{alerts:?}");
    let a = &alerts[0];
    assert_eq!(a["kind"], "monitor_breach");
    assert_eq!(a["status"], "open");
    assert_eq!(a["scope"], "agent");
    assert_eq!(a["scope_ref"], agent.to_string());
    assert_eq!(a["severity"], "warning");
    assert_eq!(a["link"], format!("/tokenops?agent={agent}"));
    let d = &a["details"];
    assert_eq!(d["metric"], "error_rate");
    assert!((d["value"].as_f64().unwrap() - 18.18).abs() < 0.1, "{d}");
    assert_eq!(d["threshold"].as_f64(), Some(10.0));
    assert_eq!(d["window_minutes"], 15);
    assert_eq!(d["samples"], 22);
    assert_eq!(d["clear_streak"], 0);

    tick_monitors(&server.db, Utc::now()).await.unwrap();
    let alerts = monitor_alert(&server, &id).await;
    assert_eq!(alerts.len(), 1);
    assert_eq!(alerts[0]["occurrences"], 2);

    // Recovery: no failures and enough clean samples.
    sqlx::query("DELETE FROM llm_call_failures WHERE agent_id = $1")
        .bind(agent)
        .execute(&server.db)
        .await
        .unwrap();
    seed_calls(&server, user, agent, "gpt-4o-mini", 5, None, |_| 100).await;
    for expected in 1..RESOLVE_CLEAR_EVALS {
        tick_monitors(&server.db, Utc::now()).await.unwrap();
        let a = &monitor_alert(&server, &id).await[0];
        assert_eq!(a["status"], "open", "streak {expected}");
        assert_eq!(a["details"]["clear_streak"], expected);
    }
    tick_monitors(&server.db, Utc::now()).await.unwrap();
    let alerts = monitor_alert(&server, &id).await;
    assert_eq!(alerts.len(), 1);
    assert_eq!(alerts[0]["status"], "resolved");
    let events: Vec<String> = outbox(&server)
        .await
        .iter()
        .map(|o| o["event"].as_str().unwrap().to_owned())
        .collect();
    assert!(events.contains(&"opened".to_owned()), "{events:?}");
    assert!(events.contains(&"resolved".to_owned()), "{events:?}");
    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn p95_breach_by_model() {
    let server = TestServer::start().await;
    let root = seed_root(&server).await;
    let (user, agent) = member_agent(&server, "mon-p95").await;
    let slow = make_monitor(
        &server,
        root,
        json!({"name": "slow mini", "metric": "p95_latency_ms", "scope": "model",
               "scope_ref": "gpt-4o-mini", "window_minutes": 15, "threshold": 1000}),
    )
    .await;
    let idle = make_monitor(
        &server,
        root,
        json!({"name": "idle model", "metric": "p95_latency_ms", "scope": "model",
               "scope_ref": "some-other-model", "window_minutes": 15, "threshold": 1000}),
    )
    .await;

    seed_calls(&server, user, agent, "gpt-4o-mini", 25, None, |i| {
        100 + 100 * i as i32
    })
    .await;
    // Failed attempts carry tiny latencies and must not drag p95 down.
    seed_calls(
        &server,
        user,
        agent,
        "gpt-4o-mini",
        60,
        Some("failed:upstream"),
        |_| 1,
    )
    .await;

    tick_monitors(&server.db, Utc::now()).await.unwrap();
    let alerts = monitor_alert(&server, slow["id"].as_str().unwrap()).await;
    assert_eq!(alerts.len(), 1, "{alerts:?}");
    assert_eq!(alerts[0]["scope"], "model");
    assert_eq!(alerts[0]["scope_ref"], "gpt-4o-mini");
    assert!(alerts[0]["details"]["value"].as_f64().unwrap() > 1000.0);
    assert_eq!(alerts[0]["details"]["samples"], 25);
    assert!(
        monitor_alert(&server, idle["id"].as_str().unwrap())
            .await
            .is_empty()
    );
    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn min_samples_gates_both_ways() {
    let server = TestServer::start().await;
    let root = seed_root(&server).await;
    let (user, agent) = member_agent(&server, "mon-gate").await;
    let m = make_monitor(&server, root, agent_monitor(agent)).await;
    let id = m["id"].as_str().unwrap().to_owned();

    // 5 samples, all failing: below min_samples, no alert.
    seed_failures(&server, agent, "gpt-4o-mini", 5).await;
    tick_monitors(&server.db, Utc::now()).await.unwrap();
    assert!(monitor_alert(&server, &id).await.is_empty());

    // Enough failing samples to open it.
    seed_failures(&server, agent, "gpt-4o-mini", 20).await;
    tick_monitors(&server.db, Utc::now()).await.unwrap();
    assert_eq!(monitor_alert(&server, &id).await.len(), 1);

    // One clear evaluation, then the window empties out.
    sqlx::query("DELETE FROM llm_call_failures WHERE agent_id = $1")
        .bind(agent)
        .execute(&server.db)
        .await
        .unwrap();
    seed_calls(&server, user, agent, "gpt-4o-mini", 25, None, |_| 100).await;
    tick_monitors(&server.db, Utc::now()).await.unwrap();
    assert_eq!(
        monitor_alert(&server, &id).await[0]["details"]["clear_streak"],
        1
    );

    sqlx::query("DELETE FROM token_usage WHERE agent_id = $1")
        .bind(agent)
        .execute(&server.db)
        .await
        .unwrap();
    for _ in 0..4 {
        tick_monitors(&server.db, Utc::now()).await.unwrap();
    }
    let a = &monitor_alert(&server, &id).await[0];
    assert_eq!(a["status"], "open");
    assert_eq!(a["details"]["clear_streak"], 1);
    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn monitor_tick_skips_when_lock_held() {
    let server = TestServer::start().await;
    let root = seed_root(&server).await;
    let (user, agent) = member_agent(&server, "mon-lock").await;
    let m = make_monitor(&server, root, agent_monitor(agent)).await;
    let id = m["id"].as_str().unwrap().to_owned();

    seed_failures(&server, agent, "gpt-4o-mini", 25).await;
    tick_monitors(&server.db, Utc::now()).await.unwrap();
    let a = monitor_alert(&server, &id).await;
    assert_eq!(
        (
            a[0]["occurrences"].as_i64(),
            a[0]["details"]["clear_streak"].as_i64()
        ),
        (Some(1), Some(0))
    );

    let mut other = sqlx::PgConnection::connect(&server.db_url).await.unwrap();
    sqlx::query("BEGIN").execute(&mut other).await.unwrap();
    let got: bool = sqlx::query_scalar(
        "SELECT pg_try_advisory_xact_lock(hashtextextended('alerts:monitor:' || $1, 0))",
    )
    .bind(&id)
    .fetch_one(&mut other)
    .await
    .unwrap();
    assert!(got);

    // Breach data present: occurrences must not move while another replica holds the lock.
    tick_monitors(&server.db, Utc::now()).await.unwrap();
    assert_eq!(monitor_alert(&server, &id).await[0]["occurrences"], 1);

    // Clear data present: clear_streak must not move either.
    sqlx::query("DELETE FROM llm_call_failures WHERE agent_id = $1")
        .bind(agent)
        .execute(&server.db)
        .await
        .unwrap();
    seed_calls(&server, user, agent, "gpt-4o-mini", 25, None, |_| 100).await;
    tick_monitors(&server.db, Utc::now()).await.unwrap();
    assert_eq!(
        monitor_alert(&server, &id).await[0]["details"]["clear_streak"],
        0
    );

    sqlx::query("ROLLBACK").execute(&mut other).await.unwrap();
    tick_monitors(&server.db, Utc::now()).await.unwrap();
    assert_eq!(
        monitor_alert(&server, &id).await[0]["details"]["clear_streak"],
        1
    );
    drop(other);
    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn disabled_or_deleted_monitor_resolves_alert() {
    let server = TestServer::start().await;
    let root = seed_root(&server).await;
    let (_user, agent) = member_agent(&server, "mon-sweep").await;
    let disabled = make_monitor(&server, root, agent_monitor(agent)).await;
    let deleted = make_monitor(&server, root, agent_monitor(agent)).await;
    let kept = make_monitor(&server, root, agent_monitor(agent)).await;
    let ids: Vec<String> = [&disabled, &deleted, &kept]
        .iter()
        .map(|m| m["id"].as_str().unwrap().to_owned())
        .collect();

    for id in &ids {
        let mut conn = server.db.acquire().await.unwrap();
        raise(
            &mut conn,
            &NewAlert {
                kind: AlertKind::MonitorBreach,
                severity: Severity::Warning,
                scope: AlertScope::Agent,
                scope_ref: Some(agent.to_string()),
                dedup_key: format!("monitor:{id}"),
                title: "t".into(),
                message: "m".into(),
                link: "/tokenops".into(),
                details: json!({"monitor_id": id, "clear_streak": 0}),
            },
        )
        .await
        .unwrap();
    }

    let resp = admin(
        root,
        server
            .client
            .put(server.url(&format!("/api/monitors/{}", ids[0]))),
    )
    .json(&json!({"enabled": false}))
    .send()
    .await
    .unwrap();
    assert_eq!(resp.status(), 200);
    let resp = admin(
        root,
        server
            .client
            .delete(server.url(&format!("/api/monitors/{}", ids[1]))),
    )
    .send()
    .await
    .unwrap();
    assert!(resp.status().is_success());

    tick_resolve_sweep(&server.db, Utc::now()).await.unwrap();
    assert_eq!(
        monitor_alert(&server, &ids[0]).await[0]["status"],
        "resolved"
    );
    assert_eq!(
        monitor_alert(&server, &ids[1]).await[0]["status"],
        "resolved"
    );
    assert_eq!(monitor_alert(&server, &ids[2]).await[0]["status"], "open");
    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn sweep_purges_only_old_llm_call_failures() {
    let server = TestServer::start().await;
    for age in ["31 days", "1 day"] {
        sqlx::query(&format!(
            "INSERT INTO llm_call_failures (provider, model, error_kind, operation_type, created_at) \
             VALUES ('openai', 'm', 'upstream_5xx', 'direct_llm', now() - interval '{age}')"
        ))
        .execute(&server.db)
        .await
        .unwrap();
    }
    tick_resolve_sweep(&server.db, Utc::now()).await.unwrap();
    let left: i64 = sqlx::query_scalar("SELECT count(*) FROM llm_call_failures")
        .fetch_one(&server.db)
        .await
        .unwrap();
    assert_eq!(left, 1);
    server.cleanup().await;
}

// ─── spend spikes ────────────────────────────────────────────────────────────

use nasiko_server::alerts::{SpikeSettings, tick_spike};

const SPIKE_SETTINGS: SpikeSettings = SpikeSettings {
    sigma: 3.0,
    floor_usd: 5.0,
};

/// Current hour + 5 minutes: `latest` is then the previous complete hour.
fn spike_now() -> DateTime<Utc> {
    let now = Utc::now();
    let hour_start = Utc
        .with_ymd_and_hms(now.year(), now.month(), now.day(), now.hour(), 0, 0)
        .unwrap();
    hour_start + chrono::Duration::minutes(5)
}

fn hour_of(t: DateTime<Utc>) -> DateTime<Utc> {
    t - chrono::Duration::minutes(5)
}

fn hour_key(h: DateTime<Utc>) -> String {
    h.format("%Y-%m-%dT%H:00Z").to_string()
}

async fn spike_usage(
    server: &TestServer,
    user: Uuid,
    agent: Option<Uuid>,
    op: &str,
    cost: f64,
    at: DateTime<Utc>,
) {
    sqlx::query(
        "INSERT INTO token_usage (user_id, agent_id, operation_type, provider, model, cost_usd, created_at) \
         VALUES ($1, $2, $3, 'openai', 'gpt-4o-mini', $4::float8::numeric, $5)",
    )
    .bind(user)
    .bind(agent)
    .bind(op)
    .bind(cost)
    .bind(at)
    .execute(&server.db)
    .await
    .expect("seed spike usage");
}

/// One row per hour for `hours` complete hours before `latest`, mid-hour.
async fn seed_baseline(
    server: &TestServer,
    user: Uuid,
    agent: Uuid,
    latest: DateTime<Utc>,
    hours: i64,
    cost: f64,
) {
    for i in 1..=hours {
        let at = latest - chrono::Duration::hours(i) + chrono::Duration::minutes(10);
        spike_usage(server, user, Some(agent), "direct_llm", cost, at).await;
    }
}

async fn spike_alerts(server: &TestServer) -> Vec<Value> {
    sqlx::query_scalar::<_, Value>(
        "SELECT to_jsonb(a) FROM alerts a WHERE kind = 'spend_spike' ORDER BY dedup_key",
    )
    .fetch_all(&server.db)
    .await
    .expect("spike alerts")
}

#[tokio::test]
#[serial]
async fn spike_raises_for_agent_and_platform() {
    let server = TestServer::start().await;
    let (user, a) = member_agent(&server, "spike-a").await;
    let now = spike_now();
    let latest = hour_of(now) - chrono::Duration::hours(1);
    seed_baseline(&server, user, a, latest, 30, 1.0).await;
    for _ in 0..3 {
        let at = latest + chrono::Duration::minutes(20);
        spike_usage(&server, user, Some(a), "direct_llm", 20.0, at).await;
    }

    assert_eq!(
        tick_spike(&server.db, &SPIKE_SETTINGS, now).await.unwrap(),
        2
    );
    let key = hour_key(latest);
    let agent_alert = alert_by_key(&server, &format!("spike:agent:{a}:{key}")).await;
    assert_eq!(agent_alert.len(), 1);
    let al = &agent_alert[0];
    assert_eq!(al["status"], "open");
    assert_eq!(al["severity"], "critical");
    assert_eq!(al["scope"], "agent");
    assert_eq!(al["scope_ref"], a.to_string());
    assert_eq!(al["link"], format!("/tokenops?agent={a}&range=24h"));
    for k in [
        "hour_start",
        "spend_usd",
        "threshold_usd",
        "baseline_mean",
        "baseline_std",
    ] {
        assert!(!al["details"][k].is_null(), "details.{k}");
    }
    assert!((al["details"]["spend_usd"].as_f64().unwrap() - 60.0).abs() < 1e-6);

    let platform = alert_by_key(&server, &format!("spike:platform:all:{key}")).await;
    assert_eq!(platform.len(), 1);
    assert_eq!(platform[0]["scope"], "platform");
    assert!(platform[0]["scope_ref"].is_null());
    assert_eq!(platform[0]["link"], "/tokenops?range=24h");

    tick_spike(&server.db, &SPIKE_SETTINGS, now).await.unwrap();
    assert_eq!(spike_alerts(&server).await.len(), 2);
    let again = alert_by_key(&server, &format!("spike:agent:{a}:{key}")).await;
    assert_eq!(again[0]["occurrences"], 2);
    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn spike_respects_floor_and_cold_start() {
    let server = TestServer::start().await;
    let (user_b, b) = member_agent(&server, "spike-b").await;
    let (user_c, c) = member_agent(&server, "spike-c").await;
    let now = spike_now();
    let latest = hour_of(now) - chrono::Duration::hours(1);
    let at = latest + chrono::Duration::minutes(20);

    seed_baseline(&server, user_b, b, latest, 30, 0.01).await;
    spike_usage(&server, user_b, Some(b), "direct_llm", 4.0, at).await;
    seed_baseline(&server, user_c, c, latest, 10, 1.0).await;
    spike_usage(&server, user_c, Some(c), "direct_llm", 50.0, at).await;

    tick_spike(&server.db, &SPIKE_SETTINGS, now).await.unwrap();
    let agent_scoped: Vec<_> = spike_alerts(&server)
        .await
        .into_iter()
        .filter(|a| a["scope"] == "agent")
        .collect();
    assert!(agent_scoped.is_empty(), "{agent_scoped:?}");
    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn embedding_rows_count_other_operations_do_not() {
    let server = TestServer::start().await;
    let (user, a) = member_agent(&server, "spike-op").await;
    let now = spike_now();
    let latest = hour_of(now) - chrono::Duration::hours(1);
    seed_baseline(&server, user, a, latest, 30, 1.0).await;
    let at = latest + chrono::Duration::minutes(20);

    spike_usage(&server, user, Some(a), "orchestrator", 90.0, at).await;
    tick_spike(&server.db, &SPIKE_SETTINGS, now).await.unwrap();
    assert!(spike_alerts(&server).await.is_empty());

    spike_usage(&server, user, Some(a), "embedding", 50.0, at).await;
    tick_spike(&server.db, &SPIKE_SETTINGS, now).await.unwrap();
    let key = format!("spike:agent:{a}:{}", hour_key(latest));
    let alert = alert_by_key(&server, &key).await;
    assert_eq!(alert.len(), 1);
    assert!((alert[0]["details"]["spend_usd"].as_f64().unwrap() - 50.0).abs() < 1e-6);
    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn spike_resolves_next_quiet_hour_or_after_24h() {
    let server = TestServer::start().await;
    let (user, a) = member_agent(&server, "spike-res").await;
    let ch = seed_channel(&server, "spike-ch", true).await;
    seed_route(&server, ch, Some("spend_spike"), "info").await;
    let now = spike_now();
    let latest = hour_of(now) - chrono::Duration::hours(1);
    seed_baseline(&server, user, a, latest, 30, 1.0).await;
    let at = latest + chrono::Duration::minutes(20);
    spike_usage(&server, user, Some(a), "direct_llm", 60.0, at).await;
    tick_spike(&server.db, &SPIKE_SETTINGS, now).await.unwrap();

    let next_hour = hour_of(now);
    spike_usage(
        &server,
        user,
        Some(a),
        "direct_llm",
        1.0,
        next_hour + chrono::Duration::minutes(10),
    )
    .await;
    let later = now + chrono::Duration::hours(1);
    tick_spike(&server.db, &SPIKE_SETTINGS, later)
        .await
        .unwrap();
    let key = format!("spike:agent:{a}:{}", hour_key(latest));
    assert_eq!(alert_by_key(&server, &key).await[0]["status"], "resolved");
    let events: Vec<String> = outbox(&server)
        .await
        .iter()
        .map(|o| o["event"].as_str().unwrap().to_owned())
        .collect();
    assert!(events.iter().any(|e| e == "resolved"), "{events:?}");

    // A stale open spike alert is closed by the sweep after 24h.
    let stale_key = "spike:agent:stale:old";
    sqlx::query(
        "INSERT INTO alerts (kind, severity, scope, dedup_key, status, title, message, link, details, first_seen_at) \
         VALUES ('spend_spike', 'warning', 'platform', $1, 'open', 't', 'm', '/tokenops', '{}', now() - interval '25 hours')",
    )
    .bind(stale_key)
    .execute(&server.db)
    .await
    .unwrap();
    tick_resolve_sweep(&server.db, Utc::now()).await.unwrap();
    assert_eq!(
        alert_by_key(&server, stale_key).await[0]["status"],
        "resolved"
    );
    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn spike_resolves_when_next_hour_has_no_rows() {
    let server = TestServer::start().await;
    let (user, a) = member_agent(&server, "spike-quiet").await;
    let now = spike_now();
    let latest = hour_of(now) - chrono::Duration::hours(1);
    seed_baseline(&server, user, a, latest, 30, 1.0).await;
    spike_usage(
        &server,
        user,
        Some(a),
        "direct_llm",
        60.0,
        latest + chrono::Duration::minutes(20),
    )
    .await;
    tick_spike(&server.db, &SPIKE_SETTINGS, now).await.unwrap();
    assert_eq!(spike_alerts(&server).await.len(), 2);

    // Whole platform silent in the next hour.
    tick_spike(
        &server.db,
        &SPIKE_SETTINGS,
        now + chrono::Duration::hours(1),
    )
    .await
    .unwrap();
    let all = spike_alerts(&server).await;
    assert_eq!(all.len(), 2);
    assert!(all.iter().all(|a| a["status"] == "resolved"), "{all:?}");
    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn spike_skips_when_lock_held() {
    let server = TestServer::start().await;
    let (user, a) = member_agent(&server, "spike-lock").await;
    let now = spike_now();
    let latest = hour_of(now) - chrono::Duration::hours(1);
    seed_baseline(&server, user, a, latest, 30, 1.0).await;
    spike_usage(
        &server,
        user,
        Some(a),
        "direct_llm",
        60.0,
        latest + chrono::Duration::minutes(20),
    )
    .await;

    let mut other = sqlx::PgConnection::connect(&server.db_url).await.unwrap();
    sqlx::query("BEGIN").execute(&mut other).await.unwrap();
    let got: bool =
        sqlx::query_scalar("SELECT pg_try_advisory_xact_lock(hashtext('alerts:spike'))")
            .fetch_one(&mut other)
            .await
            .unwrap();
    assert!(got);
    assert_eq!(
        tick_spike(&server.db, &SPIKE_SETTINGS, now).await.unwrap(),
        0
    );
    assert!(spike_alerts(&server).await.is_empty());

    sqlx::query("ROLLBACK").execute(&mut other).await.unwrap();
    assert!(tick_spike(&server.db, &SPIKE_SETTINGS, now).await.unwrap() > 0);
    drop(other);
    server.cleanup().await;
}

async fn raise_marker(
    server: &TestServer,
    agent: Option<Uuid>,
    hour: DateTime<Utc>,
    title: &str,
) -> Uuid {
    let (scope, scope_ref, key) = match agent {
        Some(id) => (
            AlertScope::Agent,
            Some(id.to_string()),
            format!("spike:agent:{id}:{}", hour_key(hour)),
        ),
        None => (
            AlertScope::Platform,
            None,
            format!("spike:platform:all:{}", hour_key(hour)),
        ),
    };
    let mut conn = sqlx::PgConnection::connect(&server.db_url).await.unwrap();
    let outcome = raise(
        &mut conn,
        &NewAlert {
            kind: AlertKind::SpendSpike,
            severity: Severity::Warning,
            scope,
            scope_ref,
            dedup_key: key,
            title: title.to_owned(),
            message: "m".into(),
            link: "/tokenops".into(),
            details: json!({
                "hour_start": hour.to_rfc3339_opts(SecondsFormat::Secs, true),
                "spend_usd": 12.5, "threshold_usd": 3.0,
            }),
        },
    )
    .await
    .unwrap();
    match outcome {
        RaiseOutcome::Opened(id) => id,
        other => panic!("expected Opened, got {other:?}"),
    }
}

async fn markers(
    server: &TestServer,
    who: Option<(Uuid, &str, bool)>,
    query: &[(&str, String)],
) -> (u16, Value) {
    let mut rb = server.client.get(server.url("/api/alerts/spike-markers"));
    rb = match who {
        Some((id, name, true)) => common::as_superuser(rb, &id.to_string(), name),
        Some((id, name, false)) => member(rb, id, name),
        None => rb,
    };
    let resp = rb.query(query).send().await.expect("markers");
    let status = resp.status().as_u16();
    (status, resp.json().await.unwrap_or(Value::Null))
}

#[tokio::test]
#[serial]
async fn spike_markers_scoping() {
    let server = TestServer::start().await;
    let root = seed_root(&server).await;
    let (viewer, a) = member_agent(&server, "mark-a").await;
    let (_other, b) = member_agent(&server, "mark-b").await;
    let hour = hour_of(spike_now()) - chrono::Duration::hours(2);
    let id_a = raise_marker(&server, Some(a), hour, "A spike").await;
    raise_marker(&server, Some(b), hour, "B spike").await;
    raise_marker(&server, None, hour, "Platform spike").await;
    raise_marker(
        &server,
        Some(a),
        hour - chrono::Duration::days(3),
        "Old A spike",
    )
    .await;

    let root_who = Some((root, "alerts-root", true));
    let (status, body) = markers(&server, root_who, &[("range", "7d".into())]).await;
    assert_eq!(status, 200);
    let data = body["data"].as_array().unwrap();
    assert_eq!(data.len(), 4, "{body}");
    for m in data {
        assert!(DateTime::parse_from_rfc3339(m["hour_start"].as_str().unwrap()).is_ok());
    }
    let first = data
        .iter()
        .find(|m| m["alert_id"] == id_a.to_string())
        .unwrap();
    assert_eq!(first["scope"], "agent");
    assert_eq!(first["agent_id"], a.to_string());
    assert_eq!(first["severity"], "warning");
    assert_eq!(first["title"], "A spike");
    assert!((first["spend_usd"].as_f64().unwrap() - 12.5).abs() < 1e-9);
    assert!((first["threshold_usd"].as_f64().unwrap() - 3.0).abs() < 1e-9);

    // Window: the 3-day-old marker is outside range=24h... and so is everything
    // older than a day, so only the 2h-old ones remain.
    let (_, body) = markers(&server, root_who, &[("range", "24h".into())]).await;
    assert_eq!(body["data"].as_array().unwrap().len(), 3);

    let me = Some((viewer, "mark-a", false));
    let (status, body) = markers(&server, me, &[("range", "7d".into())]).await;
    assert_eq!(status, 200);
    let data = body["data"].as_array().unwrap();
    assert_eq!(data.len(), 2, "{body}");
    assert!(data.iter().all(|m| m["agent_id"] == a.to_string()));

    let (status, _) = markers(&server, me, &[("agent_id", b.to_string())]).await;
    assert_eq!(status, 404);

    let (status, body) = markers(&server, root_who, &[("agent_id", a.to_string())]).await;
    assert_eq!(status, 200);
    let data = body["data"].as_array().unwrap();
    assert!(!data.is_empty() && data.iter().all(|m| m["agent_id"] == a.to_string()));

    let (status, _) = markers(&server, None, &[]).await;
    assert_eq!(status, 401);
    let (status, _) = markers(&server, root_who, &[("range", "9y".into())]).await;
    assert_eq!(status, 400);
    server.cleanup().await;
}
