//! Tokio-side HTTP server, wired to the script registry via
//! [`siphon::script::ScriptHandle`].
//!
//! Driven from [`crate::task`]: the engine hands us a [`ScriptHandle`] once
//! the script is loaded. We snapshot the registered `http.*` handlers, build
//! an axum router that dispatches each match into `script.call_handler(...)`,
//! and bind the listeners configured in [`crate::HttpConfig`].
//!
//! # Boundaries
//!
//! - **Rust handles**: TCP/TLS termination, HTTP/1.1 framing, path routing
//!   (axum), body buffering with caps, request timeouts, the outbound pool.
//! - **Python handles**: handler dispatch, auth, content negotiation,
//!   business logic, response building.
//!
//! # Handler kinds
//!
//! - `http.route` — a request matched by path + method.
//! - `http.middleware` — a request guard run (in registration order) before
//!   the route handler; returning a `Response` short-circuits, `None`
//!   continues.
//! - `http.startup` — run once, to completion, before any listener accepts.
//! - `http.shutdown` — **roadmap**: needs a siphon shutdown hook for addon
//!   tasks, which isn't exposed yet, so these are not invoked (a loud warning
//!   fires if a script registers one).

use std::collections::HashMap;
use std::io::BufReader;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::extract::{ConnectInfo, MatchedPath, Request as AxumRequest};
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode};
use axum::response::Response as AxumResponse;
use axum::routing::{any, MethodRouter};
use axum::Router;
use http_body_util::BodyExt;
use pyo3::prelude::*;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::server::WebPkiClientVerifier;
use rustls::{RootCertStore, ServerConfig as RustlsServerConfig};
use siphon::script::{HandlerHandle, ScriptHandle};
use tower_http::limit::RequestBodyLimitLayer;
use tower_http::timeout::TimeoutLayer;

use crate::config::ClientConfig;
use crate::parse::{extract_path_params, parse_query};
use crate::{HttpConfig, Request as PyRequest, Response as PyResponse, ServerConfig, TlsConfig};

// ── Named outbound clients ───────────────────────────────────────────────
//
// The `clients:` map from the install-time config, stashed so
// `http.Client("name")` (see `client.rs`) can resolve a pre-configured,
// pooled client by name. Set once at task startup; read-only thereafter.

static NAMED_CLIENTS: OnceLock<HashMap<String, ClientConfig>> = OnceLock::new();

fn set_named_clients(clients: HashMap<String, ClientConfig>) {
    let _ = NAMED_CLIENTS.set(clients);
}

/// Look up a named client's config (used by [`crate::client::Client`]).
pub(crate) fn named_client(name: &str) -> Option<ClientConfig> {
    NAMED_CLIENTS.get().and_then(|m| m.get(name).cloned())
}

// ── Trusted proxies (X-Forwarded-For) ────────────────────────────────────
//
// The union of every listener's `trusted_proxies`. When a request's socket peer
// is in this set, the left-most `X-Forwarded-For` entry is taken as the client
// address instead of the socket peer — so a request through a trusted LB reports
// the real client. Untrusted peers can't spoof it.

static TRUSTED_PROXIES: OnceLock<Vec<IpAddr>> = OnceLock::new();

fn set_trusted_proxies(entries: &[String]) {
    let ips: Vec<IpAddr> = entries
        .iter()
        .filter_map(|e| match e.parse::<IpAddr>() {
            Ok(ip) => Some(ip),
            Err(_) => {
                tracing::warn!(
                    target: "siphon_http",
                    entry = %e,
                    "trusted_proxies entry is not a bare IP address (CIDR ranges are not supported) — ignoring"
                );
                None
            }
        })
        .collect();
    let _ = TRUSTED_PROXIES.set(ips);
}

fn peer_is_trusted(ip: &IpAddr) -> bool {
    TRUSTED_PROXIES.get().is_some_and(|v| v.contains(ip))
}

