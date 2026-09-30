//! Authorization regression tests for user management (and `PUT /api/settings`).
//!
//! Bug: a plain member could `POST /api/users {"is_superuser":true,"role":"admin"}`
//! and mint a superuser, because OSS `AuthService::can_manage_users` defaults to
//! allow-all and the handlers themselves checked nothing. The same allow-all gate
//! guarded `PUT /api/settings`.
//!
//!   cargo test -p nasiko-server --test users_authz -- --test-threads=1

mod common;

use reqwest::Response;
use serde_json::{Value, json};
use serial_test::serial;
use uuid::Uuid;

const STRONG_PASSWORD: &str = "Correct-Horse-Battery-9";

// ─── helpers ────────────────────────────────────────────────────────────────

async fn init_admin(server: &common::TestServer) -> String {
    server
        .client
        .post(server.url("/api/auth/initialize-admin"))
        .json(&json!({"username": "admin", "email": "admin@test.local"}))
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap()["user_id"]
        .as_str()
        .unwrap()
        .to_string()
}

/// Create a user as the bootstrap superuser and return its id.
async fn create_user(
    server: &common::TestServer,
    superuser_id: &str,
    username: &str,
    role: Option<&str>,
) -> String {
    let mut body = json!({"username": username, "email": format!("{username}@test.local")});
    if let Some(r) = role {
        body["role"] = json!(r);
    }
    let res = common::as_superuser(
        server.client.post(server.url("/api/users")),
        superuser_id,
        "admin",
    )
    .json(&body)
    .send()
    .await
    .unwrap();
    assert_eq!(res.status(), 201, "seeding {username} failed");
    res.json::<Value>().await.unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string()
}

async fn assert_forbidden(res: Response, expected_code: &str) {
    let status = res.status();
    let text = res.text().await.unwrap();
    assert_eq!(status, 403, "expected 403, body: {text}");
    let body: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
    assert_eq!(body["code"], expected_code, "body: {text}");
}

async fn user_count(server: &common::TestServer, username: &str) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM users WHERE username = $1")
        .bind(username)
        .fetch_one(&server.db)
        .await
        .unwrap()
}

async fn access_key(server: &common::TestServer, id: &str) -> Option<String> {
    sqlx::query_scalar("SELECT access_key FROM user_credentials WHERE user_id = $1")
        .bind(Uuid::parse_str(id).unwrap())
        .fetch_optional(&server.db)
        .await
        .unwrap()
}

// ─── member is denied ───────────────────────────────────────────────────────

