//! A request through a real listener, read the way a handler reads it.
//!
//! Each test binds a listener the way the addon binds its own, connects a
//! real client to it, and serves the request with a probe that builds the
//! `Request` object the dispatcher builds and evaluates a Python expression
//! on it. What comes back is what a script's handler would have seen.
//!
//! The TLS client is rustls driven directly, so a test can pin the protocol
//! version and the cipher suite and compare what the handler saw with what
//! the client itself negotiated.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::extract::Request as AxumRequest;
use axum::routing::any;
use axum::Router;
use pyo3::prelude::*;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::runtime::{bind_listener, request_from_parts};
use crate::HttpConfig;

const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/tls");

/// What `openssl x509 -noout -fingerprint -sha256` prints for client.pem.
const CLIENT_FINGERPRINT: &str = "ac7fb7e0378136e7925681c08649a24217db52a537abcac5268c6d4f1a539eae";
/// What `openssl x509 -noout -subject -nameopt RFC2253` prints for it.
const CLIENT_SUBJECT: &str = "CN=naf.example.com,OU=Application Functions,O=Example Test,C=XX";

/// Serve every path with a probe: the request header `x-probe` holds a Python
/// expression over `req`, and the response body is its `repr`.
fn probe_router() -> Router {
    Router::new().route(
        "/{*path}",
        any(|request: AxumRequest| async move {
            let (parts, _body) = request.into_parts();
            let expression = parts
                .headers
                .get("x-probe")
                .and_then(|value| value.to_str().ok())
                .unwrap_or("None")
                .to_string();
            let py_request = request_from_parts(&parts, Vec::new());
            Python::attach(|py| -> PyResult<String> {
                let locals = pyo3::types::PyDict::new(py);
                locals.set_item("req", Py::new(py, py_request)?)?;
                let code = std::ffi::CString::new(expression)?;
                let value = py.eval(code.as_c_str(), None, Some(&locals))?;
                Ok(value.repr()?.to_string())
            })
            .unwrap_or_else(|error| format!("probe failed: {error}"))
        }),
    )
}

/// Bind `yaml`'s only listener on a free port, serve the probe on it, and
/// return its address.
async fn serve(yaml: &str) -> SocketAddr {
    let config = HttpConfig::from_yaml(yaml).expect("the test config");
    let listener = bind_listener(&config.servers[0]).expect("the listener binds");
    let address = listener.local_addr().expect("the bound address");
    tokio::spawn(async move {
        listener
            .serve(probe_router())
            .await
            .expect("the listener serves");
    });
    // Bound is not listening yet: wait until the socket accepts.
    for _ in 0..200 {
        if tokio::net::TcpStream::connect(address).await.is_ok() {
            return address;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("the listener on {address} never accepted");
}

fn tls_yaml(name: &str, mutual: bool) -> String {
    let client_ca = if mutual {
        format!("      client_ca: \"{FIXTURES}/ca.pem\"\n")
    } else {
        String::new()
    };
    format!(
        "servers:\n  - listen: \"127.0.0.1:0\"\n    name: {name}\n    tls:\n      cert_path: \"{FIXTURES}/server.pem\"\n      key_path: \"{FIXTURES}/server.key\"\n{client_ca}"
    )
}

fn certificates(file: &str) -> Vec<CertificateDer<'static>> {
    CertificateDer::pem_file_iter(format!("{FIXTURES}/{file}"))
        .expect("the fixture opens")
        .collect::<Result<_, _>>()
        .expect("the fixture parses")
}

fn private_key(file: &str) -> PrivateKeyDer<'static> {
    PrivateKeyDer::from_pem_file(format!("{FIXTURES}/{file}")).expect("the fixture key")
}

/// How a test's TLS client is set up.
struct Client {
    version: &'static rustls::SupportedProtocolVersion,
    cipher_suite: rustls::CipherSuite,
    /// Certificate and key files, for a listener that asks for a certificate.
    identity: Option<(&'static str, &'static str)>,
    alpn: Option<&'static [u8]>,
}