/// The client address to report to scripts: the left-most `X-Forwarded-For`
/// entry when the socket peer is a trusted proxy, otherwise the socket peer.
fn client_addr(peer: Option<SocketAddr>, headers: &HeaderMap) -> String {
    match peer {
        Some(peer) => {
            if peer_is_trusted(&peer.ip()) {
                if let Some(first) = headers
                    .get("x-forwarded-for")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|xff| xff.split(',').next())
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                {
                    return first.to_string();
                }
            }
            peer.to_string()
        }
        None => "unknown:0".to_string(),
    }
}

/// Spin up the HTTP listeners and register the outbound client pool.
///
/// Called from [`crate::task`]'s closure, so we're already on the tokio
/// runtime that `script.tokio_handle()` points to. Build one shared
/// `axum::Router` across all listeners, run any `http.startup` hooks, then
/// bind each `cfg.servers` entry.
pub fn spawn(cfg: HttpConfig, script: ScriptHandle) {
    set_named_clients(cfg.clients.clone());
    set_trusted_proxies(
        &cfg.servers
            .iter()
            .flat_map(|s| s.trusted_proxies.iter().cloned())
            .collect::<Vec<_>>(),
    );
    let runtime = script.tokio_handle().clone();

    // Snapshot handlers once at startup. Live reload (re-snapshotting on
    // script reload) is a follow-up — it would also need to coordinate with
    // siphon's script reload path.
    let routes = script.handlers_for("http.route");
    let middlewares = Arc::new(script.handlers_for("http.middleware"));
    let startups = script.handlers_for("http.startup");
    tracing::info!(
        target: "siphon_http",
        routes = routes.len(),
        middleware = middlewares.len(),
        startup = startups.len(),
        "handlers registered from script"
    );

    // on_shutdown needs a siphon-side shutdown hook exposed to addon tasks,
    // which isn't available yet. Warn loudly so a registered handler's no-op
    // is never silent.
    let n_shutdown = script.handlers_for("http.shutdown").len();
    if n_shutdown > 0 {
        tracing::warn!(
            target: "siphon_http",
            count = n_shutdown,
            "@http.on_shutdown handlers are not invoked in this release \
             (pending a siphon shutdown hook) — do cleanup elsewhere"
        );
    }

    let router = build_router(routes, Arc::clone(&middlewares), &cfg, script.clone());
    let servers = cfg.servers.clone();

    runtime.spawn(async move {
        // Startup hooks run to completion before any listener accepts.
        for h in &startups {
            if let Err(e) = script.call_handler(h, Vec::new()).await {
                tracing::error!(target: "siphon_http", error = %e, "http.startup handler raised");
            }
        }

        let mut tasks = Vec::new();
        for server in servers {
            let router = router.clone();
            tasks.push(tokio::spawn(async move {
                if let Err(e) = serve_one(server, router).await {
                    tracing::error!(target: "siphon_http", error = %e, "listener failed");
                }
            }));
        }
        for t in tasks {
            let _ = t.await;
        }
    });
}

// ── Router build ─────────────────────────────────────────────────────────

