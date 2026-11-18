//! Address pinning (`resolve`) and the redirect limit (`redirects`) of the
//! outbound client, against a real TLS socket.

use std::net::SocketAddr;
use std::sync::Arc;

use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::client::Client;
use crate::HttpConfig;

const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/tls");
/// A name nothing resolves: RFC 2606 reserves `.example.com` and the label is
/// not delegated.
const PINNED_NAME: &str = "host.example.com";

/// What the TLS server saw of one connection.
#[derive(Debug)]
struct Seen {
    sni: Option<String>,
    request: String,
}

/// Accept one TLS connection with the `host.example.com` certificate, answer
/// 200 and report the SNI and the request head.
async fn serve_pinned_name() -> (SocketAddr, tokio::sync::oneshot::Receiver<Seen>) {
    use rustls::pki_types::pem::PemObject;
    let certificates =
        rustls::pki_types::CertificateDer::pem_file_iter(format!("{FIXTURES}/pinned.pem"))
            .expect("the pinned certificate")
            .collect::<Result<Vec<_>, _>>()
            .expect("the pinned certificate parses");
    let key = rustls::pki_types::PrivateKeyDer::from_pem_file(format!("{FIXTURES}/pinned.key"))
        .expect("the pinned key");
    let config = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .expect("protocol versions")
    .with_no_client_auth()
    .with_single_cert(certificates, key)
    .expect("the pinned identity");
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let address = listener.local_addr().expect("address");
    let (report, seen) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("accept");
        let mut stream = acceptor.accept(stream).await.expect("the handshake");
        let sni = stream.get_ref().1.server_name().map(str::to_string);
        let mut buffer = vec![0u8; 4096];
        let length = stream.read(&mut buffer).await.expect("the request");
        let request = String::from_utf8_lossy(&buffer[..length]).into_owned();
        stream
            .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\nok")
            .await
            .expect("the reply");
        let _ = stream.shutdown().await;
        let _ = report.send(Seen { sni, request });
    });
    (address, seen)
}

fn pinned_yaml(address: SocketAddr, extra: &str) -> String {
    format!(
        "clients:\n  p:\n    verify: \"{FIXTURES}/pinned.pem\"\n    verify_only: true\n    resolve:\n      {PINNED_NAME}: [\"{address}\"]\n{extra}"
    )
}

fn pool(yaml: &str) -> reqwest::Client {
    let config = HttpConfig::from_yaml(yaml).expect("the test config");
    Client::pool(&config.clients["p"]).expect("the pool builds")
}

/// The request goes to the pinned address, TLS names the host in the SNI and
/// verifies the certificate against that name, and the Host header is the
/// host.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pinned_host_is_reached_at_its_address_with_its_own_name_in_tls() {
    let (address, seen) = serve_pinned_name().await;
    let client = pool(&pinned_yaml(address, ""));
    let response = client
        .get(format!("https://{PINNED_NAME}:{}/hook", address.port()))
        .send()
        .await
        .expect("the pinned request succeeds");
    assert_eq!(response.status().as_u16(), 200);
    let seen = seen.await.expect("the server saw the connection");
    assert_eq!(seen.sni.as_deref(), Some(PINNED_NAME));
    assert!(
        seen.request
            .to_ascii_lowercase()
            .contains(&format!("host: {PINNED_NAME}:{}", address.port())),
        "{}",
        seen.request
    );
}

/// The certificate is checked against the host name, not the address: pin the
/// host to a server whose certificate is for another name and the request
/// fails.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pinned_host_is_verified_by_name_not_by_address() {
    let (address, _seen) = serve_pinned_name().await;
    let yaml = format!(
        "clients:\n  p:\n    verify: \"{FIXTURES}/pinned.pem\"\n    verify_only: true\n    resolve:\n      other.example.com: [\"{address}\"]\n"
    );
    let result = pool(&yaml)
        .get(format!("https://other.example.com:{}/", address.port()))
        .send()
        .await;
    assert!(result.is_err(), "a certificate for another name is refused");
}

/// Without the pin the synthetic name does not resolve.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn without_a_pin_the_synthetic_name_does_not_resolve() {
    let (address, _seen) = serve_pinned_name().await;
    let yaml =
        format!("clients:\n  p:\n    verify: \"{FIXTURES}/pinned.pem\"\n    verify_only: true\n");
    let error = pool(&yaml)
        .get(format!("https://{PINNED_NAME}:{}/", address.port()))
        .send()
        .await
        .expect_err("the name has no address");
    assert!(error.is_connect(), "{error}");
}

