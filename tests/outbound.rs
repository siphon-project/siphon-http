//! The outbound client from another crate: a configuration, a named pool,
//! an HTTP/2 request. No Python interpreter, no server task.

use std::net::TcpListener;

use axum::{routing::any, Router};
use siphon_http::{HttpClient, HttpConfig};

#[tokio::test]
async fn a_named_pool_sends_from_rust() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    let app = Router::new().route(
        "/{*path}",
        any(|request: axum::extract::Request| async move { format!("{:?}", request.version()) }),
    );
    tokio::spawn(async move {
        axum_server::from_tcp(listener)
            .unwrap()
            .serve(app.into_make_service())
            .await
            .unwrap();
    });

    let configuration = HttpConfig::from_yaml(&format!(
        "clients:\n  peer:\n    base_url: \"http://{address}\"\n    http2_prior_knowledge: true\n"
    ))
    .unwrap();
    let client = HttpClient::named(&configuration, "peer").unwrap();
    let reply = client
        .request(http::Method::GET, "/version", &[], None)
        .await
        .unwrap();
    assert_eq!(reply.status, 200);
    assert_eq!(reply.body, b"HTTP/2.0");
}