fn build_router(
    routes: Vec<HandlerHandle>,
    middlewares: Arc<Vec<HandlerHandle>>,
    cfg: &HttpConfig,
    script: ScriptHandle,
) -> Router {
    let mut by_path: HashMap<String, Vec<(Vec<Method>, HandlerHandle)>> = HashMap::new();

    for handler in routes {
        let (path, methods) = Python::attach(|py| read_route_options(py, &handler))
            .unwrap_or_else(|e| {
                tracing::warn!(target: "siphon_http", error = %e, "skipping malformed http.route options");
                ("".to_string(), Vec::new())
            });
        if path.is_empty() {
            continue;
        }
        by_path.entry(path).or_default().push((methods, handler));
    }

    let mut router = Router::new();
    for (path, entries) in by_path {
        let method_router = build_method_router(entries, Arc::clone(&middlewares), script.clone());
        router = router.route(&path, method_router);
    }

    // Body cap is per-listener but axum applies it as a layer on the whole
    // router. If multiple listeners with different caps are configured, the
    // most permissive wins; sharper enforcement is a follow-up via a
    // per-listener wrapper service.
    let max_body = cfg
        .servers
        .iter()
        .map(|s| s.max_body_bytes)
        .max()
        .unwrap_or(1 << 20);
    // Router-wide (most permissive across listeners). Bounds the client's wait
    // and returns 408; a Python handler already executing can't be cancelled
    // mid-call, so the work may continue after the client is answered.
    let timeout = cfg
        .servers
        .iter()
        .map(|s| s.request_timeout_ms)
        .max()
        .unwrap_or(30_000);
    router
        .layer(RequestBodyLimitLayer::new(max_body))
        .layer(TimeoutLayer::with_status_code(
            StatusCode::GATEWAY_TIMEOUT,
            Duration::from_millis(timeout),
        ))
}

fn read_route_options(py: Python<'_>, handler: &HandlerHandle) -> PyResult<(String, Vec<Method>)> {
    let opts = handler
        .options(py)
        .ok_or_else(|| pyo3::exceptions::PyValueError::new_err("http.route missing options"))?;
    let path: String = opts
        .get_item("path")?
        .ok_or_else(|| {
            pyo3::exceptions::PyValueError::new_err("http.route options missing 'path'")
        })?
        .extract()?;
    let methods: Vec<String> = opts
        .get_item("methods")?
        .ok_or_else(|| {
            pyo3::exceptions::PyValueError::new_err("http.route options missing 'methods'")
        })?
        .extract()?;
    let methods = methods
        .into_iter()
        .filter_map(|m| Method::from_bytes(m.as_bytes()).ok())
        .collect();
    Ok((path, methods))
}

fn build_method_router(
    entries: Vec<(Vec<Method>, HandlerHandle)>,
    middlewares: Arc<Vec<HandlerHandle>>,
    script: ScriptHandle,
) -> MethodRouter {
    // Single `any` dispatcher that picks the matching handler by method at
    // request time. Lets us handle multiple `@http.route` decorators for the
    // same path with different method sets without needing axum's typed
    // `.get(...).put(...)` wiring at build time.
    //
    // Wrap entries in Arc — `HandlerHandle: Clone` calls `Py<T>::clone` under
    // the hood, which panics when invoked from a tokio worker that hasn't
    // called `Python::attach`. Cloning the Arc is a plain refcount bump.
    let entries = Arc::new(entries);
    any(move |req: AxumRequest| {
        let entries = Arc::clone(&entries);
        let middlewares = Arc::clone(&middlewares);
        let script = script.clone();
        async move { dispatch(req, entries, middlewares, script).await }
    })
}

// ── Dispatch ─────────────────────────────────────────────────────────────

