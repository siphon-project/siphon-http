//! The configs shipped in the repository load, with the values they spell out.

use siphon_http::HttpConfig;

fn repo_file(path: &str) -> String {
    std::fs::read_to_string(format!("{}/{path}", env!("CARGO_MANIFEST_DIR")))
        .unwrap_or_else(|error| panic!("read {path}: {error}"))
}

#[test]
fn deploy_example_loads() {
    let config = HttpConfig::from_file(format!(
        "{}/deploy/http.example.yaml",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap();
    assert_eq!(config.servers.len(), 2);
    let public = &config.servers[0];
    assert_eq!(public.listen, "0.0.0.0:8443");
    let tls = public.tls.as_ref().unwrap();
    assert_eq!(tls.cert_path, "/etc/siphon/tls/server.crt");
    assert_eq!(tls.key_path, "/etc/siphon/tls/server.key");
    assert_eq!(tls.client_ca, None);
    assert_eq!(public.max_body_bytes, 65536);
    assert_eq!(public.request_timeout_ms, 5000);
    let local = &config.servers[1];
    assert_eq!(local.listen, "127.0.0.1:9090");
    assert!(local.tls.is_none());
    assert_eq!(local.max_body_bytes, 1 << 20);
    assert_eq!(local.request_timeout_ms, 30_000);
    let api = &config.clients["api"];
    assert_eq!(api.base_url.as_deref(), Some("https://api.example.com"));
    assert_eq!(api.timeout_ms, 5000);
    assert_eq!(api.verify.as_deref(), Some("/etc/siphon/tls/ca.crt"));
    assert_eq!(api.pool_size, 16);
    assert!(!api.http2_prior_knowledge);
}

#[test]
fn bench_harness_config_loads() {
    let config = HttpConfig::from_yaml(&repo_file("harness/http.yaml")).unwrap();
    assert_eq!(config.servers.len(), 1);
    assert_eq!(config.servers[0].listen, "0.0.0.0:8080");
    assert_eq!(config.servers[0].max_body_bytes, 65536);
    assert_eq!(config.servers[0].request_timeout_ms, 5000);
    assert!(config.clients.is_empty());
}

#[test]
fn kubernetes_configmap_http_yaml_loads() {
    // The ConfigMap carries http.yaml as an indented block scalar.
    let configmap = repo_file("deploy/k8s/configmap.yaml");
    let start = configmap
        .find("  http.yaml: |\n")
        .expect("configmap has an http.yaml key");
    let body: String = configmap[start..]
        .lines()
        .skip(1)
        .take_while(|line| line.is_empty() || line.starts_with("    "))
        .map(|line| format!("{}\n", line.get(4..).unwrap_or("")))
        .collect();
    let config = HttpConfig::from_yaml(&body).unwrap();
    assert_eq!(config.servers.len(), 1);
    assert_eq!(config.servers[0].listen, "0.0.0.0:8443");
    assert!(config.servers[0].tls.is_some());
    assert_eq!(config.servers[0].max_body_bytes, 65536);
    assert_eq!(config.servers[0].request_timeout_ms, 5000);
}