impl Client {
    /// A rustls client that can negotiate exactly one cipher suite.
    fn config(&self) -> rustls::ClientConfig {
        let mut provider = rustls::crypto::ring::default_provider();
        provider
            .cipher_suites
            .retain(|suite| suite.suite() == self.cipher_suite);
        assert_eq!(provider.cipher_suites.len(), 1, "one suite to offer");
        let mut roots = rustls::RootCertStore::empty();
        for certificate in certificates("ca.pem") {
            roots.add(certificate).expect("the test CA");
        }
        let builder = rustls::ClientConfig::builder_with_provider(Arc::new(provider))
            .with_protocol_versions(&[self.version])
            .expect("the version and the suite agree")
            .with_root_certificates(roots);
        let mut config = match self.identity {
            Some((certificate, key)) => builder
                .with_client_auth_cert(certificates(certificate), private_key(key))
                .expect("the client certificate"),
            None => builder.with_no_client_auth(),
        };
        if let Some(protocol) = self.alpn {
            config.alpn_protocols = vec![protocol.to_vec()];
        }
        config
    }

    /// Connect, send one HTTP/1.1 request whose probe is `expression`, and
    /// return the cipher suite the client negotiated and the response body.
    async fn probe(
        &self,
        address: SocketAddr,
        expression: &str,
    ) -> std::io::Result<(rustls::CipherSuite, String)> {
        let connector = tokio_rustls::TlsConnector::from(Arc::new(self.config()));
        let tcp = tokio::net::TcpStream::connect(address).await?;
        let name = ServerName::try_from("localhost").expect("a DNS name");
        let mut stream = connector.connect(name, tcp).await?;
        let negotiated = stream
            .get_ref()
            .1
            .negotiated_cipher_suite()
            .expect("the handshake completed")
            .suite();
        let request = format!(
            "GET /probe HTTP/1.1\r\nHost: localhost\r\nX-Probe: {expression}\r\nConnection: close\r\n\r\n"
        );
        stream.write_all(request.as_bytes()).await?;
        let mut response = Vec::new();
        // A peer that closes without close_notify still gave a full response.
        let _ = stream.read_to_end(&mut response).await;
        let response = String::from_utf8_lossy(&response).into_owned();
        if response.is_empty() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "the server closed the connection without a response",
            ));
        }
        let body = response
            .split_once("\r\n\r\n")
            .map(|(_, body)| body.to_string())
            .unwrap_or_default();
        Ok((negotiated, body))
    }
}