async fn dispatch(
    req: AxumRequest,
    entries: Arc<Vec<(Vec<Method>, HandlerHandle)>>,
    middlewares: Arc<Vec<HandlerHandle>>,
    script: ScriptHandle,
) -> AxumResponse {
    let method = req.method().clone();
    let handler_idx = match entries
        .iter()
        .position(|(methods, _)| methods.contains(&method))
    {
        Some(i) => i,
        None => return error_response(StatusCode::METHOD_NOT_ALLOWED, "method not allowed"),
    };
    let handler = &entries[handler_idx].1;

    let (parts, body) = req.into_parts();
    let body_bytes = match collect_body(body).await {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(target: "siphon_http", error = %e, "body collect failed");
            return error_response(StatusCode::BAD_REQUEST, "bad body");
        }
    };

    let path_params = parts
        .extensions
        .get::<MatchedPath>()
        .map(|m| extract_path_params(m.as_str(), parts.uri.path()))
        .unwrap_or_default();
    let query_params = parse_query(parts.uri.query().unwrap_or(""));
    let headers = headers_to_map(&parts.headers);
    let peer = parts
        .extensions
        .get::<ConnectInfo<SocketAddr>>()
        .map(|c| c.0);
    let client = client_addr(peer, &parts.headers);

    let py_request = PyRequest::from_parts(
        method.as_str().to_string(),
        parts.uri.path().to_string(),
        path_params,
        query_params,
        headers,
        body_bytes.to_vec(),
        client,
    );

    // Build the request pyclass once; share it (refcount clone) across every
    // middleware plus the route handler.
    let req_obj = match Python::attach(|py| -> PyResult<Py<PyAny>> {
        Ok(Py::new(py, py_request)?.into_any())
    }) {
        Ok(p) => p,
        Err(e) => {
            tracing::error!(target: "siphon_http", error = %e, "Py::new(Request) failed");
            return error_response(StatusCode::INTERNAL_SERVER_ERROR, "internal error");
        }
    };

    // Middleware chain: each runs before the route handler and may
    // short-circuit by returning a Response; None continues.
    for mw in middlewares.iter() {
        let arg = Python::attach(|py| req_obj.clone_ref(py));
        match script.call_handler(mw, vec![arg]).await {
            Ok(ret) => match Python::attach(|py| middleware_outcome(py, &ret)) {
                MwOutcome::Continue => {}
                MwOutcome::Respond(resp) => return resp,
            },
            Err(e) => {
                tracing::warn!(target: "siphon_http", error = %e, "http.middleware raised");
                return error_response(StatusCode::INTERNAL_SERVER_ERROR, "middleware error");
            }
        }
    }

    let arg = Python::attach(|py| req_obj.clone_ref(py));
    match script.call_handler(handler, vec![arg]).await {
        Ok(returned) => convert_response(returned),
        Err(e) => {
            tracing::warn!(target: "siphon_http", error = %e, "handler raised");
            error_response(StatusCode::INTERNAL_SERVER_ERROR, "handler error")
        }
    }
}

enum MwOutcome {
    Continue,
    Respond(AxumResponse),
}

/// Interpret a middleware's return value: `None` → continue the chain; a
/// `Response` → short-circuit with it; anything else → 500 (script error).
fn middleware_outcome(py: Python<'_>, ret: &Py<PyAny>) -> MwOutcome {
    let bound = ret.bind(py);
    if bound.is_none() {
        return MwOutcome::Continue;
    }
    match bound.extract::<PyRef<PyResponse>>() {
        Ok(resp) => MwOutcome::Respond(build_axum_response(&resp)),
        Err(_) => {
            tracing::warn!(
                target: "siphon_http",
                "http.middleware returned neither Response nor None"
            );
            MwOutcome::Respond(error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "middleware must return Response or None",
            ))
        }
    }
}

async fn collect_body(body: Body) -> Result<Bytes, axum::Error> {
    let collected = body.collect().await.map_err(axum::Error::new)?;
    Ok(collected.to_bytes())
}

fn headers_to_map(headers: &HeaderMap) -> HashMap<String, String> {
    let mut out: HashMap<String, Vec<String>> = HashMap::new();
    for (name, value) in headers {
        let name = name.as_str().to_ascii_lowercase();
        if let Ok(val) = value.to_str() {
            out.entry(name).or_default().push(val.to_string());
        }
    }
    out.into_iter().map(|(k, vs)| (k, vs.join(", "))).collect()
}

fn convert_response(returned: Py<PyAny>) -> AxumResponse {
    Python::attach(|py| {
        let bound = returned.bind(py);
        match bound.extract::<PyRef<PyResponse>>() {
            Ok(resp) => build_axum_response(&resp),
            Err(e) => {
                tracing::warn!(target: "siphon_http", error = %e, "handler did not return Response");
                error_response(StatusCode::INTERNAL_SERVER_ERROR, "bad response")
            }
        }
    })
}

