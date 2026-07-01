//! Transport-level proof for the HTTP/2 wiring.
//!
//! A cleartext listener, bound the way the addon binds its own listeners
//! (`axum_server` over a std `TcpListener`), must accept HTTP/2 with prior
//! knowledge (h2c) *and* still serve HTTP/1.1 to plain clients on the same
//! socket. The outbound client must be able to open a connection directly as
//! HTTP/2. This pins the Rust transport stack + its cargo features; no Python
//! or script glue is involved.

use std::net::{SocketAddr, TcpListener};
use std::time::Duration;

use axum::{routing::any, Router};

/// Bind a cleartext listener and serve a trivial router on it, mirroring the
/// runtime's cleartext bind path. Returns the bound address.
async fn spawn_cleartext() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let addr = listener.local_addr().unwrap();

    let app = Router::new().route("/echo", any(|| async { "ok" }));
    tokio::spawn(async move {
        axum_server::from_tcp(listener)
            .unwrap()
            .serve(app.into_make_service())
            .await
            .unwrap();
    });
    // The socket is already listening (backlog accepts connects immediately),
    // but give the accept loop a beat to come up.
    tokio::time::sleep(Duration::from_millis(50)).await;
    addr
}

/// A client configured for prior-knowledge HTTP/2 reaches a cleartext peer as
/// HTTP/2, with no upgrade dance.
#[tokio::test]
async fn h2c_prior_knowledge_end_to_end() {
    let addr = spawn_cleartext().await;

    let client = reqwest::Client::builder()
        .http2_prior_knowledge()
        .build()
        .unwrap();

    let resp = client
        .post(format!("http://{addr}/echo"))
        .body("ping")
        .send()
        .await
        .expect("h2c request failed");

    assert_eq!(
        resp.version(),
        reqwest::Version::HTTP_2,
        "cleartext listener did not serve HTTP/2 (h2c)"
    );
    assert!(resp.status().is_success());
    assert_eq!(resp.text().await.unwrap(), "ok");
}

/// The same listener still serves HTTP/1.1 to a default client — h1 and h2
/// coexist on one cleartext socket (auto-detected via the connection preface).
#[tokio::test]
async fn plain_http1_still_works_on_same_listener() {
    let addr = spawn_cleartext().await;

    let client = reqwest::Client::builder().build().unwrap();
    let resp = client
        .get(format!("http://{addr}/echo"))
        .send()
        .await
        .expect("http/1.1 request failed");

    assert_eq!(resp.version(), reqwest::Version::HTTP_11);
    assert!(resp.status().is_success());
}
