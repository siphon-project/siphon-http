//! What a handler can know about the connection a request arrived on: the
//! listener, and on a TLS listener the session.
//!
//! Each listener serves through an acceptor of its own, which attaches a
//! [`ConnectionInfo`] to every request of a connection as a request
//! extension, next to axum's `ConnectInfo`. The dispatcher turns it into
//! `req.listener` and `req.tls`.

use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;

use axum::middleware::AddExtension;
use axum::Extension;
use axum_server::accept::Accept;
use axum_server::tls_rustls::RustlsAcceptor;
use pyo3::prelude::*;
use pyo3::types::PyBytes;
use rustls::crypto::hash::{Hash, HashAlgorithm};
use rustls::SupportedCipherSuite;
use tokio::net::TcpStream;
use tokio_rustls::server::TlsStream;
use tower::Layer;

// ── The listener ─────────────────────────────────────────────────────────

/// A `servers[]` entry as requests see it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ListenerIdentity {
    /// `servers[].name`, or the `listen` value of an entry without one.
    pub(crate) name: String,
    pub(crate) tls: bool,
}

/// Attached to every request: where it came in, and how.
#[derive(Debug, Clone)]
pub(crate) struct ConnectionInfo {
    pub(crate) listener: Arc<ListenerIdentity>,
    /// The local address of this connection. On a wildcard listener it is the
    /// address the client reached, not `0.0.0.0`.
    pub(crate) local_addr: Option<SocketAddr>,
    pub(crate) tls: Option<Arc<TlsSession>>,
}

/// `req.listener`: the listener a request arrived on.
#[pyclass(module = "siphon", name = "Listener", frozen)]
pub struct Listener {
    info: ConnectionInfo,
}

#[pymethods]
impl Listener {
    /// The `name` of the `servers[]` entry, or its `listen` value when it has
    /// no name. This is what `@http.route(..., listeners=[...])` matches.
    #[getter]
    fn name(&self) -> &str {
        &self.info.listener.name
    }

    /// The local address of the connection as `ip:port`. On a listener bound
    /// to a wildcard address this is the address the client connected to.
    #[getter]
    fn address(&self) -> Option<String> {
        self.info.local_addr.map(|address| address.to_string())
    }

    /// The local port of the connection.
    #[getter]
    fn port(&self) -> Option<u16> {
        self.info.local_addr.map(|address| address.port())
    }

    /// Whether the listener terminates TLS.
    #[getter]
    fn tls(&self) -> bool {
        self.info.listener.tls
    }

    fn __repr__(&self) -> String {
        format!(
            "Listener(name={:?}, address={:?}, tls={})",
            self.info.listener.name,
            self.address().unwrap_or_default(),
            self.info.listener.tls
        )
    }
}

impl Listener {
    pub(crate) fn new(info: ConnectionInfo) -> Self {
        Self { info }
    }
}

// ── The TLS session ──────────────────────────────────────────────────────

/// One certificate of the chain the client presented.
#[derive(Debug, Clone)]
pub(crate) struct PeerCertificateFacts {
    der: Vec<u8>,
    fingerprint_sha256: String,
    names: Option<crate::x509::Names>,
}

/// The facts of one TLS session, read once when its handshake completes.
#[derive(Debug, Clone)]
pub(crate) struct TlsSession {
    version: String,
    cipher_suite: String,
    cipher_suite_id: u16,
    server_name: Option<String>,
    alpn: Option<String>,
    peer_certificates: Vec<Arc<PeerCertificateFacts>>,
}

/// The name the IANA TLS Cipher Suite Registry gives a suite. rustls spells
/// the TLS 1.3 ones `TLS13_AES_128_GCM_SHA256`; the registry has
/// `TLS_AES_128_GCM_SHA256`. The TLS 1.2 names are the registry's already.
fn iana_cipher_suite_name(suite: rustls::CipherSuite) -> String {
    match suite.as_str() {
        Some(name) => match name.strip_prefix("TLS13_") {
            Some(rest) => format!("TLS_{rest}"),
            None => name.to_string(),
        },
        None => format!("0x{:04X}", u16::from(suite)),
    }
}

