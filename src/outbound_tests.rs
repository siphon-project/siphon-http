//! The outbound client against a real listener of this crate.
//!
//! Every test but the one about `http.Client` runs without a Python
//! interpreter and without the server task: a plain tokio test that builds a
//! client from a configuration and sends.

use std::collections::HashSet;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::{ConnectInfo, Request as AxumRequest};
use axum::http::{HeaderValue, StatusCode};
use axum::response::Response as AxumResponse;
use axum::routing::any;
use axum::Router;
use http::Method;
use http_body_util::BodyExt;
use pyo3::prelude::*;

use crate::runtime::bind_listener;
use crate::{ClientError, HttpClient, HttpConfig};

const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/tls");

/// The socket peers a server has seen: one entry per connection.
type Peers = Arc<Mutex<HashSet<SocketAddr>>>;

/// Answer every path with what the request was. `/status/<n>` answers `n`,
/// `/slow` answers after two seconds.
fn echo_router(peers: Peers) -> Router {
    Router::new().route(
        "/{*path}",
        any(
            move |ConnectInfo(peer): ConnectInfo<SocketAddr>, request: AxumRequest| {
                let peers = Arc::clone(&peers);
                async move {
                    peers.lock().unwrap().insert(peer);
                    let (parts, body) = request.into_parts();
                    if parts.uri.path() == "/slow" {
                        tokio::time::sleep(Duration::from_secs(2)).await;
                    }
                    let status = parts
                        .uri
                        .path()
                        .strip_prefix("/status/")
                        .and_then(|code| code.parse().ok())
                        .and_then(|code| StatusCode::from_u16(code).ok())
                        .unwrap_or(StatusCode::OK);
                    let echo: Vec<&str> = parts
                        .headers
                        .get_all("x-echo")
                        .iter()
                        .map(|value| value.to_str().unwrap())
                        .collect();
                    let body = body.collect().await.unwrap().to_bytes();
                    let mut response = AxumResponse::new(axum::body::Body::from(body));
                    *response.status_mut() = status;
                    let headers = response.headers_mut();
                    let mut set = |name: &'static str, value: String| {
                        headers.append(name, HeaderValue::from_str(&value).unwrap());
                    };
                    set("x-method", parts.method.to_string());
                    set("x-version", format!("{:?}", parts.version));
                    set("x-target", parts.uri.to_string());
                    set("x-echo", echo.join("|"));
                    set("via", "1.1 first".into());
                    set("via", "1.1 second".into());
                    response
                }
            },
        ),
    )
}

