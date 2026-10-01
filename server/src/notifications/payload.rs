//! Notification bodies and signing.
//!
//! The outbox stores a snapshot of the alert (`{"event", "alert": {..}}`);
//! these functions turn it into the exact bytes that are signed and sent.
//! Signing and sending must use the same bytes, so callers serialize once.
//!
//! Webhook signature (documented for receivers in docs/ALERTS.md):
//! `X-Nasiko-Signature: sha256=hex(HMAC-SHA256(secret, "{timestamp}.{raw_body}"))`
//! with the unix-seconds timestamp sent in `X-Nasiko-Timestamp`. Covering the
//! timestamp lets receivers reject replays outside a tolerance window.

use chrono::{DateTime, SecondsFormat, Utc};
use hmac::{Hmac, Mac};
use serde_json::{Value, json};
use sha2::Sha256;

/// Header names, shared with the dispatcher and the docs.
pub const HEADER_EVENT: &str = "X-Nasiko-Event";
pub const HEADER_DELIVERY: &str = "X-Nasiko-Delivery";
pub const HEADER_TIMESTAMP: &str = "X-Nasiko-Timestamp";
pub const HEADER_SIGNATURE: &str = "X-Nasiko-Signature";

/// Path characters of a channel URL revealed by [`url_hint`].
const HINT_TAIL_CHARS: usize = 4;
/// Paths shorter than this reveal nothing: a short secret path would be mostly exposed.
const HINT_MIN_PATH_CHARS: usize = 8;

/// HMAC-SHA256 as lowercase hex.
pub fn hmac_sha256_hex(key: &[u8], msg: &[u8]) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("HMAC accepts keys of any length");
    mac.update(msg);
    hex::encode(mac.finalize().into_bytes())
}

/// `sha256=<hex>` over `"{timestamp}.{body}"`.
pub fn sign(secret: &[u8], timestamp: i64, body: &[u8]) -> String {
    let mut msg = format!("{timestamp}.").into_bytes();
    msg.extend_from_slice(body);
    format!("sha256={}", hmac_sha256_hex(secret, &msg))
}

/// The webhook JSON body: event, the alert snapshot and the send time.
pub fn webhook_body(snapshot: &Value, sent_at: DateTime<Utc>) -> Vec<u8> {
    let body = json!({
        "event": snapshot.get("event").cloned().unwrap_or(Value::Null),
        "alert": snapshot.get("alert").cloned().unwrap_or(Value::Null),
        "sent_at": sent_at.to_rfc3339_opts(SecondsFormat::Secs, true),
    });
    serde_json::to_vec(&body).expect("serializing a json Value cannot fail")
}

/// Slack mrkdwn treats `&`, `<` and `>` as control characters; escape every
/// interpolated field so alert text (budget names are admin-chosen) cannot
/// inject links or mentions.
fn slack_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Strip the characters that would break out of a Slack `<url|label>` link.
fn slack_url_part(s: &str) -> String {
    s.chars()
        .filter(|c| !matches!(c, '<' | '>' | '|' | '\n' | '\r' | ' '))
        .collect()
}

fn slack_label(event: &str, severity: &str) -> &'static str {
    if event == "resolved" {
        return ":white_check_mark: RESOLVED";
    }
    match severity {
        "critical" => ":rotating_light: CRITICAL",
        "warning" => ":warning: WARNING",
        _ => ":information_source: INFO",
    }
}

/// The Slack incoming-webhook body: fallback `text` plus `blocks`.
pub fn slack_body(snapshot: &Value, public_base_url: &str) -> Vec<u8> {
    let alert = snapshot.get("alert").unwrap_or(&Value::Null);
    let field = |name: &str| alert.get(name).and_then(Value::as_str).unwrap_or("");
    let event = snapshot.get("event").and_then(Value::as_str).unwrap_or("");
    let label = slack_label(event, field("severity"));
    let title = slack_escape(field("title"));
    let message = slack_escape(field("message"));
    let link = field("link");
    let link_line = if public_base_url.is_empty() {
        format!("View in Nasiko: {}", slack_escape(link))
    } else {
        format!(
            "<{}{}|Open in Nasiko>",
            slack_url_part(public_base_url),
            slack_url_part(link)
        )
    };
    let body = json!({
        "text": format!("{label}: {title}"),
        "blocks": [
            {"type": "section", "text": {"type": "mrkdwn", "text": format!("*{label}*\n{title}")}},
            {"type": "section", "text": {"type": "mrkdwn", "text": message}},
            {"type": "context", "elements": [{"type": "mrkdwn", "text": link_line}]},
        ],
    });
    serde_json::to_vec(&body).expect("serializing a json Value cannot fail")
}

