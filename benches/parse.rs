//! Per-request Rust work siphon-http adds on top of axum/hyper: path-param
//! extraction, query parsing, percent-decoding, and config parse (boot /
//! hot-reload). The live runtime is socket- and Python-bound; these cover the
//! pure-Rust hot paths.

use criterion::{black_box, criterion_group, criterion_main, Criterion};
use siphon_http::parse::{extract_path_params, parse_query, urldecode};
use siphon_http::HttpConfig;

const CONFIG: &str = r#"
servers:
  - listen: "0.0.0.0:8443"
    tls:
      cert_path: "/etc/siphon/tls/server.crt"
      key_path: "/etc/siphon/tls/server.key"
    max_body_bytes: 65536
    request_timeout_ms: 5000
  - listen: "127.0.0.1:9090"
clients:
  api:
    base_url: "https://api.example.com"
    timeout_ms: 5000
    verify: "/etc/siphon/tls/ca.crt"
    pool_size: 16
"#;

fn bench(c: &mut Criterion) {
    c.bench_function("extract_path_params", |b| {
        b.iter(|| {
            extract_path_params(
                black_box("/users/{id}/orders/{order}"),
                black_box("/users/42/orders/9001"),
            )
        })
    });

    c.bench_function("parse_query", |b| {
        b.iter(|| parse_query(black_box("page=2&limit=50&q=hello%20world&sort=-created")))
    });

    c.bench_function("urldecode", |b| {
        b.iter(|| urldecode(black_box("hello%20world%21+%28ok%29")))
    });

    c.bench_function("http.yaml parse", |b| {
        b.iter(|| HttpConfig::from_yaml(black_box(CONFIG)).unwrap())
    });
}

criterion_group!(benches, bench);
criterion_main!(benches);