/// Several addresses for one host are all handed to the connector.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_host_may_be_pinned_to_several_addresses() {
    let (address, seen) = serve_pinned_name().await;
    // The first address refuses; the connector falls through to the second.
    let yaml = format!(
        "clients:\n  p:\n    verify: \"{FIXTURES}/pinned.pem\"\n    verify_only: true\n    resolve:\n      {PINNED_NAME}: [\"127.0.0.1:1\", \"{address}\"]\n"
    );
    let response = pool(&yaml)
        .get(format!("https://{PINNED_NAME}:{}/", address.port()))
        .send()
        .await
        .expect("the second address answers");
    assert_eq!(response.status().as_u16(), 200);
    assert!(seen.await.is_ok());
}

#[test]
fn a_bad_resolve_entry_is_a_configuration_error() {
    for (entry, what) in [
        ("[\"not-an-address\"]", "not a socket address"),
        ("[\"192.0.2.10\"]", "no port"),
        ("[]", "no address"),
    ] {
        let yaml = format!("clients:\n  p:\n    resolve:\n      host.example.com: {entry}\n");
        let config = HttpConfig::from_yaml(&yaml).expect("the yaml parses");
        let error = Client::pool(&config.clients["p"]).expect_err(what);
        assert!(
            error.to_string().contains("host.example.com"),
            "{what}: {error}"
        );
    }
    let config =
        HttpConfig::from_yaml("clients:\n  p:\n    resolve:\n      \"\": [\"192.0.2.10:443\"]\n")
            .expect("the yaml parses");
    assert!(Client::pool(&config.clients["p"]).is_err(), "an empty host");
}

#[test]
fn redirects_take_a_bool_or_a_hop_limit() {
    use crate::config::Redirects;
    let config = HttpConfig::from_yaml(
        "clients:\n  none: {}\n  off:\n    redirects: false\n  zero:\n    redirects: 0\n  three:\n    redirects: 3\n  on:\n    redirects: true\n",
    )
    .expect("the test config");
    assert_eq!(config.clients["none"].redirects, None);
    assert_eq!(
        config.clients["off"].redirects,
        Some(Redirects::Follow(false))
    );
    assert_eq!(config.clients["zero"].redirects, Some(Redirects::Limit(0)));
    assert_eq!(config.clients["three"].redirects, Some(Redirects::Limit(3)));
    assert_eq!(
        config.clients["on"].redirects,
        Some(Redirects::Follow(true))
    );
    HttpConfig::from_yaml("clients:\n  p:\n    redirects: sometimes\n").unwrap_err();
}

async fn redirecting_server() -> SocketAddr {
    use axum::routing::any;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let address = listener.local_addr().expect("address");
    let router = axum::Router::new()
        .route(
            "/a",
            any(|| async {
                axum::response::Response::builder()
                    .status(302)
                    .header("location", "/b")
                    .body(axum::body::Body::empty())
                    .expect("response")
            }),
        )
        .route(
            "/b",
            any(|| async {
                axum::response::Response::builder()
                    .status(302)
                    .header("location", "/c")
                    .body(axum::body::Body::empty())
                    .expect("response")
            }),
        )
        .route("/c", any(|| async { "end" }));
    tokio::spawn(async move { axum::serve(listener, router).await.expect("serves") });
    address
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn redirects_false_returns_the_3xx_and_a_limit_bounds_the_hops() {
    let address = redirecting_server().await;
    let status = |redirects: &'static str| async move {
        let client = pool(&format!("clients:\n  p:\n    redirects: {redirects}\n"));
        client
            .get(format!("http://{address}/a"))
            .send()
            .await
            .map(|response| response.status().as_u16())
    };
    assert_eq!(status("false").await.expect("answers"), 302);
    assert_eq!(status("0").await.expect("answers"), 302);
    assert_eq!(status("2").await.expect("answers"), 200);
    assert!(
        status("1").await.is_err(),
        "two hops are one more than the limit"
    );
    assert_eq!(status("true").await.expect("answers"), 200);
}

/// `redirects` wins over the older `follow_redirects` when both are set.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn redirects_take_precedence_over_follow_redirects() {
    let address = redirecting_server().await;
    let client = pool("clients:\n  p:\n    follow_redirects: false\n    redirects: 5\n");
    let response = client
        .get(format!("http://{address}/a"))
        .send()
        .await
        .expect("answers");
    assert_eq!(response.status().as_u16(), 200);
}
