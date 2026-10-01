//! Destination policy for notification channels (SSRF guard).
//!
//! Channel URLs are admin-supplied but the server fetches them from inside the
//! deployment network, so every URL is checked three times: when it is stored,
//! right before each send (DNS can change after creation), and at connect time
//! by [`NotifyResolver`]. IP-literal hosts never reach a DNS resolver, so they
//! are rejected by the literal check instead. Redirects are never followed: a
//! public receiver must not be able to bounce the request to an internal one.
//!
//! The private-address definition is shared with the MCP gateway
//! (`nasiko_mcp_gateway::net::is_blocked_ip`). `allow_private` is a
//! config-only dev/test switch (`ALERTS_ALLOW_PRIVATE_URLS`); it relaxes the
//! https requirement, the address checks and the Slack host pin together.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use nasiko_mcp_gateway::net::is_blocked_ip;
use reqwest::dns::{Addrs, Name, Resolve, Resolving};

/// Longest accepted channel URL.
const MAX_URL_LEN: usize = 2048;
/// Whole-request timeout on the notification client (connect + send + read).
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// Bound on the pre-send DNS lookup so a stalled resolver cannot hold a claim.
const RESOLVE_TIMEOUT: Duration = Duration::from_secs(5);
/// The only host a Slack channel may post to (outside dev/test).
const SLACK_HOST: &str = "hooks.slack.com";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChannelKind {
    Webhook,
    Slack,
}

impl ChannelKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ChannelKind::Webhook => "webhook",
            ChannelKind::Slack => "slack",
        }
    }

    pub fn parse(s: &str) -> Option<ChannelKind> {
        match s {
            "webhook" => Some(ChannelKind::Webhook),
            "slack" => Some(ChannelKind::Slack),
            _ => None,
        }
    }
}

/// Why a channel URL was rejected. Variants carry no URL text on purpose: the
/// slug is what reaches API responses and `last_error`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UrlError {
    InvalidUrl,
    SchemeNotAllowed,
    UserinfoNotAllowed,
    HostNotAllowed,
    BlockedAddress,
    ResolveFailed,
    UrlTooLong,
}

impl UrlError {
    pub fn slug(&self) -> &'static str {
        match self {
            UrlError::InvalidUrl => "invalid_url",
            UrlError::SchemeNotAllowed => "scheme_not_allowed",
            UrlError::UserinfoNotAllowed => "userinfo_not_allowed",
            UrlError::HostNotAllowed => "host_not_allowed",
            UrlError::BlockedAddress => "blocked_address",
            UrlError::ResolveFailed => "resolve_failed",
            UrlError::UrlTooLong => "url_too_long",
        }
    }
}

/// The host of `url` as an IP when it is a literal. `host_str` brackets IPv6
/// and the URL parser has already normalised exotic IPv4 spellings
/// (`0x7f.1`, `2130706433`) to dotted form.
fn literal_ip(url: &reqwest::Url) -> Option<IpAddr> {
    let host = url.host_str()?;
    host.trim_start_matches('[')
        .trim_end_matches(']')
        .parse()
        .ok()
}

/// Validate a channel URL without touching the network.
pub fn validate_channel_url(
    raw: &str,
    kind: ChannelKind,
    allow_private: bool,
) -> Result<reqwest::Url, UrlError> {
    if raw.len() > MAX_URL_LEN {
        return Err(UrlError::UrlTooLong);
    }
    let url = reqwest::Url::parse(raw).map_err(|_| UrlError::InvalidUrl)?;
    match url.scheme() {
        "https" => {}
        "http" if allow_private => {}
        _ => return Err(UrlError::SchemeNotAllowed),
    }
    // Rejected even in dev: `https://hooks.slack.com@evil/` is the classic
    // host-confusion trick, and credentials in URLs end up in logs.
    if !url.username().is_empty() || url.password().is_some() {
        return Err(UrlError::UserinfoNotAllowed);
    }
    let host = url.host_str().ok_or(UrlError::InvalidUrl)?.to_owned();
    if let Some(ip) = literal_ip(&url) {
        if !allow_private && is_blocked_ip(ip) {
            return Err(UrlError::BlockedAddress);
        }
    } else {
        let host = host.trim_end_matches('.').to_ascii_lowercase();
        if !allow_private && (host == "localhost" || host.ends_with(".localhost")) {
            return Err(UrlError::HostNotAllowed);
        }
        if kind == ChannelKind::Slack && !allow_private && host != SLACK_HOST {
            return Err(UrlError::HostNotAllowed);
        }
    }
    if kind == ChannelKind::Slack && !allow_private && literal_ip(&url).is_some() {
        return Err(UrlError::HostNotAllowed);
    }
    Ok(url)
}

/// Resolve the URL's host and reject it when any address is non-public. Run
/// right before every send: a hostname that was public at creation can be
/// re-pointed at an internal address later.
pub async fn check_resolves_public(
    url: &reqwest::Url,
    allow_private: bool,
) -> Result<(), UrlError> {
    if allow_private {
        return Ok(());
    }
    if let Some(ip) = literal_ip(url) {
        return if is_blocked_ip(ip) {
            Err(UrlError::BlockedAddress)
        } else {
            Ok(())
        };
    }
    let host = url.host_str().ok_or(UrlError::InvalidUrl)?;
    let port = url.port_or_known_default().unwrap_or(443);
    let lookup = tokio::time::timeout(RESOLVE_TIMEOUT, tokio::net::lookup_host((host, port)))
        .await
        .map_err(|_| UrlError::ResolveFailed)?
        .map_err(|_| UrlError::ResolveFailed)?;
    let addrs: Vec<SocketAddr> = lookup.collect();
    if addrs.is_empty() {
        return Err(UrlError::ResolveFailed);
    }
    if addrs.iter().any(|a| is_blocked_ip(a.ip())) {
        return Err(UrlError::BlockedAddress);
    }
    Ok(())
}