/// `TLSv1.3`, `TLSv1.2`: the spelling OpenSSL and most logs use.
fn protocol_version_name(version: rustls::ProtocolVersion) -> String {
    match version {
        rustls::ProtocolVersion::TLSv1_3 => "TLSv1.3".to_string(),
        rustls::ProtocolVersion::TLSv1_2 => "TLSv1.2".to_string(),
        other => format!("{other:?}"),
    }
}

/// SHA-256 as the linked rustls crypto provider implements it, so the
/// fingerprint of a certificate needs no hashing crate of its own.
#[derive(Clone, Copy)]
pub(crate) struct Sha256(&'static dyn Hash);

impl Sha256 {
    /// The SHA-256 of the provider's cipher suites, or an error when the
    /// provider has none, which fails the listener at startup.
    pub(crate) fn from_provider(provider: &rustls::crypto::CryptoProvider) -> io::Result<Self> {
        provider
            .cipher_suites
            .iter()
            .map(|suite| match suite {
                SupportedCipherSuite::Tls13(suite) => suite.common.hash_provider,
                SupportedCipherSuite::Tls12(suite) => suite.common.hash_provider,
            })
            .find(|hash| hash.algorithm() == HashAlgorithm::SHA256)
            .map(Self)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::Unsupported,
                    "the TLS crypto provider has no SHA-256, needed for certificate fingerprints",
                )
            })
    }

    fn hex(&self, data: &[u8]) -> String {
        self.0
            .hash(data)
            .as_ref()
            .iter()
            .map(|octet| format!("{octet:02x}"))
            .collect()
    }
}

impl TlsSession {
    /// Read the session off a connection whose handshake has completed.
    fn from_connection(connection: &rustls::ServerConnection, sha256: Sha256) -> Self {
        let suite = connection
            .negotiated_cipher_suite()
            .map(|suite| suite.suite());
        let peer_certificates = connection
            .peer_certificates()
            .unwrap_or_default()
            .iter()
            .map(|certificate| {
                Arc::new(PeerCertificateFacts {
                    der: certificate.to_vec(),
                    fingerprint_sha256: sha256.hex(certificate),
                    names: crate::x509::names(certificate),
                })
            })
            .collect();
        Self {
            version: connection
                .protocol_version()
                .map(protocol_version_name)
                .unwrap_or_default(),
            cipher_suite: suite.map(iana_cipher_suite_name).unwrap_or_default(),
            cipher_suite_id: suite.map(u16::from).unwrap_or_default(),
            server_name: connection.server_name().map(str::to_string),
            alpn: connection
                .alpn_protocol()
                .map(|protocol| String::from_utf8_lossy(protocol).into_owned()),
            peer_certificates,
        }
    }
}

/// `req.tls`: the TLS session of the connection a request arrived on.
#[pyclass(module = "siphon", name = "TlsInfo", frozen)]
pub struct TlsInfo {
    session: Arc<TlsSession>,
}

impl TlsInfo {
    pub(crate) fn new(session: Arc<TlsSession>) -> Self {
        Self { session }
    }
}

#[pymethods]
impl TlsInfo {
    /// The protocol version: `"TLSv1.3"` or `"TLSv1.2"`.
    #[getter]
    fn version(&self) -> &str {
        &self.session.version
    }

    /// The negotiated cipher suite by its name in the IANA TLS Cipher Suite
    /// Registry, e.g. `"TLS_AES_128_GCM_SHA256"`.
    #[getter]
    fn cipher_suite(&self) -> &str {
        &self.session.cipher_suite
    }

    /// The negotiated cipher suite as its registry value, e.g. `0x1301`.
    #[getter]
    fn cipher_suite_id(&self) -> u16 {
        self.session.cipher_suite_id
    }