/// A non-secret description of a channel URL for the API: the host plus the
/// last few characters of the path, so an admin can tell channels apart
/// without the (often secret) path being recoverable.
pub fn url_hint(url: &reqwest::Url) -> String {
    let host = url.host_str().unwrap_or("");
    let path = url.path().trim_matches('/');
    let chars: Vec<char> = path.chars().collect();
    if chars.len() < HINT_MIN_PATH_CHARS {
        return host.to_owned();
    }
    let tail: String = chars[chars.len() - HINT_TAIL_CHARS..].iter().collect();
    format!("{host}/…/{tail}")
}

/// Snapshot used by the channel test endpoint.
pub fn test_snapshot(now: DateTime<Utc>) -> Value {
    let ts = now.to_rfc3339_opts(SecondsFormat::Micros, true);
    json!({
        "event": "test",
        "alert": {
            "id": null, "kind": "test", "severity": "info", "scope": "platform",
            "scope_ref": null, "title": "Nasiko test notification",
            "message": "This is a test notification from Nasiko.",
            "link": "/alerts", "first_seen_at": ts, "last_seen_at": ts,
            "occurrences": 1, "status": "open",
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(event: &str, severity: &str, title: &str) -> Value {
        json!({"event": event, "alert": {
            "id": "a", "kind": "budget_soft", "severity": severity, "scope": "platform",
            "title": title, "message": "m & <n>", "link": "/budgets",
        }})
    }

    #[test]
    fn hmac_matches_rfc4231_case_2() {
        assert_eq!(
            hmac_sha256_hex(b"Jefe", b"what do ya want for nothing?"),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
    }

    #[test]
    fn sign_covers_timestamp_and_body() {
        let want = format!("sha256={}", hmac_sha256_hex(b"k", b"1700000000.{\"a\":1}"));
        assert_eq!(sign(b"k", 1_700_000_000, b"{\"a\":1}"), want);
        assert_ne!(sign(b"k", 1_700_000_001, b"{\"a\":1}"), want);
    }

    #[test]
    fn webhook_body_has_event_alert_sent_at() {
        let raw = webhook_body(&snapshot("opened", "warning", "t"), Utc::now());
        let v: Value = serde_json::from_slice(&raw).unwrap();
        assert_eq!(v["event"], "opened");
        assert_eq!(v["alert"]["title"], "t");
        assert!(v["sent_at"].is_string());
    }

    #[test]
    fn slack_body_escapes_and_links() {
        let raw = slack_body(
            &snapshot("opened", "critical", "a<b>&c"),
            "https://n.example",
        );
        let v: Value = serde_json::from_slice(&raw).unwrap();
        let text = v["text"].as_str().unwrap();
        assert!(
            text.contains("CRITICAL") && text.contains("a&lt;b&gt;&amp;c"),
            "{text}"
        );
        assert!(v["blocks"].is_array());
        assert!(
            raw.windows(31)
                .any(|w| w == b"<https://n.example/budgets|Open")
        );

        let plain = slack_body(&snapshot("opened", "info", "t"), "");
        let v: Value = serde_json::from_slice(&plain).unwrap();
        assert!(
            v["blocks"][2]["elements"][0]["text"]
                .as_str()
                .unwrap()
                .contains("View in Nasiko: /budgets")
        );
        assert!(String::from_utf8(plain).unwrap().contains("INFO"));
    }

    #[test]
    fn slack_resolved_event_is_labelled() {
        let raw = slack_body(&snapshot("resolved", "critical", "t"), "");
        let v: Value = serde_json::from_slice(&raw).unwrap();
        assert!(v["text"].as_str().unwrap().contains("RESOLVED"));
        let warn = slack_body(&snapshot("opened", "warning", "t"), "");
        let v: Value = serde_json::from_slice(&warn).unwrap();
        assert!(v["text"].as_str().unwrap().contains("WARNING"));
    }

    #[test]
    fn url_hint_reveals_host_and_tail_only() {
        let u = reqwest::Url::parse("https://hooks.slack.com/services/T0/B0/AbCdEfGh").unwrap();
        assert_eq!(url_hint(&u), "hooks.slack.com/…/EfGh");
        let short = reqwest::Url::parse("https://example.com/hook").unwrap();
        assert_eq!(url_hint(&short), "example.com");
    }
}
