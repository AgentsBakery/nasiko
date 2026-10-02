//! Per-destination cache of the telemetry features a Nasiko server supports.
//!
//! Why a cache rather than a probe at report time:
//!
//! - The Stop/report hook runs inside a 9 s budget and must work offline, so it
//!   never touches the network. It reads this file; `install` primes it and
//!   every `sync` refreshes it.
//! - A new event shape is sent only when the destination advertised it. A
//!   missing, stale, or unreadable cache therefore means "no features" and the
//!   CLI keeps sending the v1.0 shapes every server accepts.
//! - The Codex exclusive-input fix and its `source.adapter_version` marker are
//!   gated on the same feature, so a receipt without the marker always carries
//!   inclusive Codex input and the server's legacy correction stays valid.
//!
//! Entries are keyed by normalized cluster URL plus principal so capabilities
//! learned for one cluster or account never shape events bound for another.

use anyhow::Result;
use chrono::{DateTime, Utc};
use nasiko_types::{CODING_AGENT_EVENT_VERSION, CodingAgentCapabilities};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::queue::QueueDestination;
use super::state::{self, InstallationBinding};
use crate::config::Config;

const CACHE_FILE: &str = "capabilities.json";
const PROBE_TIMEOUT: Duration = Duration::from_secs(10);

/// Features one destination server accepts. Unknown slugs are kept but inert:
/// only slugs this build checks via [`Capabilities::supports`] change behaviour.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Capabilities {
    features: BTreeSet<String>,
}

impl Capabilities {
    /// The v1.0 baseline: an old server, or nothing known yet.
    pub fn none() -> Self {
        Self::default()
    }

    #[cfg_attr(not(test), allow(dead_code))] // read by the report hook (next commit)
    pub fn supports(&self, feature: &str) -> bool {
        self.features.contains(feature)
    }