    /// The registry value as its two octets, e.g. `b"\x13\x01"`.
    #[getter]
    fn cipher_suite_bytes<'py>(&self, py: Python<'py>) -> Bound<'py, PyBytes> {
        PyBytes::new(py, &self.session.cipher_suite_id.to_be_bytes())
    }

    /// The server name the client asked for (SNI), or `None`.
    #[getter]
    fn server_name(&self) -> Option<&str> {
        self.session.server_name.as_deref()
    }

    /// The application protocol negotiated by ALPN (`"h2"`, `"http/1.1"`), or
    /// `None` when the client offered none.
    #[getter]
    fn alpn(&self) -> Option<&str> {
        self.session.alpn.as_deref()
    }

    /// The certificate chain the client presented, its own certificate
    /// first. Empty on a listener without `client_ca`.
    #[getter]
    fn peer_certificates(&self) -> Vec<PeerCertificate> {
        self.session
            .peer_certificates
            .iter()
            .map(|facts| PeerCertificate {
                facts: Arc::clone(facts),
            })
            .collect()
    }

    /// The client's own certificate, or `None` when it presented none.
    #[getter]
    fn peer_certificate(&self) -> Option<PeerCertificate> {
        self.session
            .peer_certificates
            .first()
            .map(|facts| PeerCertificate {
                facts: Arc::clone(facts),
            })
    }

    /// Whether the client presented a certificate chain that verified
    /// against the listener's `client_ca`. A listener with `client_ca`
    /// refuses the handshake of a client without such a chain, so there this
    /// is always `True`; without `client_ca` no certificate is asked for and
    /// it is `False`.
    #[getter]
    fn peer_verified(&self) -> bool {
        !self.session.peer_certificates.is_empty()
    }

    fn __repr__(&self) -> String {
        format!(
            "TlsInfo(version={:?}, cipher_suite={:?}, server_name={:?}, peer_verified={})",
            self.session.version,
            self.session.cipher_suite,
            self.session.server_name,
            self.peer_verified()
        )
    }
}

/// One certificate of the chain a client presented.
#[pyclass(module = "siphon", name = "PeerCertificate", frozen)]
pub struct PeerCertificate {
    facts: Arc<PeerCertificateFacts>,
}

#[pymethods]
impl PeerCertificate {
    /// The subject as an RFC 4514 string, most specific part first:
    /// `"CN=naf.example.com,O=Example,C=XX"`. `None` if the certificate
    /// could not be read.
    #[getter]
    fn subject(&self) -> Option<&str> {
        self.facts
            .names
            .as_ref()
            .map(|names| names.subject.as_str())
    }

    /// The issuer as an RFC 4514 string, in the same form as `subject`.
    /// `None` if the certificate could not be read.
    #[getter]
    fn issuer(&self) -> Option<&str> {
        self.facts.names.as_ref().map(|names| names.issuer.as_str())
    }

    /// The subject alternative names as `(kind, value)` tuples in the order
    /// of the certificate, with kind `"dns"`, `"uri"`, `"email"` or `"ip"`.
    #[getter]
    fn subject_alt_names(&self) -> Vec<(&'static str, String)> {
        self.facts
            .names
            .as_ref()
            .map(|names| names.subject_alt_names.clone())
            .unwrap_or_default()
    }

    /// The SHA-256 of the DER certificate as 64 lowercase hexadecimal digits.
    #[getter]
    fn fingerprint_sha256(&self) -> &str {
        &self.facts.fingerprint_sha256
    }

    /// The certificate itself, DER encoded.
    #[getter]
    fn der<'py>(&self, py: Python<'py>) -> Bound<'py, PyBytes> {
        PyBytes::new(py, &self.facts.der)
    }

    fn __repr__(&self) -> String {
        format!(
            "PeerCertificate(subject={:?}, fingerprint_sha256={:?})",
            self.subject().unwrap_or_default(),
            self.facts.fingerprint_sha256
        )
    }
}

// ── Acceptors ────────────────────────────────────────────────────────────

type Accepting<Stream, Service> =
    Pin<Box<dyn Future<Output = io::Result<(Stream, Service)>> + Send>>;

/// The acceptor of a cleartext listener.
#[derive(Clone)]
pub(crate) struct PlainAcceptor {
    pub(crate) listener: Arc<ListenerIdentity>,
}