fn build_axum_response(resp: &PyResponse) -> AxumResponse {
    let status = StatusCode::from_u16(resp.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    let mut builder = AxumResponse::builder().status(status);
    for (k, v) in &resp.headers {
        if let (Ok(name), Ok(val)) = (
            HeaderName::from_bytes(k.as_bytes()),
            HeaderValue::from_str(v),
        ) {
            builder = builder.header(name, val);
        }
    }
    let body = Body::from(resp.body_bytes.clone());
    builder
        .body(body)
        .unwrap_or_else(|_| error_response(StatusCode::INTERNAL_SERVER_ERROR, "build failed"))
}

fn error_response(status: StatusCode, msg: &str) -> AxumResponse {
    AxumResponse::builder()
        .status(status)
        .header("content-type", "text/plain; charset=utf-8")
        .body(Body::from(format!("{msg}\n")))
        .unwrap()
}

// ── Listener bind ────────────────────────────────────────────────────────

async fn serve_one(server: ServerConfig, router: Router) -> std::io::Result<()> {
    let addr: SocketAddr = server
        .listen
        .parse()
        .map_err(|e: std::net::AddrParseError| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, e.to_string())
        })?;
    let app = router.into_make_service_with_connect_info::<SocketAddr>();

    if let Some(tls) = server.tls.as_ref() {
        let config = build_rustls_config(tls).await?;
        tracing::info!(target: "siphon_http", listen = %addr, tls = true, "binding HTTPS listener");
        axum_server::bind_rustls(addr, config).serve(app).await?;
    } else {
        tracing::info!(target: "siphon_http", listen = %addr, tls = false, "binding HTTP listener");
        axum_server::bind(addr).serve(app).await?;
    }
    Ok(())
}

/// Build the TLS config for a listener.
///
/// Loads the server cert chain + key, wires client-certificate verification
/// when `client_ca` is set (mutual TLS), and advertises ALPN `h2, http/1.1` so
/// HTTP/2 negotiates over TLS. Pins the ring crypto provider explicitly to
/// avoid ambiguity when more than one provider is linked.
async fn build_rustls_config(
    tls: &TlsConfig,
) -> std::io::Result<axum_server::tls_rustls::RustlsConfig> {
    let certs = load_certs(&tls.cert_path)?;
    let key = load_key(&tls.key_path)?;

    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let builder = RustlsServerConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .map_err(to_io)?;

    let builder = match &tls.client_ca {
        Some(ca_path) => {
            let mut roots = RootCertStore::empty();
            for cert in load_certs(ca_path)? {
                roots.add(cert).map_err(to_io)?;
            }
            let verifier = WebPkiClientVerifier::builder_with_provider(Arc::new(roots), provider)
                .build()
                .map_err(to_io)?;
            builder.with_client_cert_verifier(verifier)
        }
        None => builder.with_no_client_auth(),
    };

    let mut config = builder.with_single_cert(certs, key).map_err(to_io)?;
    config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];

    Ok(axum_server::tls_rustls::RustlsConfig::from_config(
        Arc::new(config),
    ))
}

fn load_certs(path: &str) -> std::io::Result<Vec<CertificateDer<'static>>> {
    let file = std::fs::File::open(path).map_err(|e| {
        std::io::Error::new(std::io::ErrorKind::NotFound, format!("open {path}: {e}"))
    })?;
    rustls_pemfile::certs(&mut BufReader::new(file)).collect()
}

fn load_key(path: &str) -> std::io::Result<PrivateKeyDer<'static>> {
    let file = std::fs::File::open(path).map_err(|e| {
        std::io::Error::new(std::io::ErrorKind::NotFound, format!("open {path}: {e}"))
    })?;
    rustls_pemfile::private_key(&mut BufReader::new(file))?.ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("no private key in {path}"),
        )
    })
}

fn to_io<E: std::fmt::Display>(e: E) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidInput, e.to_string())
}