#[tokio::test]
#[serial]
async fn member_cannot_create_user() {
    let server = common::TestServer::start().await;
    let root = init_admin(&server).await;
    let alice = create_user(&server, &root, "alice", None).await;

    let res = common::as_member(
        server.client.post(server.url("/api/users")),
        &alice,
        "alice",
    )
    .json(&json!({"username": "mallory", "email": "mallory@test.local"}))
    .send()
    .await
    .unwrap();
    assert_forbidden(res, "admin_required").await;
    assert_eq!(user_count(&server, "mallory").await, 0);

    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn member_cannot_create_superuser() {
    let server = common::TestServer::start().await;
    let root = init_admin(&server).await;
    let alice = create_user(&server, &root, "alice", None).await;

    let res = common::as_member(
        server.client.post(server.url("/api/users")),
        &alice,
        "alice",
    )
    .json(&json!({
        "username": "mallory",
        "email": "mallory@test.local",
        "role": "admin",
        "is_superuser": true,
    }))
    .send()
    .await
    .unwrap();
    assert_forbidden(res, "admin_required").await;
    assert_eq!(user_count(&server, "mallory").await, 0);

    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn member_cannot_mutate_other_users() {
    let server = common::TestServer::start().await;
    let root = init_admin(&server).await;
    let alice = create_user(&server, &root, "alice", None).await;
    let bob = create_user(&server, &root, "bob", None).await;
    let bob_key_before = access_key(&server, &bob).await;

    let c = &server.client;
    let calls = vec![
        common::as_member(
            c.put(server.url(&format!("/api/users/{bob}/role"))),
            &alice,
            "alice",
        )
        .json(&json!({"role": "admin"})),
        common::as_member(
            c.put(server.url(&format!("/api/users/{bob}"))),
            &alice,
            "alice",
        )
        .json(&json!({"display_name": "x"})),
        common::as_member(
            c.put(server.url(&format!("/api/users/{bob}"))),
            &alice,
            "alice",
        )
        .json(&json!({"password": STRONG_PASSWORD})),
        common::as_member(
            c.delete(server.url(&format!("/api/users/{bob}"))),
            &alice,
            "alice",
        ),
        common::as_member(
            c.post(server.url(&format!("/api/users/{bob}/deactivate"))),
            &alice,
            "alice",
        ),
        common::as_member(
            c.post(server.url(&format!("/api/users/{bob}/reinstate"))),
            &alice,
            "alice",
        ),
        common::as_member(
            c.post(server.url(&format!("/api/users/{bob}/regenerate-credentials"))),
            &alice,
            "alice",
        ),
    ];
    for call in calls {
        assert_forbidden(call.send().await.unwrap(), "admin_required").await;
    }

    let (role, active, deleted, name): (String, bool, bool, Option<String>) = sqlx::query_as(
        "SELECT role::text, is_active, deleted_at IS NOT NULL, display_name FROM users WHERE id = $1",
    )
    .bind(Uuid::parse_str(&bob).unwrap())
    .fetch_one(&server.db)
    .await
    .unwrap();
    assert_eq!(role, "member");
    assert!(active);
    assert!(!deleted);
    assert_ne!(name.as_deref(), Some("x"));
    assert_eq!(access_key(&server, &bob).await, bob_key_before);

    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn member_cannot_escalate_self() {
    let server = common::TestServer::start().await;
    let root = init_admin(&server).await;
    let alice = create_user(&server, &root, "alice", None).await;
    let c = &server.client;

    for body in [
        json!({"role": "admin"}),
        json!({"is_active": false}),
        json!({"password": STRONG_PASSWORD}),
    ] {
        let res = common::as_member(
            c.put(server.url(&format!("/api/users/{alice}"))),
            &alice,
            "alice",
        )
        .json(&body)
        .send()
        .await
        .unwrap();
        assert_forbidden(res, "admin_required").await;
    }
    let res = common::as_member(
        c.put(server.url(&format!("/api/users/{alice}/role"))),
        &alice,
        "alice",
    )
    .json(&json!({"role": "admin"}))
    .send()
    .await
    .unwrap();
    assert_eq!(res.status(), 403);

    let role: String = sqlx::query_scalar("SELECT role::text FROM users WHERE id = $1")
        .bind(Uuid::parse_str(&alice).unwrap())
        .fetch_one(&server.db)
        .await
        .unwrap();
    assert_eq!(role, "member");

    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn member_keeps_self_service() {
    let server = common::TestServer::start().await;
    let root = init_admin(&server).await;
    let alice = create_user(&server, &root, "alice", None).await;
    let c = &server.client;

    for path in [
        "/api/users/me".to_string(),
        "/api/users/me/accessible-agents".to_string(),
        format!("/api/users/{alice}"),
    ] {
        let res = common::as_member(c.get(server.url(&path)), &alice, "alice")
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 200, "GET {path}");
    }
    let res = common::as_member(
        c.put(server.url(&format!("/api/users/{alice}"))),
        &alice,
        "alice",
    )
    .json(&json!({"display_name": "Alice A"}))
    .send()
    .await
    .unwrap();
    assert_eq!(res.status(), 200);
    let name: Option<String> = sqlx::query_scalar("SELECT display_name FROM users WHERE id = $1")
        .bind(Uuid::parse_str(&alice).unwrap())
        .fetch_one(&server.db)
        .await
        .unwrap();
    assert_eq!(name.as_deref(), Some("Alice A"));

    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn member_cannot_read_other_users_privileged() {
    let server = common::TestServer::start().await;
    let root = init_admin(&server).await;
    let alice = create_user(&server, &root, "alice", None).await;
    let bob = create_user(&server, &root, "bob", None).await;
    let c = &server.client;

    for path in [
        "/api/users".to_string(),
        "/api/users/admins".to_string(),
        format!("/api/users/{bob}"),
        format!("/api/users/{bob}/accessible-agents"),
    ] {
        let res = common::as_member(c.get(server.url(&path)), &alice, "alice")
            .send()
            .await
            .unwrap();
        assert_forbidden(res, "admin_required").await;
    }

    server.cleanup().await;
}

// ─── non-superuser admin ────────────────────────────────────────────────────

#[tokio::test]
#[serial]
async fn admin_non_superuser_can_manage_members() {
    let server = common::TestServer::start().await;
    let root = init_admin(&server).await;
    let carol = create_user(&server, &root, "carol", Some("admin")).await;
    let bob = create_user(&server, &root, "bob", None).await;
    let c = &server.client;

    for body in [
        json!({"username": "u1", "email": "u1@test.local"}),
        json!({"username": "u2", "email": "u2@test.local", "role": "admin"}),
    ] {
        let res = common::as_member(c.post(server.url("/api/users")), &carol, "carol")
            .json(&body)
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 201);
    }
    let res = common::as_member(
        c.put(server.url(&format!("/api/users/{bob}/role"))),
        &carol,
        "carol",
    )
    .json(&json!({"role": "team_member"}))
    .send()
    .await
    .unwrap();
    assert_eq!(res.status(), 204);
    let res = common::as_member(
        c.post(server.url(&format!("/api/users/{bob}/deactivate"))),
        &carol,
        "carol",
    )
    .send()
    .await
    .unwrap();
    assert!(res.status().is_success(), "deactivate: {}", res.status());

    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn admin_non_superuser_cannot_mint_or_touch_superuser() {
    let server = common::TestServer::start().await;
    let root = init_admin(&server).await;
    let carol = create_user(&server, &root, "carol", Some("admin")).await;
    let c = &server.client;
    let key_before = access_key(&server, &root).await;

    let res = common::as_member(c.post(server.url("/api/users")), &carol, "carol")
        .json(&json!({"username": "evil", "email": "evil@test.local", "is_superuser": true}))
        .send()
        .await
        .unwrap();
    assert_forbidden(res, "superuser_required").await;
    assert_eq!(user_count(&server, "evil").await, 0);

    let calls = vec![
        common::as_member(
            c.post(server.url(&format!("/api/users/{root}/regenerate-credentials"))),
            &carol,
            "carol",
        ),
        common::as_member(
            c.put(server.url(&format!("/api/users/{root}"))),
            &carol,
            "carol",
        )
        .json(&json!({"password": STRONG_PASSWORD})),
        common::as_member(
            c.delete(server.url(&format!("/api/users/{root}"))),
            &carol,
            "carol",
        ),
        common::as_member(
            c.post(server.url(&format!("/api/users/{root}/deactivate"))),
            &carol,
            "carol",
        ),
        common::as_member(
            c.put(server.url(&format!("/api/users/{root}/role"))),
            &carol,
            "carol",
        )
        .json(&json!({"role": "member"})),
    ];
    for call in calls {
        assert_forbidden(call.send().await.unwrap(), "superuser_required").await;
    }
    assert_eq!(access_key(&server, &root).await, key_before);
    let (active, role): (bool, String) =
        sqlx::query_as("SELECT is_active, role::text FROM users WHERE id = $1")
            .bind(Uuid::parse_str(&root).unwrap())
            .fetch_one(&server.db)
            .await
            .unwrap();
    assert!(active);
    assert_eq!(role, "admin");

    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn inactive_admin_is_not_admin() {
    let server = common::TestServer::start().await;
    let root = init_admin(&server).await;
    let dave = create_user(&server, &root, "dave", Some("admin")).await;
    sqlx::query("UPDATE users SET is_active = false WHERE id = $1")
        .bind(Uuid::parse_str(&dave).unwrap())
        .execute(&server.db)
        .await
        .unwrap();

    let res = common::as_member(server.client.post(server.url("/api/users")), &dave, "dave")
        .json(&json!({"username": "x", "email": "x@test.local"}))
        .send()
        .await
        .unwrap();
    assert!(
        res.status() == 401 || res.status() == 403,
        "got {}",
        res.status()
    );
    assert_eq!(user_count(&server, "x").await, 0);

    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn superuser_can_still_create_superuser() {
    let server = common::TestServer::start().await;
    let root = init_admin(&server).await;

    let res = common::as_superuser(
        server.client.post(server.url("/api/users")),
        &root,
        "admin",
    )
    .json(&json!({"username": "su2", "email": "su2@test.local", "role": "admin", "is_superuser": true}))
    .send()
    .await
    .unwrap();
    assert_eq!(res.status(), 201);

    server.cleanup().await;
}

// ─── PUT /api/settings ──────────────────────────────────────────────────────

#[tokio::test]
#[serial]
async fn member_cannot_update_settings() {
    let server = common::TestServer::start().await;
    let root = init_admin(&server).await;
    let alice = create_user(&server, &root, "alice", None).await;

    let res = common::as_member(
        server.client.put(server.url("/api/settings")),
        &alice,
        "alice",
    )
    .json(&json!({"router_model": "member-chosen-model"}))
    .send()
    .await
    .unwrap();
    assert_forbidden(res, "admin_required").await;
    let stored: Option<String> =
        sqlx::query_scalar("SELECT router_model FROM settings WHERE id = 1")
            .fetch_optional(&server.db)
            .await
            .unwrap()
            .flatten();
    assert_ne!(stored.as_deref(), Some("member-chosen-model"));

    // Reads stay open to any authenticated user (no secrets in the payload).
    let res = common::as_member(
        server.client.get(server.url("/api/settings")),
        &alice,
        "alice",
    )
    .send()
    .await
    .unwrap();
    assert_eq!(res.status(), 200);

    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn admin_can_update_settings() {
    let server = common::TestServer::start().await;
    let root = init_admin(&server).await;
    let carol = create_user(&server, &root, "carol", Some("admin")).await;

    for (id, name, is_su) in [(&root, "admin", true), (&carol, "carol", false)] {
        let rb = server.client.put(server.url("/api/settings"));
        let rb = if is_su {
            common::as_superuser(rb, id, name)
        } else {
            common::as_member(rb, id, name)
        };
        let res = rb
            .json(&json!({"router_model": "admin-model"}))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 200, "{name}");
    }

    server.cleanup().await;
}