    /// A server validating a different event version gets the v1.0 baseline:
    /// its feature slugs may not mean what this build thinks they mean.
    pub fn from_server(capabilities: &CodingAgentCapabilities) -> Self {
        if capabilities.event_version != CODING_AGENT_EVENT_VERSION {
            return Self::none();
        }
        Self {
            features: capabilities.features.iter().cloned().collect(),
        }
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct CacheFile {
    #[serde(default)]
    destinations: BTreeMap<String, CacheEntry>,
}

#[derive(Debug, Serialize, Deserialize)]
struct CacheEntry {
    features: Vec<String>,
    fetched_at: DateTime<Utc>,
}

/// Result of one probe, so callers can tell the user why nothing was cached.
#[derive(Debug, PartialEq, Eq)]
pub enum Probe {
    /// The cache now reflects the server (an empty set for an old server).
    Updated,
    /// The probe failed; the previous cache entry, if any, is unchanged.
    Unavailable(String),
}

#[allow(dead_code)] // read by the report hook (next commit)
pub fn load_cached(destination: &QueueDestination) -> Capabilities {
    load_cached_at(&state::integrations_dir(), destination)
}

/// Whether any probe result (even "no features") is cached for the destination.
#[allow(dead_code)] // read by the report hook (next commit)
pub fn has_cache(destination: &QueueDestination) -> bool {
    has_cache_at(&state::integrations_dir(), destination)
}

/// Prime the cache right after install so the first Stop already uses the
/// server's features. Never fails install: an unreachable server leaves no
/// cache and the first report spawns a background sync that fills it.
pub fn prime_after_install(binding: &InstallationBinding) -> Result<()> {
    let config = crate::config::load()?;
    prime_after_install_at(&state::integrations_dir(), &config, binding)
}

#[cfg_attr(not(test), allow(dead_code))] // read by the report hook (next commit)
pub(super) fn load_cached_at(dir: &Path, destination: &QueueDestination) -> Capabilities {
    read_cache(dir)
        .destinations
        .get(&cache_key(destination))
        .map(|entry| Capabilities {
            features: entry.features.iter().cloned().collect(),
        })
        .unwrap_or_default()
}

#[cfg_attr(not(test), allow(dead_code))] // read by the report hook (next commit)
pub(super) fn has_cache_at(dir: &Path, destination: &QueueDestination) -> bool {
    read_cache(dir)
        .destinations
        .contains_key(&cache_key(destination))
}

/// Probe the server and update the cache in `dir`. Probe failures keep the
/// previous entry and return `Ok(Probe::Unavailable)`; only a cache write
/// failure errs.
pub(super) fn refresh_at(
    dir: &Path,
    client: &crate::api::Client,
    destination: &QueueDestination,
) -> Result<Probe> {
    let capabilities = match client.get_coding_agent_capabilities() {
        Ok(Some(capabilities)) => Capabilities::from_server(&capabilities),
        Ok(None) => Capabilities::none(),
        Err(error) => return Ok(Probe::Unavailable(format!("{error:#}"))),
    };
    let mut cache = read_cache(dir);
    cache.destinations.insert(
        cache_key(destination),
        CacheEntry {
            features: capabilities.features.into_iter().collect(),
            fetched_at: Utc::now(),
        },
    );
    state::atomic_write(
        &cache_path(dir),
        serde_json::to_string_pretty(&cache)?.as_bytes(),
    )?;
    Ok(Probe::Updated)
}

fn prime_after_install_at(
    dir: &Path,
    config: &Config,
    binding: &InstallationBinding,
) -> Result<()> {
    let destination = QueueDestination {
        cluster_name: binding.cluster_name.clone(),
        cluster_url: binding.cluster_url.clone(),
        principal_id: binding.principal_id,
    };
    let cluster = match super::sync::validate_destination(config, &destination) {
        Ok(cluster) => cluster,
        Err(error) => {
            eprintln!(
                "note: telemetry capabilities not checked yet ({error}); the next sync will check them."
            );
            return Ok(());
        }
    };
    let client = crate::api::Client::from_cluster_entry_with_timeout(cluster, Some(PROBE_TIMEOUT));
    if let Probe::Unavailable(error) = refresh_at(dir, &client, &destination)? {
        eprintln!(
            "note: telemetry capabilities not checked yet ({error}); the next sync will check them."
        );
    }
    Ok(())
}

fn cache_key(destination: &QueueDestination) -> String {
    format!(
        "{}|{}",
        super::sync::normalize_url(&destination.cluster_url),
        destination.principal_id
    )
}

fn cache_path(dir: &Path) -> PathBuf {
    dir.join(CACHE_FILE)
}

/// Unreadable or corrupt caches read as empty: the safe answer is "no features".
fn read_cache(dir: &Path) -> CacheFile {
    std::fs::read_to_string(cache_path(dir))
        .ok()
        .and_then(|content| serde_json::from_str(&content).ok())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ClusterEntry, Config};
    use std::collections::HashMap;
    use uuid::Uuid;

    fn destination(url: &str, principal_id: Uuid) -> QueueDestination {
        QueueDestination {
            cluster_name: "bound".into(),
            cluster_url: url.into(),
            principal_id,
        }
    }

    fn token(subject: Uuid) -> String {
        use base64::Engine as _;
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(
            serde_json::json!({"sub": subject.to_string(), "exp": 4_102_444_800_i64}).to_string(),
        );
        format!("header.{payload}.signature")
    }

    fn mock_capabilities(server: &mut mockito::Server, status: usize) -> mockito::Mock {
        server
            .mock("GET", "/api/telemetry/coding-agent/capabilities")
            .with_status(status)
            .with_header("content-type", "application/json")
            .with_body(
                r#"{"data":{"event_version":1,"features":["agent_scope","adapter_version"]}}"#,
            )
            .create()
    }

    #[test]
    fn missing_cache_supports_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let destination = destination("https://a.example", Uuid::nil());

        let capabilities = load_cached_at(dir.path(), &destination);
        assert!(!capabilities.supports(nasiko_types::CODING_AGENT_FEATURE_ADAPTER_VERSION));
        assert!(!capabilities.supports(nasiko_types::CODING_AGENT_FEATURE_AGENT_SCOPE));
        assert!(!has_cache_at(dir.path(), &destination));
    }

    #[test]
    fn refresh_stores_features_per_url_and_principal() {
        let dir = tempfile::tempdir().unwrap();
        let mut server = mockito::Server::new();
        let probe = mock_capabilities(&mut server, 200);
        let principal = Uuid::new_v4();
        let client = crate::api::Client::for_test(&server.url(), Some("t"));

        refresh_at(dir.path(), &client, &destination(&server.url(), principal)).unwrap();
        probe.assert();

        let with_slash = destination(&format!("{}/", server.url()), principal);
        let cached = load_cached_at(dir.path(), &with_slash);
        assert!(cached.supports(nasiko_types::CODING_AGENT_FEATURE_ADAPTER_VERSION));
        assert!(cached.supports(nasiko_types::CODING_AGENT_FEATURE_AGENT_SCOPE));
        assert!(has_cache_at(dir.path(), &with_slash));

        let other_principal = destination(&server.url(), Uuid::new_v4());
        assert!(
            !load_cached_at(dir.path(), &other_principal)
                .supports(nasiko_types::CODING_AGENT_FEATURE_ADAPTER_VERSION)
        );
        assert!(!has_cache_at(dir.path(), &other_principal));
    }