/// Serve the echo on `yaml`'s only listener; its address and the peers it
/// sees.
async fn serve(yaml: &str) -> (SocketAddr, Peers) {
    let config = HttpConfig::from_yaml(yaml).expect("the test config");
    let listener = bind_listener(&config.servers[0]).expect("the listener binds");
    let address = listener.local_addr().expect("the bound address");
    let peers = Peers::default();
    let router = echo_router(Arc::clone(&peers));
    tokio::spawn(async move { listener.serve(router).await.expect("the listener serves") });
    for _ in 0..200 {
        if tokio::net::TcpStream::connect(address).await.is_ok() {
            // The probe connection itself is not a request: forget it.
            tokio::time::sleep(Duration::from_millis(20)).await;
            return (address, peers);
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("the listener on {address} never accepted");
}

const PLAIN_SERVER: &str = "servers:\n  - listen: \"127.0.0.1:0\"\n";

fn mutual_server() -> String {
    format!(
        "servers:\n  - listen: \"127.0.0.1:0\"\n    tls:\n      cert_path: \"{FIXTURES}/server.pem\"\n      key_path: \"{FIXTURES}/server.key\"\n      client_ca: \"{FIXTURES}/ca.pem\"\n"
    )
}

/// A configuration with one client, `name`, with the given settings.
fn clients(name: &str, settings: &str) -> HttpConfig {
    HttpConfig::from_yaml(&format!("clients:\n  {name}:\n{settings}")).expect("the client config")
}

fn plain_client(name: &str, address: SocketAddr) -> HttpConfig {
    clients(name, &format!("    base_url: \"http://{address}\"\n"))
}

fn fields(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
    pairs
        .iter()
        .map(|(name, value)| (name.to_string(), value.to_string()))
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn requests_carry_their_method_headers_and_body_over_one_connection() {
    let (address, peers) = serve(PLAIN_SERVER).await;
    let client = HttpClient::named(&plain_client("methods", address), "methods").unwrap();

    let reply = client
        .request(Method::GET, "/things?id=1", &[], None)
        .await
        .unwrap();
    assert_eq!(reply.status, 200);
    assert_eq!(reply.header("X-Method"), Some("GET"));
    assert_eq!(reply.header("x-target"), Some("/things?id=1"));
    assert_eq!(
        reply.header("via"),
        Some("1.1 first, 1.1 second"),
        "a repeated field is joined"
    );
    assert!(reply.body.is_empty());
    let names: Vec<&str> = reply
        .headers
        .iter()
        .map(|(name, _)| name.as_str())
        .collect();
    assert_eq!(
        &names[..5],
        ["x-method", "x-version", "x-target", "x-echo", "via"],
        "names in the order received"
    );

    let headers = fields(&[("x-echo", "one"), ("X-Echo", "two")]);
    for method in [Method::PUT, Method::POST] {
        let reply = client
            .request(
                method.clone(),
                "/things/1",
                &headers,
                Some(b"payload".to_vec()),
            )
            .await
            .unwrap();
        assert_eq!(reply.status, 200);
        assert_eq!(reply.header("x-method"), Some(method.as_str()));
        assert_eq!(
            reply.header("x-echo"),
            Some("one|two"),
            "both sent in order"
        );
        assert_eq!(reply.body, b"payload");
    }

    let reply = client
        .request(Method::DELETE, "/status/204", &[], None)
        .await
        .unwrap();
    assert_eq!(reply.status, 204);
    assert_eq!(reply.header("x-method"), Some("DELETE"));

    let reply = client
        .request(Method::GET, "/status/404", &[], None)
        .await
        .unwrap();
    assert_eq!(reply.status, 404, "an error status is a reply");

    assert_eq!(peers.lock().unwrap().len(), 1, "one pooled connection");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn http_2_over_mutual_tls() {
    let (address, peers) = serve(&mutual_server()).await;
    let base = format!("    base_url: \"https://localhost:{}\"\n", address.port());
    let trust = format!("    verify: \"{FIXTURES}/ca.pem\"\n    verify_only: true\n");
    let identity =
        format!("    cert: \"{FIXTURES}/client.pem\"\n    key: \"{FIXTURES}/client.key\"\n");

    let configuration = clients("mutual", &format!("{base}{trust}{identity}"));
    let client = HttpClient::named(&configuration, "mutual").unwrap();
    for _ in 0..2 {
        let reply = client
            .request(Method::PUT, "/records/1", &[], Some(b"{}".to_vec()))
            .await
            .unwrap();
        assert_eq!(reply.status, 200);
        assert_eq!(reply.header("x-version"), Some("HTTP/2.0"));
        assert_eq!(reply.body, b"{}");
    }
    assert_eq!(peers.lock().unwrap().len(), 1, "one HTTP/2 connection");

    let configuration = clients("anonymous", &format!("{base}{trust}"));
    let anonymous = HttpClient::named(&configuration, "anonymous").unwrap();
    let error = anonymous
        .request(Method::GET, "/x", &[], None)
        .await
        .unwrap_err();
    // In TLS 1.3 the server refuses the missing certificate after the
    // client has finished its side of the handshake, so the client learns of
    // it as a connection closed under its request, not as a failed connect.
    assert!(
        matches!(
            error,
            ClientError::Connect { .. } | ClientError::Request { .. }
        ),
        "no client certificate: {error:?}"
    );

    let configuration = clients(
        "distrustful",
        &format!(
            "{base}    verify: \"{FIXTURES}/untrusted-client.pem\"\n    verify_only: true\n{identity}"
        ),
    );
    let distrustful = HttpClient::named(&configuration, "distrustful").unwrap();
    let error = distrustful
        .request(Method::GET, "/x", &[], None)
        .await
        .unwrap_err();
    assert!(
        matches!(error, ClientError::Connect { .. }),
        "an untrusted server: {error:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_name_is_one_pool_and_other_settings_for_it_conflict() {
    let (address, peers) = serve(PLAIN_SERVER).await;
    let configuration = plain_client("shared", address);
    let first = HttpClient::named(&configuration, "shared").unwrap();
    let second = HttpClient::named(&configuration.clone(), "shared").unwrap();
    for client in [&first, &second] {
        let reply = client.request(Method::GET, "/x", &[], None).await.unwrap();
        assert_eq!(reply.status, 200);
    }
    assert_eq!(peers.lock().unwrap().len(), 1, "both use one pool");

    let other = clients(
        "shared",
        &format!("    base_url: \"http://{address}\"\n    timeout_ms: 1\n"),
    );
    assert!(matches!(
        HttpClient::named(&other, "shared"),
        Err(ClientError::Conflict { name }) if name == "shared"
    ));
    assert!(matches!(
        HttpClient::named(&configuration, "missing"),
        Err(ClientError::UnknownClient { name }) if name == "missing"
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failures_are_typed_and_leave_out_the_target() {
    let (address, _) = serve(PLAIN_SERVER).await;

    let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let closed_address = closed.local_addr().unwrap();
    drop(closed);
    let refused = HttpClient::named(&plain_client("refused", closed_address), "refused").unwrap();
    let error = refused
        .request(Method::GET, "/path?token=secret", &[], None)
        .await
        .unwrap_err();
    assert!(matches!(error, ClientError::Connect { .. }), "{error:?}");
    let text = error.to_string();
    assert!(
        !text.contains("secret") && !text.contains("/path"),
        "the target is not in {text:?}"
    );

    let impatient = clients(
        "impatient",
        &format!("    base_url: \"http://{address}\"\n    timeout_ms: 200\n"),
    );
    let impatient = HttpClient::named(&impatient, "impatient").unwrap();
    let error = impatient
        .request(Method::GET, "/slow", &[], None)
        .await
        .unwrap_err();
    assert!(matches!(error, ClientError::Timeout), "{error:?}");

    let plain = HttpClient::named(&plain_client("headers", address), "headers").unwrap();
    let error = plain
        .request(Method::GET, "/x", &fields(&[("bad name", "v")]), None)
        .await
        .unwrap_err();
    assert!(matches!(error, ClientError::Header { ref name } if name == "bad name"));
    let error = plain
        .request(Method::GET, "/x", &fields(&[("x-ok", "line\nbreak")]), None)
        .await
        .unwrap_err();
    assert!(matches!(error, ClientError::Header { ref name } if name == "x-ok"));
    assert!(!error.to_string().contains("break"), "no value in the text");

    let bare = HttpClient::named(&clients("bare", "    timeout_ms: 500\n"), "bare").unwrap();
    let error = bare
        .request(Method::GET, "/relative", &[], None)
        .await
        .unwrap_err();
    assert!(matches!(error, ClientError::Target { .. }), "{error:?}");
    let reply = bare
        .request(
            Method::GET,
            &format!("http://{address}/absolute"),
            &[],
            None,
        )
        .await
        .unwrap();
    assert_eq!(reply.header("x-target"), Some("/absolute"));

    let unusable = clients("unusable", "    verify_only: true\n");
    assert!(matches!(
        HttpClient::named(&unusable, "unusable"),
        Err(ClientError::Configuration { .. })
    ));
    let unreadable = clients(
        "unreadable",
        "    cert: \"/nonexistent/c.pem\"\n    key: \"/nonexistent/c.key\"\n",
    );
    assert!(matches!(
        HttpClient::named(&unreadable, "unreadable"),
        Err(ClientError::Configuration { .. })
    ));
}

/// A caller on a short-lived runtime (a start-up step) and one on another
/// runtime later share the pool; the second is not handed a connection that
/// died with the first runtime.
#[test]
fn a_pool_outlives_the_runtime_that_opened_its_connections() {
    let server = tokio::runtime::Runtime::new().unwrap();
    let (address, _) = server.block_on(serve(PLAIN_SERVER));
    let configuration = plain_client("runtimes", address);

    for _ in 0..2 {
        let caller = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let client = HttpClient::named(&configuration, "runtimes").unwrap();
        let reply = caller
            .block_on(client.request(Method::GET, "/x", &[], None))
            .unwrap();
        assert_eq!(reply.status, 200);
        drop(caller);
    }
}

/// `http.Client("<name>")` in a script and [`HttpClient::named`] share the
/// pool of `clients.<name>`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_script_client_and_a_rust_client_of_one_name_share_the_pool() {
    let (address, peers) = serve(PLAIN_SERVER).await;
    let configuration = plain_client("script", address);
    // What the server task does with the configuration at start-up.
    crate::runtime::set_named_clients(configuration.clients.clone());

    let script_client = Python::attach(|py| {
        let namespace = crate::namespace(configuration.clone())(py).unwrap();
        let locals = pyo3::types::PyDict::new(py);
        locals.set_item("http", namespace).unwrap();
        let client = py
            .eval(c"http.Client('script')", None, Some(&locals))
            .unwrap();
        let client = client.extract::<PyRef<crate::Client>>().unwrap();
        client.outbound().clone()
    });
    let rust_client = HttpClient::named(&configuration, "script").unwrap();

    for client in [&script_client, &rust_client] {
        let reply = client.request(Method::GET, "/x", &[], None).await.unwrap();
        assert_eq!(reply.status, 200);
    }
    assert_eq!(peers.lock().unwrap().len(), 1, "one pool for both");
}
