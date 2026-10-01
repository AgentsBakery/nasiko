//! Notification channels: where alerts are delivered (signed webhooks and
//! Slack), how they are routed, and the durable outbox that carries them.
//!
//! `ssrf` decides which destinations are allowed, `payload` renders and signs
//! bodies, `dispatch` is the outbox worker and `channels` is the admin API.

use std::time::Duration;

use axum::Router;
use axum::extract::{Request, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};

use crate::auth::Claims;
use crate::rate_limit::RateLimiter;
use crate::state::AppState;

pub mod channels;
pub mod dispatch;
pub mod payload;
pub mod ssrf;

/// Test sends allowed per caller per window. Each one is an outbound HTTP
/// request to an admin-chosen host, so the cap bounds port-scan and
/// amplification use even by an admin.
pub const TEST_SENDS_PER_WINDOW: u32 = 10;
pub const TEST_WINDOW: Duration = Duration::from_secs(60);

/// Per-caller limit for the test endpoint, answering 429 with `Retry-After`.
async fn limit_test_sends(
    State(limiter): State<RateLimiter>,
    claims: Claims,
    req: Request,
    next: Next,
) -> Response {
    if limiter.allow(&claims.sub) {
        return next.run(req).await;
    }
    let mut resp = (
        StatusCode::TOO_MANY_REQUESTS,
        axum::Json(serde_json::json!({
            "error": "too many test sends, try again shortly",
            "code": "rate_limited",
        })),
    )
        .into_response();
    resp.headers_mut().insert(
        header::RETRY_AFTER,
        HeaderValue::from(TEST_WINDOW.as_secs()),
    );
    resp
}

/// Admin notification routes. Role checks live in the handlers; the caller
/// layers `require_user_manager` on top.
pub fn admin_router(test_limiter: RateLimiter) -> Router<AppState> {
    Router::new()
        .route(
            "/notification-channels",
            get(channels::list_channels).post(channels::create_channel),
        )
        .route(
            "/notification-channels/{id}",
            get(channels::get_channel)
                .put(channels::update_channel)
                .delete(channels::delete_channel),
        )
        .route(
            "/notification-channels/{id}/routes",
            get(channels::get_routes).put(channels::put_routes),
        )
        .route(
            "/notification-channels/{id}/test",
            post(channels::test_channel).layer(middleware::from_fn_with_state(
                test_limiter,
                limit_test_sends,
            )),
        )
        .route("/notification-deliveries", get(channels::list_deliveries))
}
