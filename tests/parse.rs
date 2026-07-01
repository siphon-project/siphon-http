//! Unit tests for the pure request-parsing helpers + config parse. No pyo3 /
//! Python interpreter needed — these exercise the Rust hot paths directly.

use siphon_http::parse::{extract_path_params, parse_query, urldecode};
use siphon_http::HttpConfig;

#[test]
fn path_params_single_segment() {
    let p = extract_path_params("/users/{id}/profile", "/users/42/profile");
    assert_eq!(p.get("id").map(String::as_str), Some("42"));
    assert_eq!(p.len(), 1);
}

#[test]
fn path_params_multiple() {
    let p = extract_path_params("/users/{id}/orders/{order}", "/users/7/orders/9001");
    assert_eq!(p.get("id").map(String::as_str), Some("7"));
    assert_eq!(p.get("order").map(String::as_str), Some("9001"));
}

#[test]
fn path_params_catch_all() {
    let p = extract_path_params("/static/{*rest}", "/static/css/site.css");
    assert_eq!(p.get("rest").map(String::as_str), Some("css/site.css"));
}

#[test]
fn path_params_urldecoded() {
    let p = extract_path_params("/u/{name}", "/u/a%20b");
    assert_eq!(p.get("name").map(String::as_str), Some("a b"));
}

#[test]
fn query_parsing() {
    let q = parse_query("a=1&b=hello%20world&flag");
    assert_eq!(q.get("a").map(String::as_str), Some("1"));
    assert_eq!(q.get("b").map(String::as_str), Some("hello world"));
    assert_eq!(q.get("flag").map(String::as_str), Some(""));
}

#[test]
fn query_empty_is_empty() {
    assert!(parse_query("").is_empty());
}

#[test]
fn urldecode_basics() {
    assert_eq!(urldecode("a%2Bb"), "a+b"); // %2B decodes to '+'
    assert_eq!(urldecode("a+b"), "a b"); // bare '+' is a space
    assert_eq!(urldecode("plain"), "plain");
    assert_eq!(urldecode("%28ok%29"), "(ok)");
}

#[test]
fn config_roundtrip() {
    let cfg = HttpConfig::from_yaml(
        r#"
servers:
  - listen: "0.0.0.0:8443"
    max_body_bytes: 65536
    tls:
      cert_path: "/tls/server.crt"
      key_path: "/tls/server.key"
clients:
  api:
    base_url: "https://api.example.com"
    pool_size: 16
"#,
    )
    .unwrap();
    assert_eq!(cfg.servers.len(), 1);
    assert_eq!(cfg.servers[0].listen, "0.0.0.0:8443");
    assert_eq!(cfg.servers[0].max_body_bytes, 65536);
    assert!(cfg.servers[0].tls.is_some());
    assert_eq!(
        cfg.clients["api"].base_url.as_deref(),
        Some("https://api.example.com")
    );
    assert_eq!(cfg.clients["api"].pool_size, 16);
}

#[test]
fn config_defaults_applied() {
    let cfg = HttpConfig::from_yaml("servers:\n  - listen: \"127.0.0.1:8080\"\n").unwrap();
    assert_eq!(cfg.servers[0].max_body_bytes, 1 << 20);
    assert_eq!(cfg.servers[0].request_timeout_ms, 30_000);
    assert!(cfg.servers[0].tls.is_none());
    assert!(cfg.clients.is_empty());
}