/// DNS resolver for the notification client: reqwest connects to exactly the
/// addresses returned here, so the check and the connection share one
/// resolution and there is no rebinding window. Unlike the MCP gateway's
/// resolver it reads its own flag, not an environment variable.
#[derive(Debug, Clone, Copy)]
pub struct NotifyResolver {
    pub allow_private: bool,
}

impl Resolve for NotifyResolver {
    fn resolve(&self, name: Name) -> Resolving {
        let allow_private = self.allow_private;
        Box::pin(async move {
            let host = name.as_str().to_owned();
            let addrs = tokio::net::lookup_host((host.as_str(), 0)).await?;
            let allowed: Vec<SocketAddr> = addrs
                .filter(|sa| allow_private || !is_blocked_ip(sa.ip()))
                .collect();
            if allowed.is_empty() {
                return Err("host did not resolve to an allowed address".into());
            }
            let iter: Addrs = Box::new(allowed.into_iter());
            Ok(iter)
        })
    }
}

/// The client every notification goes through. No redirects, a hard timeout
/// and the guarded resolver. Deliberately does not call `.no_proxy()`:
/// operators may need an egress proxy to reach Slack.
pub fn guarded_client(allow_private: bool) -> reqwest::Client {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(REQUEST_TIMEOUT)
        .dns_resolver(Arc::new(NotifyResolver { allow_private }))
        .build()
        .expect("static client config")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check(raw: &str, kind: ChannelKind) -> Result<(), UrlError> {
        validate_channel_url(raw, kind, false).map(|_| ())
    }

    #[test]
    fn rejects_unsafe_urls_with_slugs() {
        let long = format!("https://example.com/{}", "a".repeat(2100));
        let cases: [(&str, UrlError); 14] = [
            ("http://example.com", UrlError::SchemeNotAllowed),
            ("ftp://x", UrlError::SchemeNotAllowed),
            ("https://127.0.0.1", UrlError::BlockedAddress),
            ("https://0.0.0.0", UrlError::BlockedAddress),
            ("https://10.0.0.1", UrlError::BlockedAddress),
            ("https://169.254.169.254", UrlError::BlockedAddress),
            ("https://[::1]", UrlError::BlockedAddress),
            ("https://[fe80::1]", UrlError::BlockedAddress),
            ("https://[fc00::1]", UrlError::BlockedAddress),
            ("https://[::ffff:127.0.0.1]", UrlError::BlockedAddress),
            ("https://localhost", UrlError::HostNotAllowed),
            ("https://a.localhost", UrlError::HostNotAllowed),
            ("https://user:pw@example.com", UrlError::UserinfoNotAllowed),
            (
                "https://hooks.slack.com@evil.example/",
                UrlError::UserinfoNotAllowed,
            ),
        ];
        for (raw, want) in cases {
            assert_eq!(check(raw, ChannelKind::Webhook), Err(want), "{raw}");
        }
        assert_eq!(
            check(&long, ChannelKind::Webhook),
            Err(UrlError::UrlTooLong)
        );
        assert_eq!(
            check("not a url", ChannelKind::Webhook),
            Err(UrlError::InvalidUrl)
        );
        // Encoded-decimal and hex IPv4 forms normalise to a blocked literal.
        assert_eq!(
            check("https://2130706433/", ChannelKind::Webhook),
            Err(UrlError::BlockedAddress)
        );
        assert_eq!(
            check("https://localhost./", ChannelKind::Webhook),
            Err(UrlError::HostNotAllowed)
        );
    }

    #[test]
    fn accepts_public_urls_and_pins_slack() {
        assert!(check("https://example.com/hook", ChannelKind::Webhook).is_ok());
        assert!(check("https://hooks.slack.com/services/T/B/X", ChannelKind::Slack).is_ok());
        assert_eq!(
            check("https://example.com", ChannelKind::Slack),
            Err(UrlError::HostNotAllowed)
        );
    }

    #[test]
    fn allow_private_relaxes_scheme_address_and_slack_pin() {
        for kind in [ChannelKind::Webhook, ChannelKind::Slack] {
            assert!(validate_channel_url("http://127.0.0.1:1234/x", kind, true).is_ok());
        }
        // Userinfo stays rejected even in dev.
        assert_eq!(
            validate_channel_url("http://u:p@127.0.0.1/x", ChannelKind::Webhook, true).map(|_| ()),
            Err(UrlError::UserinfoNotAllowed)
        );
    }

    #[tokio::test]
    async fn resolve_check_rejects_literals_and_localhost() {
        let blocked = reqwest::Url::parse("https://127.0.0.1/x").unwrap();
        assert_eq!(
            check_resolves_public(&blocked, false).await,
            Err(UrlError::BlockedAddress)
        );
        let public = reqwest::Url::parse("https://93.184.216.34/x").unwrap();
        assert_eq!(check_resolves_public(&public, false).await, Ok(()));
        assert_eq!(check_resolves_public(&blocked, true).await, Ok(()));
        let names = reqwest::Url::parse("https://localhost/x").unwrap();
        assert_eq!(
            check_resolves_public(&names, false).await,
            Err(UrlError::BlockedAddress)
        );
    }
}