impl<S> Accept<TcpStream, S> for PlainAcceptor
where
    S: Send + 'static,
{
    type Stream = TcpStream;
    type Service = AddExtension<S, ConnectionInfo>;
    type Future = std::future::Ready<io::Result<(Self::Stream, Self::Service)>>;

    fn accept(&self, stream: TcpStream, service: S) -> Self::Future {
        let info = ConnectionInfo {
            listener: Arc::clone(&self.listener),
            local_addr: stream.local_addr().ok(),
            tls: None,
        };
        std::future::ready(Ok((stream, Extension(info).layer(service))))
    }
}

/// The acceptor of a TLS listener: the rustls handshake, then the session
/// read off the connection it produced.
#[derive(Clone)]
pub(crate) struct TlsAcceptor {
    pub(crate) listener: Arc<ListenerIdentity>,
    pub(crate) inner: RustlsAcceptor,
    pub(crate) sha256: Sha256,
}

impl<S> Accept<TcpStream, S> for TlsAcceptor
where
    S: Send + 'static,
{
    type Stream = TlsStream<TcpStream>;
    type Service = AddExtension<S, ConnectionInfo>;
    type Future = Accepting<Self::Stream, Self::Service>;

    fn accept(&self, stream: TcpStream, service: S) -> Self::Future {
        let local_addr = stream.local_addr().ok();
        let listener = Arc::clone(&self.listener);
        let sha256 = self.sha256;
        let handshake = self.inner.accept(stream, service);
        Box::pin(async move {
            let (stream, service) = handshake.await?;
            let session = TlsSession::from_connection(stream.get_ref().1, sha256);
            let info = ConnectionInfo {
                listener,
                local_addr,
                tls: Some(Arc::new(session)),
            };
            Ok((stream, Extension(info).layer(service)))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cipher_suites_carry_their_iana_registry_names_and_values() {
        // Name and value from the IANA TLS Cipher Suite Registry, for every
        // suite the ring provider can negotiate.
        let registry = [
            (0x1301, "TLS_AES_128_GCM_SHA256"),
            (0x1302, "TLS_AES_256_GCM_SHA384"),
            (0x1303, "TLS_CHACHA20_POLY1305_SHA256"),
            (0xC02B, "TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256"),
            (0xC02C, "TLS_ECDHE_ECDSA_WITH_AES_256_GCM_SHA384"),
            (0xC02F, "TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256"),
            (0xC030, "TLS_ECDHE_RSA_WITH_AES_256_GCM_SHA384"),
            (0xCCA8, "TLS_ECDHE_RSA_WITH_CHACHA20_POLY1305_SHA256"),
            (0xCCA9, "TLS_ECDHE_ECDSA_WITH_CHACHA20_POLY1305_SHA256"),
        ];
        let provider = rustls::crypto::ring::default_provider();
        assert_eq!(provider.cipher_suites.len(), registry.len());
        for suite in &provider.cipher_suites {
            let suite = suite.suite();
            let value = u16::from(suite);
            let expected = registry
                .iter()
                .find(|(known, _)| *known == value)
                .unwrap_or_else(|| panic!("0x{value:04X} is not in the table"))
                .1;
            assert_eq!(iana_cipher_suite_name(suite), expected);
        }
        assert_eq!(
            iana_cipher_suite_name(rustls::CipherSuite::Unknown(0x00ff)),
            "0x00FF"
        );
    }

    #[test]
    fn the_fingerprint_is_the_sha_256_of_the_certificate() {
        // FIPS 180-4 appendix B.1: SHA-256("abc").
        let sha256 = Sha256::from_provider(&rustls::crypto::ring::default_provider()).unwrap();
        assert_eq!(
            sha256.hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn protocol_versions_read_the_way_logs_spell_them() {
        assert_eq!(
            protocol_version_name(rustls::ProtocolVersion::TLSv1_3),
            "TLSv1.3"
        );
        assert_eq!(
            protocol_version_name(rustls::ProtocolVersion::TLSv1_2),
            "TLSv1.2"
        );
    }
}
