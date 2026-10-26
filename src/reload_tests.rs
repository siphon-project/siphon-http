//! TLS files read again while running (`servers[].tls.reload_seconds`,
//! `clients.<name>.reload_seconds`): through a real listener and a real
//! pool, with the files replaced under them.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::routing::any;
use axum::Router;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, SignatureScheme};

use crate::runtime::bind_listener;
use crate::{HttpClient, HttpConfig};

const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/tls");

/// A directory of copies of fixtures, which a test replaces.
struct Files(PathBuf);
impl Files {
    fn new(tag: &str) -> Self {
        let directory =
            std::env::temp_dir().join(format!("siphon-http-reload-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        Self(directory)
    }
    fn path(&self, name: &str) -> String {
        self.0.join(name).display().to_string()
    }
    /// Copy fixture `from` to `to`, replacing it.
    fn put(&self, from: &str, to: &str) {
        let temporary = self.0.join(format!("{to}.new"));
        std::fs::copy(Path::new(FIXTURES).join(from), &temporary).unwrap();
        std::fs::rename(temporary, self.0.join(to)).unwrap();
    }
}
impl Drop for Files {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Accepts any server certificate and keeps the last one presented.
#[derive(Debug, Default)]
struct Recording(Mutex<Option<Vec<u8>>>);
impl ServerCertVerifier for Recording {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _: &[CertificateDer<'_>],
        _: &ServerName<'_>,
        _: &[u8],
        _: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        *self.0.lock().unwrap() = Some(end_entity.to_vec());
        Ok(ServerCertVerified::assertion())
    }
    fn verify_tls12_signature(
        &self,
        _: &[u8],
        _: &CertificateDer<'_>,
        _: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }
    fn verify_tls13_signature(
        &self,
        _: &[u8],
        _: &CertificateDer<'_>,
        _: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }
    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

/// The certificate a new connection to `address` is served with.
async fn served(address: SocketAddr) -> Vec<u8> {
    let recording = Arc::new(Recording::default());
    let config = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .dangerous()
    .with_custom_certificate_verifier(recording.clone())
    .with_no_client_auth();
    let connector = tokio_rustls::TlsConnector::from(Arc::new(config));
    let tcp = tokio::net::TcpStream::connect(address).await.unwrap();
    let _ = connector
        .connect(ServerName::try_from("localhost").unwrap(), tcp)
        .await
        .unwrap();
    let certificate = recording.0.lock().unwrap().clone();
    certificate.unwrap()
}

fn der(file: &str) -> Vec<u8> {
    CertificateDer::pem_file_iter(format!("{FIXTURES}/{file}"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .to_vec()
}

async fn listen(yaml: &str) -> SocketAddr {
    let config = HttpConfig::from_yaml(yaml).unwrap();
    let listener = bind_listener(&config.servers[0]).unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        listener
            .serve(Router::new().route("/{*path}", any(|| async { "ok" })))
            .await
            .unwrap()
    });
    for _ in 0..200 {
        if tokio::net::TcpStream::connect(address).await.is_ok() {
            return address;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("never listened");
}

/// A listener with `reload_seconds` serves new connections with the
/// certificate that replaced the old one, and keeps the old one when the
/// new files do not load; without it the files are read once.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_listener_serves_the_certificate_that_replaced_its_own() {
    let files = Files::new("listener");
    files.put("server.pem", "cert.pem");
    files.put("server.key", "key.pem");
    let yaml = |reload: &str| {
        format!(
            "servers:\n  - listen: \"127.0.0.1:0\"\n    tls:\n      cert_path: \"{}\"\n      key_path: \"{}\"\n{reload}",
            files.path("cert.pem"),
            files.path("key.pem")
        )
    };
    let reloading = listen(&yaml("      reload_seconds: 1\n")).await;
    let fixed = listen(&yaml("")).await;
    assert_eq!(served(reloading).await, der("server.pem"));
    // Another certificate and its key, as a renewal leaves them.
    files.put("client.pem", "cert.pem");
    files.put("client.key", "key.pem");
    let mut now = Vec::new();
    for _ in 0..40 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        now = served(reloading).await;
        if now != der("server.pem") {
            break;
        }
    }
    assert_eq!(now, der("client.pem"));
    assert_eq!(served(fixed).await, der("server.pem"));
    // A file that does not load: the last good one stays.
    std::fs::write(files.path("cert.pem"), "not a certificate").unwrap();
    tokio::time::sleep(Duration::from_millis(2500)).await;
    assert_eq!(served(reloading).await, der("client.pem"));
}

/// A pool with `reload_seconds` presents the client certificate that
/// replaced its own: a listener that trusts only the first refuses it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pool_presents_the_certificate_that_replaced_its_own() {
    let address = listen(&format!(
        "servers:\n  - listen: \"127.0.0.1:0\"\n    tls:\n      cert_path: \"{FIXTURES}/server.pem\"\n      key_path: \"{FIXTURES}/server.key\"\n      client_ca: \"{FIXTURES}/ca.pem\"\n"
    ))
    .await;
    let files = Files::new("pool");
    files.put("client.pem", "cert.pem");
    files.put("client.key", "key.pem");
    let config = HttpConfig::from_yaml(&format!(
        "clients:\n  reload-pool:\n    base_url: \"https://localhost:{}\"\n    verify: \"{FIXTURES}/ca.pem\"\n    verify_only: true\n    cert: \"{}\"\n    key: \"{}\"\n    reload_seconds: 1\n    timeout_ms: 2000\n",
        address.port(),
        files.path("cert.pem"),
        files.path("key.pem")
    ))
    .unwrap();
    let client = HttpClient::named(&config, "reload-pool").unwrap();
    let get = || client.request(http::Method::GET, "/x", &[], None);
    assert_eq!(get().await.unwrap().status, 200);
    files.put("untrusted-client.pem", "cert.pem");
    files.put("untrusted-client.key", "key.pem");
    // Not before the period is over.
    assert_eq!(get().await.unwrap().status, 200);
    tokio::time::sleep(Duration::from_millis(1100)).await;
    assert!(
        get().await.is_err(),
        "the replaced certificate is presented and refused"
    );
    files.put("client.pem", "cert.pem");
    files.put("client.key", "key.pem");
    tokio::time::sleep(Duration::from_millis(1100)).await;
    assert_eq!(get().await.unwrap().status, 200);
}