const TLS13: &rustls::SupportedProtocolVersion = &rustls::version::TLS13;
const TLS12: &rustls::SupportedProtocolVersion = &rustls::version::TLS12;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_handler_sees_the_cipher_suite_the_client_negotiated() {
    let address = serve(&tls_yaml("ua", false)).await;
    // (version, suite the client offers alone, IANA name, IANA value)
    let cases = [
        (
            TLS13,
            rustls::CipherSuite::TLS13_AES_128_GCM_SHA256,
            "TLS_AES_128_GCM_SHA256",
            0x1301u16,
        ),
        (
            TLS13,
            rustls::CipherSuite::TLS13_AES_256_GCM_SHA384,
            "TLS_AES_256_GCM_SHA384",
            0x1302,
        ),
        (
            TLS13,
            rustls::CipherSuite::TLS13_CHACHA20_POLY1305_SHA256,
            "TLS_CHACHA20_POLY1305_SHA256",
            0x1303,
        ),
        (
            TLS12,
            rustls::CipherSuite::TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256,
            "TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256",
            0xC02B,
        ),
        (
            TLS12,
            rustls::CipherSuite::TLS_ECDHE_ECDSA_WITH_CHACHA20_POLY1305_SHA256,
            "TLS_ECDHE_ECDSA_WITH_CHACHA20_POLY1305_SHA256",
            0xCCA9,
        ),
    ];
    for (version, suite, name, value) in cases {
        let client = Client {
            version,
            cipher_suite: suite,
            identity: None,
            alpn: None,
        };
        let (negotiated, body) = client
            .probe(
                address,
                "(req.tls.version, req.tls.cipher_suite, req.tls.cipher_suite_id, req.tls.cipher_suite_bytes)",
            )
            .await
            .expect("the request is served");
        // What the client's own connection says it negotiated.
        assert_eq!(negotiated, suite);
        assert_eq!(u16::from(negotiated), value);
        let version_name = if version.version == rustls::ProtocolVersion::TLSv1_3 {
            "TLSv1.3"
        } else {
            "TLSv1.2"
        };
        // Python's repr of the two octets, which shows printable ones as
        // characters.
        let octets = Python::attach(|py| {
            pyo3::types::PyBytes::new(py, &value.to_be_bytes())
                .repr()
                .map(|repr| repr.to_string())
                .expect("a repr")
        });
        assert_eq!(
            body,
            format!("('{version_name}', '{name}', {value}, {octets})")
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_handler_sees_the_server_name_and_the_application_protocol() {
    let address = serve(&tls_yaml("ua", false)).await;
    let mut client = Client {
        version: TLS13,
        cipher_suite: rustls::CipherSuite::TLS13_AES_128_GCM_SHA256,
        identity: None,
        alpn: Some(b"http/1.1"),
    };
    let expression = "(req.tls.server_name, req.tls.alpn, req.tls.peer_verified, req.tls.peer_certificate, req.tls.peer_certificates)";
    let (_, body) = client.probe(address, expression).await.unwrap();
    assert_eq!(body, "('localhost', 'http/1.1', False, None, [])");

    // A client that offers no ALPN protocol negotiates none.
    client.alpn = None;
    let (_, body) = client.probe(address, "req.tls.alpn").await.unwrap();
    assert_eq!(body, "None");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_handler_sees_the_verified_client_certificate() {
    let address = serve(&tls_yaml("zn", true)).await;
    let client = Client {
        version: TLS13,
        cipher_suite: rustls::CipherSuite::TLS13_AES_256_GCM_SHA384,
        identity: Some(("client.pem", "client.key")),
        alpn: None,
    };
    let certificate = "req.tls.peer_certificate";
    let (_, body) = client
        .probe(
            address,
            &format!("(req.tls.peer_verified, len(req.tls.peer_certificates), {certificate}.subject, {certificate}.fingerprint_sha256)"),
        )
        .await
        .expect("a client with a certificate of the CA is served");
    assert_eq!(
        body,
        format!("(True, 1, '{CLIENT_SUBJECT}', '{CLIENT_FINGERPRINT}')")
    );

    let (_, body) = client
        .probe(address, &format!("{certificate}.issuer"))
        .await
        .unwrap();
    assert_eq!(body, "'CN=Example Test CA,O=Example Test'");

    let (_, body) = client
        .probe(address, &format!("{certificate}.subject_alt_names"))
        .await
        .unwrap();
    assert_eq!(
        body,
        "[('dns', 'naf.example.com'), ('uri', 'urn:example:naf:1'), ('ip', '192.0.2.7'), ('email', 'naf@example.com')]"
    );

    // The DER octets are the certificate the client sent.
    let sent = certificates("client.pem").remove(0);
    let (_, body) = client
        .probe(address, &format!("{certificate}.der.hex()"))
        .await
        .unwrap();
    let hex: String = sent.iter().map(|octet| format!("{octet:02x}")).collect();
    assert_eq!(body, format!("'{hex}'"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_client_the_ca_did_not_sign_reaches_no_handler() {
    let address = serve(&tls_yaml("zn", true)).await;
    for identity in [Some(("untrusted-client.pem", "untrusted-client.key")), None] {
        let client = Client {
            version: TLS13,
            cipher_suite: rustls::CipherSuite::TLS13_AES_128_GCM_SHA256,
            identity,
            alpn: None,
        };
        let outcome = client.probe(address, "req.tls.peer_verified").await;
        assert!(
            outcome.is_err(),
            "a request was served to {identity:?}: {outcome:?}"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_handler_knows_the_listener_it_was_reached_on() {
    let tls = serve(&tls_yaml("zn", false)).await;
    let client = Client {
        version: TLS13,
        cipher_suite: rustls::CipherSuite::TLS13_AES_128_GCM_SHA256,
        identity: None,
        alpn: None,
    };
    let expression =
        "(req.listener.name, req.listener.address, req.listener.port, req.listener.tls)";
    let (_, body) = client.probe(tls, expression).await.unwrap();
    assert_eq!(body, format!("('zn', '{tls}', {}, True)", tls.port()));

    // A cleartext listener without a name is known by its `listen` value,
    // and has no TLS session.
    let plain = serve("servers:\n  - listen: \"127.0.0.1:0\"\n").await;
    let response = reqwest::Client::new()
        .get(format!("http://{plain}/probe"))
        .header("x-probe", format!("({expression}, req.tls)"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert_eq!(
        response,
        format!(
            "(('127.0.0.1:0', '{plain}', {}, False), None)",
            plain.port()
        )
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_peer_is_the_socket_even_behind_a_trusted_proxy() {
    // 127.0.0.1 is a trusted proxy, so `client` follows X-Forwarded-For;
    // `peer` must still name the host on the other end of the socket.
    crate::runtime::set_trusted_proxies(&["127.0.0.1".to_string()]);
    let plain = serve("servers:\n  - listen: \"127.0.0.1:0\"\n").await;
    let mut stream = tokio::net::TcpStream::connect(plain).await.unwrap();
    let local = stream.local_addr().unwrap();
    let request = "GET /probe HTTP/1.1\r\nHost: localhost\r\nX-Forwarded-For: 203.0.113.9\r\nX-Probe: (req.client, req.peer)\r\nConnection: close\r\n\r\n";
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut response = Vec::new();
    stream.read_to_end(&mut response).await.unwrap();
    let response = String::from_utf8_lossy(&response);
    let body = response.split_once("\r\n\r\n").unwrap().1;
    assert_eq!(body, format!("('203.0.113.9', '{local}')"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_host_of_an_http_2_request_is_its_authority() {
    let plain = serve("servers:\n  - listen: \"127.0.0.1:0\"\n").await;
    let expression = "(req.host, 'host' in req.headers)";

    // HTTP/2 with prior knowledge: the name travels in `:authority`.
    let response = reqwest::Client::builder()
        .http2_prior_knowledge()
        .build()
        .unwrap()
        .get(format!("http://{plain}/probe"))
        .header("x-probe", expression)
        .send()
        .await
        .unwrap();
    assert_eq!(response.version(), reqwest::Version::HTTP_2);
    assert_eq!(
        response.text().await.unwrap(),
        format!("('{plain}', False)")
    );

    // HTTP/1.1: the Host header.
    let response = reqwest::Client::new()
        .get(format!("http://{plain}/probe"))
        .header("x-probe", expression)
        .send()
        .await
        .unwrap();
    assert_eq!(response.version(), reqwest::Version::HTTP_11);
    assert_eq!(response.text().await.unwrap(), format!("('{plain}', True)"));
}

// ── Routes restricted to listeners ───────────────────────────────────────

use crate::runtime::{
    match_route, route_gate, route_options, unknown_route_listeners, RouteMatch, RouteOptions,
    RouteScope,
};
use axum::http::Method;

fn scope(methods: &[Method], listeners: Option<&[&str]>) -> RouteScope {
    RouteScope {
        methods: methods.to_vec(),
        listeners: listeners.map(|names| names.iter().map(|name| name.to_string()).collect()),
    }
}

#[test]
fn a_route_without_listeners_is_served_on_every_listener() {
    let scopes = [scope(&[Method::GET], None)];
    for listener in [Some("public"), Some("internal"), None] {
        assert_eq!(
            match_route(&scopes, &Method::GET, listener),
            RouteMatch::Handler(0)
        );
        assert_eq!(
            match_route(&scopes, &Method::POST, listener),
            RouteMatch::MethodNotAllowed
        );
    }
}

#[test]
fn a_restricted_route_does_not_exist_on_another_listener() {
    let scopes = [scope(&[Method::POST], Some(&["internal"]))];
    assert_eq!(
        match_route(&scopes, &Method::POST, Some("internal")),
        RouteMatch::Handler(0)
    );
    assert_eq!(
        match_route(&scopes, &Method::GET, Some("internal")),
        RouteMatch::MethodNotAllowed
    );
    // On the public listener it is neither served nor a 405, which would
    // say the path exists: it is not found.
    for method in [Method::POST, Method::GET] {
        assert_eq!(
            match_route(&scopes, &method, Some("public")),
            RouteMatch::NotOnThisListener
        );
    }
    // A request whose listener is not known is not let through.
    assert_eq!(
        match_route(&scopes, &Method::POST, None),
        RouteMatch::NotOnThisListener
    );
}

#[test]
fn one_path_can_have_a_handler_per_listener() {
    let scopes = [
        scope(&[Method::GET], Some(&["public"])),
        scope(&[Method::GET, Method::POST], Some(&["internal", "admin"])),
    ];
    assert_eq!(
        match_route(&scopes, &Method::GET, Some("public")),
        RouteMatch::Handler(0)
    );
    assert_eq!(
        match_route(&scopes, &Method::GET, Some("admin")),
        RouteMatch::Handler(1)
    );
    assert_eq!(
        match_route(&scopes, &Method::POST, Some("public")),
        RouteMatch::MethodNotAllowed
    );
}

#[test]
fn a_route_naming_a_listener_nobody_configured_is_reported() {
    let config = HttpConfig::from_yaml(
        "servers:\n  - listen: \"127.0.0.1:8080\"\n  - listen: \"127.0.0.1:8443\"\n    name: internal\n",
    )
    .unwrap();
    let everywhere = scope(&[Method::GET], None);
    let internal = scope(&[Method::GET], Some(&["internal"]));
    let by_address = scope(&[Method::GET], Some(&["127.0.0.1:8080"]));
    let typo = scope(&[Method::GET], Some(&["internal", "intrenal"]));
    let routes = [
        ("/a", &everywhere),
        ("/b", &internal),
        ("/c", &by_address),
        ("/d", &typo),
    ];
    assert_eq!(
        unknown_route_listeners(routes, &config),
        vec![("/d".to_string(), "intrenal".to_string())]
    );
}

/// The options the real `route` decorator registers, read back the way the
/// runtime reads them.
fn registered_options(py: Python<'_>, call: &str) -> RouteOptions {
    let namespace = crate::namespace(HttpConfig::default())(py).expect("the namespace builds");
    let code = std::ffi::CString::new(format!(
        "import sys, types\n\
         registry = types.ModuleType('_siphon_registry')\n\
         registry.calls = []\n\
         registry.register = lambda *args: registry.calls.append(args)\n\
         sys.modules['_siphon_registry'] = registry\n\
         @{call}\n\
         def handler(req):\n\
         \x20   return None\n\
         options = registry.calls[-1][4]\n"
    ))
    .unwrap();
    let globals = pyo3::types::PyDict::new(py);
    globals.set_item("http", namespace).unwrap();
    py.run(code.as_c_str(), Some(&globals), None)
        .expect("the decorator runs");
    let options = globals.get_item("options").unwrap().expect("options");
    route_options(options.cast::<pyo3::types::PyDict>().expect("a dict")).expect("valid options")
}

#[test]
fn the_decorator_registers_the_listeners_of_a_route() {
    Python::attach(|py| {
        // As every script written so far calls it: no restriction.
        let plain = registered_options(py, "http.route('/hello', methods=['get'])");
        assert_eq!(plain.path, "/hello");
        assert_eq!(plain.methods, vec![Method::GET]);
        assert_eq!(plain.listeners, None);

        let restricted = registered_options(
            py,
            "http.route('/internal', methods=['POST'], listeners=['zn', 'admin'])",
        );
        assert_eq!(
            restricted.listeners,
            Some(vec!["zn".to_string(), "admin".to_string()])
        );

        // One name given as a string is one listener, not its letters.
        let single = registered_options(py, "http.route('/internal', listeners='zn')");
        assert_eq!(single.listeners, Some(vec!["zn".to_string()]));

        // Options registered before listeners existed have no such key.
        let legacy = pyo3::types::PyDict::new(py);
        legacy.set_item("path", "/old").unwrap();
        legacy.set_item("methods", vec!["GET"]).unwrap();
        assert_eq!(route_options(&legacy).unwrap().listeners, None);
    });
}

/// Serve `/internal` restricted to the listener `internal`, and `/open`
/// everywhere, through the gate the dispatcher puts every request through.
fn gated_router() -> Router {
    let internal = Arc::new(vec![scope(&[Method::GET], Some(&["internal"]))]);
    let open = Arc::new(vec![scope(&[Method::GET], None)]);
    let gate = |scopes: Arc<Vec<RouteScope>>| {
        any(move |request: AxumRequest| {
            let scopes = Arc::clone(&scopes);
            async move {
                match route_gate(&scopes, &request) {
                    RouteMatch::Handler(index) => axum::response::Response::new(
                        axum::body::Body::from(format!("handler {index}")),
                    ),
                    refused => refused.refusal(),
                }
            }
        })
    };
    Router::new()
        .route("/internal", gate(internal))
        .route("/open", gate(open))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_restricted_route_is_not_reachable_through_another_listener() {
    let config = HttpConfig::from_yaml(
        "servers:\n  - listen: \"127.0.0.1:0\"\n    name: public\n  - listen: \"127.0.0.1:0\"\n    name: internal\n",
    )
    .unwrap();
    let mut addresses = Vec::new();
    for server in &config.servers {
        let listener = bind_listener(server).expect("the listener binds");
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            listener
                .serve(gated_router())
                .await
                .expect("the listener serves");
        });
        addresses.push(address);
    }
    let client = reqwest::Client::new();
    let get = |address: SocketAddr, path: &'static str| {
        let client = client.clone();
        async move {
            for _ in 0..200 {
                if let Ok(response) = client.get(format!("http://{address}{path}")).send().await {
                    return (response.status().as_u16(), response.text().await.unwrap());
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            panic!("no answer from {address}");
        }
    };
    let (public, internal) = (addresses[0], addresses[1]);

    assert_eq!(
        get(internal, "/internal").await,
        (200, "handler 0".to_string())
    );
    // Through the public listener the route does not exist.
    assert_eq!(get(public, "/internal").await, (404, String::new()));
    // The same answer as for a path no route has at all.
    assert_eq!(get(public, "/nothing-here").await.0, 404);
    // An unrestricted route is served on both, as before.
    assert_eq!(get(public, "/open").await.0, 200);
    assert_eq!(get(internal, "/open").await.0, 200);
    // Another method on the internal listener is a 405; on the public one
    // the path stays not found.
    let post = |address: SocketAddr| {
        let client = client.clone();
        async move {
            client
                .post(format!("http://{address}/internal"))
                .send()
                .await
                .unwrap()
                .status()
                .as_u16()
        }
    };
    assert_eq!(post(internal).await, 405);
    assert_eq!(post(public).await, 404);
}

/// `verify` is read as a bundle: a server whose CA is the second certificate
/// of the file is trusted. With `verify_only` nothing outside the bundle is.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_client_trusts_every_certificate_of_its_bundle_and_with_verify_only_nothing_else() {
    let address = serve(&tls_yaml("zt", false)).await;
    let directory = std::env::temp_dir().join(format!("siphon-http-trust-{}", std::process::id()));
    std::fs::create_dir_all(&directory).unwrap();
    let read = |name: &str| std::fs::read_to_string(format!("{FIXTURES}/{name}")).unwrap();
    let both = directory.join("both.pem");
    std::fs::write(&both, read("untrusted-client.pem") + &read("ca.pem")).unwrap();
    let other = directory.join("other.pem");
    std::fs::write(&other, read("untrusted-client.pem")).unwrap();
    let url = format!("https://localhost:{}/", address.port());
    let send = |path: &std::path::Path, only: bool| {
        let client = crate::client::trusting(reqwest::Client::builder(), path.to_str(), only)
            .unwrap()
            .build()
            .unwrap();
        let url = url.clone();
        async move { client.get(url).send().await }
    };
    assert!(
        send(&both, true).await.is_ok(),
        "the CA second in the bundle is trusted"
    );
    assert!(
        send(&other, true).await.is_err(),
        "a bundle without the CA trusts nothing else"
    );
    assert!(crate::client::trusting(reqwest::Client::builder(), None, true).is_err());
    std::fs::remove_dir_all(&directory).unwrap();
}

/// A named client follows a redirect by default, as reqwest does. With
/// `follow_redirects: false` the 3xx comes back to the script as received,
/// `Location` included, for it to relay or follow itself.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_named_client_follows_a_redirect_unless_told_not_to() {
    let config = HttpConfig::from_yaml("servers:\n  - listen: \"127.0.0.1:0\"\n").unwrap();
    let listener = bind_listener(&config.servers[0]).expect("the listener binds");
    let address = listener.local_addr().unwrap();
    let router = Router::new()
        .route(
            "/old",
            any(|| async {
                axum::response::Response::builder()
                    .status(307)
                    .header("location", "/new")
                    .header("x-hop", "old")
                    .body(axum::body::Body::empty())
                    .unwrap()
            }),
        )
        .route("/new", any(|| async { "moved" }));
    tokio::spawn(async move { listener.serve(router).await.expect("the listener serves") });

    let send = |yaml: String| async move {
        let config = HttpConfig::from_yaml(&yaml).unwrap();
        let client = crate::client::Client::pool(&config.clients["p"]).unwrap();
        for _ in 0..200 {
            if let Ok(response) = client.get(format!("http://{address}/old")).send().await {
                return response;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("no answer from {address}");
    };

    let followed = send("clients:\n  p: {}\n".to_string()).await;
    assert_eq!(followed.status().as_u16(), 200);
    assert_eq!(followed.text().await.unwrap(), "moved");

    let relayed = send("clients:\n  p:\n    follow_redirects: false\n".to_string()).await;
    assert_eq!(relayed.status().as_u16(), 307);
    assert_eq!(relayed.headers()["location"], "/new");
    assert_eq!(relayed.headers()["x-hop"], "old");
}
