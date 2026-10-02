//! Per-destination cache of the telemetry features a Nasiko server supports.

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ClusterEntry, Config};
    use std::collections::HashMap;

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