    #[test]
    fn old_server_404_or_405_caches_an_empty_feature_set() {
        for status in [404, 405] {
            let dir = tempfile::tempdir().unwrap();
            let mut server = mockito::Server::new();
            let _probe = server
                .mock("GET", "/api/telemetry/coding-agent/capabilities")
                .with_status(status)
                .with_body(r#"{"data":null,"status_code":404,"message":"not found"}"#)
                .create();
            let client = crate::api::Client::for_test(&server.url(), Some("t"));
            let destination = destination(&server.url(), Uuid::nil());

            refresh_at(dir.path(), &client, &destination).unwrap();
            assert!(has_cache_at(dir.path(), &destination), "status {status}");
            assert!(
                !load_cached_at(dir.path(), &destination)
                    .supports(nasiko_types::CODING_AGENT_FEATURE_ADAPTER_VERSION)
            );
        }
    }

    #[test]
    fn server_error_or_unreachable_keeps_previous_cache() {
        let dir = tempfile::tempdir().unwrap();
        let mut server = mockito::Server::new();
        let destination = destination(&server.url(), Uuid::nil());
        let client = crate::api::Client::for_test(&server.url(), Some("t"));
        let ok = mock_capabilities(&mut server, 200);
        refresh_at(dir.path(), &client, &destination).unwrap();
        ok.remove();

        let _failing = server
            .mock("GET", "/api/telemetry/coding-agent/capabilities")
            .with_status(500)
            .with_body(r#"{"error":"boom","code":"internal"}"#)
            .create();
        refresh_at(dir.path(), &client, &destination).unwrap();
        assert!(
            load_cached_at(dir.path(), &destination)
                .supports(nasiko_types::CODING_AGENT_FEATURE_ADAPTER_VERSION)
        );

        let unreachable = crate::api::Client::for_test("http://127.0.0.1:9", Some("t"));
        refresh_at(dir.path(), &unreachable, &destination).unwrap();
        assert!(
            load_cached_at(dir.path(), &destination)
                .supports(nasiko_types::CODING_AGENT_FEATURE_ADAPTER_VERSION)
        );
    }

    #[test]
    fn unknown_features_are_inert_and_other_event_versions_mean_none() {
        let known = Capabilities::from_server(&CodingAgentCapabilities {
            event_version: nasiko_types::CODING_AGENT_EVENT_VERSION,
            features: vec!["future_feature".into()],
        });
        assert!(!known.supports(nasiko_types::CODING_AGENT_FEATURE_ADAPTER_VERSION));
        assert!(!known.supports(nasiko_types::CODING_AGENT_FEATURE_AGENT_SCOPE));

        let other_version = Capabilities::from_server(&CodingAgentCapabilities {
            event_version: nasiko_types::CODING_AGENT_EVENT_VERSION + 1,
            features: vec![nasiko_types::CODING_AGENT_FEATURE_ADAPTER_VERSION.into()],
        });
        assert!(!other_version.supports(nasiko_types::CODING_AGENT_FEATURE_ADAPTER_VERSION));
    }

    #[test]
    fn corrupt_cache_degrades_to_no_features() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(cache_path(dir.path()), "{not json").unwrap();
        let destination = destination("https://a.example", Uuid::nil());

        assert!(
            !load_cached_at(dir.path(), &destination)
                .supports(nasiko_types::CODING_AGENT_FEATURE_ADAPTER_VERSION)
        );
        assert!(!has_cache_at(dir.path(), &destination));
    }

    fn bound_config(url: &str, principal: Uuid) -> Config {
        Config {
            active: None,
            clusters: HashMap::from([(
                "bound".into(),
                ClusterEntry {
                    url: url.into(),
                    username: None,
                    token: Some(token(principal)),
                },
            )]),
            registry_url: None,
        }
    }

    #[test]
    fn install_primes_capabilities() {
        let dir = tempfile::tempdir().unwrap();
        let mut server = mockito::Server::new();
        let probe = mock_capabilities(&mut server, 200);
        let principal = Uuid::new_v4();
        let binding = InstallationBinding {
            cluster_name: "bound".into(),
            cluster_url: server.url(),
            principal_id: principal,
        };

        prime_after_install_at(
            dir.path(),
            &bound_config(&server.url(), principal),
            &binding,
        )
        .unwrap();
        probe.assert();
        assert!(
            load_cached_at(dir.path(), &destination(&server.url(), principal))
                .supports(nasiko_types::CODING_AGENT_FEATURE_ADAPTER_VERSION)
        );
    }

    #[test]
    fn install_with_unreachable_server_writes_no_cache() {
        let dir = tempfile::tempdir().unwrap();
        let url = "http://127.0.0.1:9";
        let principal = Uuid::new_v4();
        let binding = InstallationBinding {
            cluster_name: "bound".into(),
            cluster_url: url.into(),
            principal_id: principal,
        };

        prime_after_install_at(dir.path(), &bound_config(url, principal), &binding).unwrap();
        assert!(!cache_path(dir.path()).exists());
        assert!(!has_cache_at(dir.path(), &destination(url, principal)));
    }
}
