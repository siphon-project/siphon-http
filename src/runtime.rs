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
use rustls::pki_types::pem::{self, PemObject};
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::server::WebPkiClientVerifier;
use rustls::{RootCertStore, ServerConfig as RustlsServerConfig};
use siphon::script::{HandlerHandle, ScriptHandle};
use tower_http::limit::RequestBodyLimitLayer;
use tower_http::timeout::TimeoutLayer;

use crate::admission::Admission;
use crate::config::ClientConfig;
use crate::connection::{ConnectionInfo, ListenerIdentity, PlainAcceptor, Sha256, TlsAcceptor};
use crate::parse::{extract_path_params, parse_query};
use crate::{HttpConfig, Request as PyRequest, Response as PyResponse, ServerConfig, TlsConfig};

// ── Named outbound clients ───────────────────────────────────────────────
//
// The `clients:` map from the install-time config, stashed so
// `http.Client("name")` (see `client.rs`) can resolve a pre-configured,
// pooled client by name. Set once at task startup; read-only thereafter.

static NAMED_CLIENTS: OnceLock<HashMap<String, ClientConfig>> = OnceLock::new();

pub(crate) fn set_named_clients(clients: HashMap<String, ClientConfig>) {
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

pub(crate) fn set_trusted_proxies(entries: &[String]) {
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
/// `axum::Router` across all listeners, bind each `cfg.servers` entry, run any
/// `http.startup` hooks, then start accepting.
///
/// Binding happens here, before this returns, so a bad address, a port in use
/// or an unreadable TLS file is known at startup. If the script registered
/// routes, that is fatal: the process exits, the way siphon refuses to start
/// on a SIP listener it cannot bind, because a route with no listener is
/// unreachable while everything else about the process looks healthy.
pub fn spawn(cfg: HttpConfig, script: ScriptHandle) {
    spawn_with(cfg, script, None)
}

/// [`spawn`], with `admission` deciding on every request a route serves
/// before its body is read and before any middleware or handler (see
/// [`crate::admission`]).
pub fn spawn_with(cfg: HttpConfig, script: ScriptHandle, admission: Option<Arc<dyn Admission>>) {
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

    let route_count = routes.len();
    let (mut router, scopes) = build_router(routes, Arc::clone(&middlewares), &cfg, script.clone());
    if let Some(admission) = admission {
        router = guarded(router, admission);
    }

    // A route restricted to a listener no `servers[]` entry has would be
    // served nowhere. That is a typo in the script or the config, and fatal
    // for the same reason a listener that cannot bind is.
    let unknown = unknown_route_listeners(
        scopes.iter().map(|(path, scope)| (path.as_str(), scope)),
        &cfg,
    );
    if !unknown.is_empty() {
        for (path, name) in &unknown {
            tracing::error!(
                target: "siphon_http",
                route = %path,
                listener = %name,
                "http route names a listener that is not configured"
            );
            eprintln!(
                "siphon-http: route {path} is restricted to listener {name:?}, \
                 which no servers[] entry is called; refusing to start"
            );
        }
        std::process::exit(1);
    }

    let mut bound = Vec::new();
    let mut failures = 0;
    for server in &cfg.servers {
        match bind_listener(server) {
            Ok(listener) => bound.push(listener),
            Err(error) => {
                failures += 1;
                tracing::error!(target: "siphon_http", %error, "listener failed");
            }
        }
    }
    if listener_failures_are_fatal(route_count, failures) {
        tracing::error!(
            target: "siphon_http",
            routes = route_count,
            failed = failures,
            "refusing to start: http routes are registered but a listener could not bind"
        );
        eprintln!(
            "siphon-http: {failures} of {} listener(s) could not bind and the script \
             registered {route_count} http route(s); refusing to start",
            cfg.servers.len()
        );
        std::process::exit(1);
    }

    runtime.spawn(async move {
        // Startup hooks run to completion before any listener accepts.
        for h in &startups {
            if let Err(e) = script.call_handler(h, Vec::new()).await {
                tracing::error!(target: "siphon_http", error = %e, "http.startup handler raised");
            }
        }

        let mut tasks = Vec::new();
        for listener in bound {
            let router = router.clone();
            tasks.push(tokio::spawn(async move {
                let listen = listener.listen.clone();
                if let Err(e) = listener.serve(router).await {
                    tracing::error!(target: "siphon_http", listen = %listen, error = %e, "listener failed");
                }
            }));
        }
        for t in tasks {
            let _ = t.await;
        }
    });
}

// ── Router build ─────────────────────────────────────────────────────────

/// `router` with `admission` on its routes: inside the request timeout and
/// the body limit, before the body is read. A path no route has is answered
/// 404 without it.
fn guarded(router: Router, admission: Arc<dyn Admission>) -> Router {
    router.route_layer(axum::middleware::from_fn(move |request, next| {
        crate::admission::guard(Arc::clone(&admission), request, next)
    }))
}

fn build_router(
    routes: Vec<HandlerHandle>,
    middlewares: Arc<Vec<HandlerHandle>>,
    cfg: &HttpConfig,
    script: ScriptHandle,
) -> (Router, Vec<(String, RouteScope)>) {
    let mut by_path: HashMap<String, Vec<RouteEntry>> = HashMap::new();

    for handler in routes {
        let options = match Python::attach(|py| read_route_options(py, &handler)) {
            Ok(options) => options,
            Err(e) => {
                tracing::warn!(target: "siphon_http", error = %e, "skipping malformed http.route options");
                continue;
            }
        };
        if options.path.is_empty() {
            continue;
        }
        by_path.entry(options.path).or_default().push(RouteEntry {
            scope: RouteScope {
                methods: options.methods,
                listeners: options.listeners,
            },
            handler,
        });
    }

    let scopes: Vec<(String, RouteScope)> = by_path
        .iter()
        .flat_map(|(path, entries)| {
            entries
                .iter()
                .map(move |entry| (path.clone(), entry.scope.clone()))
        })
        .collect();

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
    let router =
        router
            .layer(RequestBodyLimitLayer::new(max_body))
            .layer(TimeoutLayer::with_status_code(
                StatusCode::GATEWAY_TIMEOUT,
                Duration::from_millis(timeout),
            ));
    (router, scopes)
}

/// What `@http.route(...)` registered for one handler.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct RouteOptions {
    pub(crate) path: String,
    pub(crate) methods: Vec<Method>,
    /// `listeners=[...]`: the listeners this route is served on. `None` is
    /// every listener.
    pub(crate) listeners: Option<Vec<String>>,
}

fn read_route_options(py: Python<'_>, handler: &HandlerHandle) -> PyResult<RouteOptions> {
    let opts = handler
        .options(py)
        .ok_or_else(|| pyo3::exceptions::PyValueError::new_err("http.route missing options"))?;
    route_options(&opts)
}

/// Read the options dict `python/http.py` registers with a route.
pub(crate) fn route_options(opts: &Bound<'_, pyo3::types::PyDict>) -> PyResult<RouteOptions> {
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
    // Absent from the options of a script written before listeners existed.
    let listeners = match opts.get_item("listeners")? {
        Some(value) if !value.is_none() => Some(value.extract::<Vec<String>>()?),
        _ => None,
    };
    Ok(RouteOptions {
        path,
        methods,
        listeners,
    })
}

/// Where one `@http.route` handler applies: its methods, and the listeners it
/// is restricted to, if any.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RouteScope {
    pub(crate) methods: Vec<Method>,
    pub(crate) listeners: Option<Vec<String>>,
}

impl RouteScope {
    /// Whether the route exists on the listener a request arrived on. A
    /// restricted route does not exist for a request whose listener is not
    /// known.
    fn serves_listener(&self, listener: Option<&str>) -> bool {
        match (&self.listeners, listener) {
            (None, _) => true,
            (Some(allowed), Some(name)) => allowed.iter().any(|entry| entry == name),
            (Some(_), None) => false,
        }
    }
}

struct RouteEntry {
    scope: RouteScope,
    handler: HandlerHandle,
}

/// What a path's handlers make of one request.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum RouteMatch {
    /// The handler at this index serves it.
    Handler(usize),
    /// The path has handlers on this listener, none for the method: 405.
    MethodNotAllowed,
    /// No handler of the path is served on this listener: 404, the answer
    /// for a path that does not exist.
    NotOnThisListener,
}

/// Pick the handler for a request among the ones registered for its path.
pub(crate) fn match_route(
    scopes: &[RouteScope],
    method: &Method,
    listener: Option<&str>,
) -> RouteMatch {
    let mut on_this_listener = false;
    for (index, scope) in scopes.iter().enumerate() {
        if !scope.serves_listener(listener) {
            continue;
        }
        if scope.methods.contains(method) {
            return RouteMatch::Handler(index);
        }
        on_this_listener = true;
    }
    if on_this_listener {
        RouteMatch::MethodNotAllowed
    } else {
        RouteMatch::NotOnThisListener
    }
}

/// The listener names routes ask for that no `servers[]` entry has, each
/// with the path of the route that names it.
pub(crate) fn unknown_route_listeners<'a>(
    routes: impl IntoIterator<Item = (&'a str, &'a RouteScope)>,
    cfg: &HttpConfig,
) -> Vec<(String, String)> {
    let mut unknown = Vec::new();
    for (path, scope) in routes {
        for name in scope.listeners.iter().flatten() {
            let known = cfg
                .servers
                .iter()
                .any(|server| server.listener_name() == name);
            if !known {
                unknown.push((path.to_string(), name.clone()));
            }
        }
    }
    unknown
}

fn build_method_router(
    entries: Vec<RouteEntry>,
    middlewares: Arc<Vec<HandlerHandle>>,
    script: ScriptHandle,
) -> MethodRouter {
    // Single `any` dispatcher that picks the matching handler by method at
    // request time. Lets us handle multiple `@http.route` decorators for the
    // same path with different method sets without needing axum's typed
    // `.get(...).put(...)` wiring at build time.
    //
    // Wrap the handlers in Arc: `HandlerHandle: Clone` calls `Py<T>::clone`
    // under the hood, which panics when invoked from a tokio worker that hasn't
    // called `Python::attach`. Cloning the Arc is a plain refcount bump.
    let (scopes, handlers): (Vec<RouteScope>, Vec<HandlerHandle>) = entries
        .into_iter()
        .map(|entry| (entry.scope, entry.handler))
        .unzip();
    let scopes = Arc::new(scopes);
    let handlers = Arc::new(handlers);
    any(move |req: AxumRequest| {
        let scopes = Arc::clone(&scopes);
        let handlers = Arc::clone(&handlers);
        let middlewares = Arc::clone(&middlewares);
        let script = script.clone();
        async move { dispatch(req, scopes, handlers, middlewares, script).await }
    })
}

/// Decide which of a path's handlers serves `request`, from its method and
/// the listener it arrived on.
pub(crate) fn route_gate(scopes: &[RouteScope], request: &AxumRequest) -> RouteMatch {
    let listener = request
        .extensions()
        .get::<ConnectionInfo>()
        .map(|connection| connection.listener.name.as_str());
    match_route(scopes, request.method(), listener)
}

impl RouteMatch {
    /// The response to a request no handler serves.
    pub(crate) fn refusal(&self) -> AxumResponse {
        match self {
            RouteMatch::MethodNotAllowed => {
                error_response(StatusCode::METHOD_NOT_ALLOWED, "method not allowed")
            }
            // What axum answers for a path no route has: the route does not
            // exist on this listener, and the answer does not say otherwise.
            RouteMatch::NotOnThisListener | RouteMatch::Handler(_) => {
                let mut response = AxumResponse::new(Body::empty());
                *response.status_mut() = StatusCode::NOT_FOUND;
                response
            }
        }
    }
}

// ── Dispatch ─────────────────────────────────────────────────────────────

async fn dispatch(
    req: AxumRequest,
    scopes: Arc<Vec<RouteScope>>,
    handlers: Arc<Vec<HandlerHandle>>,
    middlewares: Arc<Vec<HandlerHandle>>,
    script: ScriptHandle,
) -> AxumResponse {
    let handler = match route_gate(&scopes, &req) {
        RouteMatch::Handler(index) => match handlers.get(index) {
            Some(handler) => handler,
            None => return error_response(StatusCode::INTERNAL_SERVER_ERROR, "internal error"),
        },
        refused => return refused.refusal(),
    };

    let (parts, body) = req.into_parts();
    let body_bytes = match collect_body(body).await {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(target: "siphon_http", error = %e, "body collect failed");
            return error_response(StatusCode::BAD_REQUEST, "bad body");
        }
    };

    let py_request = request_from_parts(&parts, body_bytes.to_vec());

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

/// The `Request` a handler receives, from the head of an HTTP request and its
/// buffered body.
pub(crate) fn request_from_parts(parts: &axum::http::request::Parts, body: Vec<u8>) -> PyRequest {
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
    let socket_peer = peer.map_or_else(|| "unknown:0".to_string(), |peer| peer.to_string());
    // An HTTP/2 request carries the name in `:authority`, which hyper puts in
    // the request target; HTTP/1.1 carries it in `Host`.
    let host = parts
        .uri
        .authority()
        .map(|authority| authority.as_str().to_string())
        .or_else(|| headers.get("host").cloned());

    PyRequest::from_parts(
        parts.method.as_str().to_string(),
        parts.uri.path().to_string(),
        path_params,
        query_params,
        headers,
        body,
        client,
        socket_peer,
        host,
        parts.extensions.get::<ConnectionInfo>().cloned(),
    )
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

/// Lower-cased names; a repeated header's values joined with ", " (RFC
/// 9110 clause 5.3), for requests and for the responses of `http.Client`.
pub(crate) fn headers_to_map(headers: &HeaderMap) -> HashMap<String, String> {
    joined_headers(headers).into_iter().collect()
}

/// [`headers_to_map`] in the order the names first appear. A value that is
/// not visible ASCII is left out.
pub(crate) fn joined_headers(headers: &HeaderMap) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::with_capacity(headers.keys_len());
    for name in headers.keys() {
        let values: Vec<&str> = headers
            .get_all(name)
            .iter()
            .filter_map(|value| value.to_str().ok())
            .collect();
        if !values.is_empty() {
            out.push((name.as_str().to_ascii_lowercase(), values.join(", ")));
        }
    }
    out
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

/// Backlog for each listener once it starts accepting (tokio's default).
const LISTEN_BACKLOG: u32 = 1024;

/// A `servers[]` entry that failed to come up, and why.
#[derive(Debug, thiserror::Error)]
#[error("listener {listen}: {source}")]
pub(crate) struct ListenerError {
    listen: String,
    source: std::io::Error,
}

/// Whether listeners that failed to bind must stop the process. A listener
/// with no route behind it serves nothing but 404, so its loss is only an
/// error; once a route exists, losing a listener makes that route unreachable.
fn listener_failures_are_fatal(route_count: usize, failures: usize) -> bool {
    route_count > 0 && failures > 0
}

/// A socket bound to its address but not yet listening, plus its TLS config.
///
/// Splitting `bind` from `listen` surfaces a port conflict at startup while
/// still refusing connections until the `http.startup` hooks have finished, so
/// a TCP readiness probe does not pass early.
pub(crate) struct BoundListener {
    listen: String,
    /// What routes and handlers call this listener.
    name: String,
    socket: tokio::net::TcpSocket,
    tls: Option<(axum_server::tls_rustls::RustlsConfig, Sha256, TlsConfig)>,
}

/// Parse the address, load any TLS material, and bind the socket.
pub(crate) fn bind_listener(server: &ServerConfig) -> Result<BoundListener, ListenerError> {
    let fail = |source| ListenerError {
        listen: server.listen.clone(),
        source,
    };
    let addr = server.socket_addr().map_err(|e| fail(to_io(e)))?;
    let tls = server
        .tls
        .as_ref()
        .map(|settings| {
            build_rustls_config(settings).map(|(config, sha256)| (config, sha256, settings.clone()))
        })
        .transpose()
        .map_err(fail)?;

    let socket = if addr.is_ipv4() {
        tokio::net::TcpSocket::new_v4()
    } else {
        tokio::net::TcpSocket::new_v6()
    }
    .map_err(fail)?;
    // Same as std's TcpListener::bind: allow a restart while old connections
    // sit in TIME_WAIT. Not on Windows, where it would allow port stealing.
    #[cfg(unix)]
    socket.set_reuseaddr(true).map_err(fail)?;
    if let Some(dscp) = server.dscp {
        if !addr.is_ipv4() {
            return Err(fail(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "dscp is set on IPv4 listeners only",
            )));
        }
        // The DS field is the upper six bits of the former TOS octet
        // (RFC 2474 clause 3).
        socket.set_tos_v4(u32::from(dscp.0) << 2).map_err(fail)?;
    }
    socket.bind(addr).map_err(fail)?;

    Ok(BoundListener {
        listen: server.listen.clone(),
        name: server.listener_name().to_string(),
        socket,
        tls,
    })
}

impl BoundListener {
    #[cfg(test)]
    pub(crate) fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.socket.local_addr()
    }

    /// The TOS octet of the listening socket.
    #[cfg(test)]
    pub(crate) fn tos(&self) -> std::io::Result<u32> {
        self.socket.tos_v4()
    }

    /// Start listening and serve `router` until the server stops.
    pub(crate) async fn serve(self, router: Router) -> std::io::Result<()> {
        let listener = self.socket.listen(LISTEN_BACKLOG)?.into_std()?;
        let addr = listener.local_addr()?;
        let app = router.into_make_service_with_connect_info::<SocketAddr>();

        // Each listener has an acceptor of its own, which tells every request
        // which listener it came through and, over TLS, in which session.
        let identity = Arc::new(ListenerIdentity {
            name: self.name,
            tls: self.tls.is_some(),
        });
        let server = axum_server::from_tcp(listener)?;
        if let Some((config, sha256, settings)) = self.tls {
            tracing::info!(target: "siphon_http", listen = %addr, name = %identity.name, tls = true, "serving HTTPS listener");
            if let Some(seconds) = settings.reload_seconds.filter(|seconds| *seconds > 0) {
                tokio::spawn(reload_listener(config.clone(), settings, seconds, addr));
            }
            let acceptor = TlsAcceptor {
                listener: identity,
                inner: axum_server::tls_rustls::RustlsAcceptor::new(config),
                sha256,
            };
            server.acceptor(acceptor).serve(app).await?;
        } else {
            tracing::info!(target: "siphon_http", listen = %addr, name = %identity.name, tls = false, "serving HTTP listener");
            let acceptor = PlainAcceptor { listener: identity };
            server.acceptor(acceptor).serve(app).await?;
        }
        Ok(())
    }
}

/// The modification times of a listener's TLS files.
fn listener_stamps(tls: &TlsConfig) -> Vec<Option<std::time::SystemTime>> {
    [
        Some(&tls.cert_path),
        Some(&tls.key_path),
        tls.client_ca.as_ref(),
    ]
    .into_iter()
    .map(|path| {
        path.and_then(|path| std::fs::metadata(path).ok())
            .and_then(|metadata| metadata.modified().ok())
    })
    .collect()
}

/// Every `seconds`, when a TLS file of the listener changed, serve new
/// connections with the files as they are now (`tls.reload_seconds`).
async fn reload_listener(
    config: axum_server::tls_rustls::RustlsConfig,
    tls: TlsConfig,
    seconds: u64,
    addr: SocketAddr,
) {
    let mut seen = listener_stamps(&tls);
    let mut interval = tokio::time::interval(Duration::from_secs(seconds));
    interval.tick().await;
    loop {
        interval.tick().await;
        let now = listener_stamps(&tls);
        if now == seen {
            continue;
        }
        seen = now;
        match build_server_config(&tls) {
            Ok((server, _)) => {
                config.reload_from_config(server);
                tracing::info!(target: "siphon_http", listen = %addr, "listener TLS files changed: new connections use them");
            }
            Err(error) => tracing::error!(
                target: "siphon_http",
                listen = %addr,
                %error,
                "listener TLS files changed but do not load; the previous ones stay"
            ),
        }
    }
}

/// Build the TLS config for a listener.
///
/// Loads the server cert chain + key, wires client-certificate verification
/// when `client_ca` is set (mutual TLS), and advertises ALPN `h2, http/1.1` so
/// HTTP/2 negotiates over TLS. Pins the ring crypto provider explicitly to
/// avoid ambiguity when more than one provider is linked.
fn build_rustls_config(
    tls: &TlsConfig,
) -> std::io::Result<(axum_server::tls_rustls::RustlsConfig, Sha256)> {
    let (config, sha256) = build_server_config(tls)?;
    Ok((
        axum_server::tls_rustls::RustlsConfig::from_config(config),
        sha256,
    ))
}

/// The rustls server configuration of `tls`, read from its files now.
fn build_server_config(tls: &TlsConfig) -> std::io::Result<(Arc<RustlsServerConfig>, Sha256)> {
    let certs = load_certs(&tls.cert_path)?;
    let key = load_key(&tls.key_path)?;

    let provider = Arc::new(rustls::crypto::ring::default_provider());
    // For the fingerprint of a client certificate (`req.tls`).
    let sha256 = Sha256::from_provider(&provider)?;
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

    Ok((Arc::new(config), sha256))
}

fn load_certs(path: &str) -> std::io::Result<Vec<CertificateDer<'static>>> {
    let file = std::fs::File::open(path).map_err(|e| {
        std::io::Error::new(std::io::ErrorKind::NotFound, format!("open {path}: {e}"))
    })?;
    CertificateDer::pem_reader_iter(file)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| pem_to_io("certificate", path, error))
}

fn load_key(path: &str) -> std::io::Result<PrivateKeyDer<'static>> {
    let file = std::fs::File::open(path).map_err(|e| {
        std::io::Error::new(std::io::ErrorKind::NotFound, format!("open {path}: {e}"))
    })?;
    PrivateKeyDer::from_pem_reader(file).map_err(|error| pem_to_io("private key", path, error))
}

/// Map a `rustls-pki-types` PEM error onto the `io::Error` shape the TLS startup
/// path reports, keeping the messages operators already see in the logs.
fn pem_to_io(what: &str, path: &str, error: pem::Error) -> std::io::Error {
    match error {
        pem::Error::Io(error) => error,
        pem::Error::NoItemsFound => std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("no {what} in {path}"),
        ),
        other => std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("parse {what} from {path}: {other}"),
        ),
    }
}

fn to_io<E: std::fmt::Display>(e: E) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidInput, e.to_string())
}

#[cfg(test)]
mod tests {
    /// RFC 2474 codepoints by number or by the names of RFC 2474 (CS),
    /// RFC 2597 (AF), RFC 3246 (EF), RFC 5865 (VA) and RFC 8622 (LE), set
    /// as the upper six bits of the listening socket's TOS octet; refused
    /// out of range and on IPv6.
    #[test]
    fn a_listener_marks_with_its_dscp() {
        use crate::config::Dscp;
        for (name, value) in [
            ("ef", 46),
            ("EF", 46),
            ("af11", 10),
            ("af12", 12),
            ("af13", 14),
            ("af21", 18),
            ("af31", 26),
            ("af41", 34),
            ("af43", 38),
            ("cs0", 0),
            ("cs3", 24),
            ("cs6", 48),
            ("cs7", 56),
            ("va", 44),
            ("le", 1),
            ("df", 0),
        ] {
            assert_eq!(Dscp::named(name), Some(Dscp(value)), "{name}");
        }
        for name in ["af14", "af51", "af10", "cs8", "ef1", "x"] {
            assert_eq!(Dscp::named(name), None, "{name}");
        }
        let yaml = |dscp: &str, listen: &str| {
            crate::HttpConfig::from_yaml(&format!(
                "servers:\n  - listen: \"{listen}\"\n    dscp: {dscp}\n"
            ))
        };
        for (dscp, tos) in [("ef", 184), ("34", 136), ("cs0", 0)] {
            let config = yaml(dscp, "127.0.0.1:0").unwrap();
            let listener = super::bind_listener(&config.servers[0]).unwrap();
            assert_eq!(listener.tos().unwrap(), tos, "{dscp}");
        }
        assert!(yaml("64", "127.0.0.1:0").is_err());
        assert!(yaml("af99", "127.0.0.1:0").is_err());
        let six = yaml("ef", "[::1]:0").unwrap();
        assert!(super::bind_listener(&six.servers[0]).is_err());
    }

    #[test]
    fn repeated_headers_are_joined_in_order() {
        let mut headers = super::HeaderMap::new();
        headers.append("via", "2.0 SEPP-a.example".parse().unwrap());
        headers.append("Via", "2.0 SCP-b.example".parse().unwrap());
        headers.insert("content-type", "application/json".parse().unwrap());
        let map = super::headers_to_map(&headers);
        assert_eq!(map["via"], "2.0 SEPP-a.example, 2.0 SCP-b.example");
        assert_eq!(map["content-type"], "application/json");
    }

    use super::{
        bind_listener, listener_failures_are_fatal, load_certs, load_key, pem_to_io,
        RustlsServerConfig,
    };
    use crate::{ServerConfig, TlsConfig};
    use rustls::pki_types::pem;
    use rustls::pki_types::PrivateKeyDer;
    use std::io::ErrorKind;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    /// A temp file that deletes itself, so these tests need no dev-dependency.
    struct TempPem(PathBuf);

    impl TempPem {
        fn new(contents: &str) -> Self {
            static COUNTER: AtomicUsize = AtomicUsize::new(0);
            let path = std::env::temp_dir().join(format!(
                "siphon-http-pem-{}-{}.pem",
                std::process::id(),
                COUNTER.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::write(&path, contents).unwrap();
            Self(path)
        }

        fn path(&self) -> &str {
            self.0.to_str().unwrap()
        }
    }

    impl Drop for TempPem {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    /// The loaders decode PEM framing; they do not validate X.509 / DER
    /// contents, so a section body only has to be valid base64.
    fn section(kind: &str, body: &str) -> String {
        format!("-----BEGIN {kind}-----\n{body}\n-----END {kind}-----\n")
    }

    #[test]
    fn certs_are_loaded_in_file_order() {
        let file = TempPem::new(&format!(
            "{}{}",
            section("CERTIFICATE", "AQID"),
            section("CERTIFICATE", "BAUG")
        ));
        let certs = load_certs(file.path()).unwrap();
        assert_eq!(certs.len(), 2);
        assert_eq!(certs[0].as_ref(), &[1, 2, 3]);
        assert_eq!(certs[1].as_ref(), &[4, 5, 6]);
    }

    #[test]
    fn certs_skip_non_certificate_sections() {
        // A combined cert+key PEM is common; load_certs must ignore the key
        // rather than fail (parity with the previous rustls-pemfile behaviour).
        let file = TempPem::new(&format!(
            "{}{}{}",
            section("PRIVATE KEY", "AQID"),
            section("CERTIFICATE", "BAUG"),
            section("EC PARAMETERS", "BwgJ")
        ));
        let certs = load_certs(file.path()).unwrap();
        assert_eq!(certs.len(), 1);
        assert_eq!(certs[0].as_ref(), &[4, 5, 6]);
    }

    #[test]
    fn certs_empty_file_is_not_an_error() {
        // with_single_cert is what rejects an empty chain; loading must not.
        let file = TempPem::new("# no PEM sections here\n");
        assert!(load_certs(file.path()).unwrap().is_empty());
    }

    #[test]
    fn certs_missing_file_is_not_found() {
        let missing = Path::new(&std::env::temp_dir()).join("siphon-http-absent.pem");
        let error = load_certs(missing.to_str().unwrap()).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::NotFound);
        assert!(error.to_string().contains("open "));
    }

    #[test]
    fn certs_malformed_base64_reports_the_path() {
        let file = TempPem::new(&section("CERTIFICATE", "not!valid!base64!"));
        let error = load_certs(file.path()).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::InvalidInput);
        assert!(error.to_string().contains("parse certificate from"));
        assert!(error.to_string().contains(file.path()));
    }

    #[test]
    fn key_loads_pkcs8() {
        let file = TempPem::new(&section("PRIVATE KEY", "AQID"));
        assert!(matches!(
            load_key(file.path()).unwrap(),
            PrivateKeyDer::Pkcs8(_)
        ));
    }

    #[test]
    fn key_loads_pkcs1() {
        let file = TempPem::new(&section("RSA PRIVATE KEY", "AQID"));
        assert!(matches!(
            load_key(file.path()).unwrap(),
            PrivateKeyDer::Pkcs1(_)
        ));
    }

    #[test]
    fn key_loads_sec1() {
        let file = TempPem::new(&section("EC PRIVATE KEY", "AQID"));
        assert!(matches!(
            load_key(file.path()).unwrap(),
            PrivateKeyDer::Sec1(_)
        ));
    }

    #[test]
    fn key_skips_leading_certificate_section() {
        let file = TempPem::new(&format!(
            "{}{}",
            section("CERTIFICATE", "BAUG"),
            section("PRIVATE KEY", "AQID")
        ));
        let key = load_key(file.path()).unwrap();
        assert!(matches!(&key, PrivateKeyDer::Pkcs8(_)));
        assert_eq!(key.secret_der(), &[1, 2, 3]);
    }

    #[test]
    fn key_absent_reports_no_private_key() {
        let file = TempPem::new(&section("CERTIFICATE", "AQID"));
        let error = load_key(file.path()).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::InvalidInput);
        assert!(error.to_string().contains("no private key in"));
        assert!(error.to_string().contains(file.path()));
    }

    #[test]
    fn key_missing_file_is_not_found() {
        let missing = Path::new(&std::env::temp_dir()).join("siphon-http-absent-key.pem");
        assert_eq!(
            load_key(missing.to_str().unwrap()).unwrap_err().kind(),
            ErrorKind::NotFound
        );
    }

    #[test]
    fn pem_io_errors_pass_through_unwrapped() {
        let inner = std::io::Error::new(ErrorKind::PermissionDenied, "denied");
        let mapped = pem_to_io("certificate", "/tmp/x.pem", pem::Error::Io(inner));
        assert_eq!(mapped.kind(), ErrorKind::PermissionDenied);
    }

    // A throwaway self-signed P-256 cert for `example.com` / 127.0.0.1, valid
    // until 2126, together with its key in both PKCS#8 and SEC1 encodings. The
    // framing tests above use fake section bodies on purpose; these are real DER
    // so that rustls itself checks the loaders, rather than a round-trip through
    // our own code.
    const REAL_CERT_PEM: &str = "\
-----BEGIN CERTIFICATE-----\n\
MIIBojCCAUegAwIBAgIUMK9ymiTuCf2w4huzkk+v+Z4BnX0wCgYIKoZIzj0EAwIw\n\
FjEUMBIGA1UEAwwLZXhhbXBsZS5jb20wIBcNMjYwODE4MDcwNTA5WhgPMjEyNjA3\n\
MjUwNzA1MDlaMBYxFDASBgNVBAMMC2V4YW1wbGUuY29tMFkwEwYHKoZIzj0CAQYI\n\
KoZIzj0DAQcDQgAEQsM0yzEzLsLgrZGUozDHeC/4ZbnMQxA3w+zqPh+58buq/1jR\n\
dhQXGOGGj2W2oE4Z0ETjIVryRGzZgKSWuJOtFaNxMG8wHQYDVR0OBBYEFCq3PZqr\n\
UKt+AwA3ajA4pW2+xrXoMB8GA1UdIwQYMBaAFCq3PZqrUKt+AwA3ajA4pW2+xrXo\n\
MA8GA1UdEwEB/wQFMAMBAf8wHAYDVR0RBBUwE4ILZXhhbXBsZS5jb22HBH8AAAEw\n\
CgYIKoZIzj0EAwIDSQAwRgIhAL940V2kbB7+i+1JoSmoLF2vcfEy2+E58zLaz/xz\n\
6zZmAiEAij+fuGdHIUc4hjFxWZ5cP0os+bTmoXTEjWoAcug9uVE=\n\
-----END CERTIFICATE-----\n\
";

    const REAL_KEY_PKCS8_PEM: &str = "\
-----BEGIN PRIVATE KEY-----\n\
MIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQgtKcpHfQWCT78MdtW\n\
umwRxA2YHMgDT/0brTs9cpzFtV2hRANCAARCwzTLMTMuwuCtkZSjMMd4L/hlucxD\n\
EDfD7Oo+H7nxu6r/WNF2FBcY4YaPZbagThnQROMhWvJEbNmApJa4k60V\n\
-----END PRIVATE KEY-----\n\
";

    const REAL_KEY_SEC1_PEM: &str = "\
-----BEGIN EC PRIVATE KEY-----\n\
MHcCAQEEILSnKR30Fgk+/DHbVrpsEcQNmBzIA0/9G607PXKcxbVdoAoGCCqGSM49\n\
AwEHoUQDQgAEQsM0yzEzLsLgrZGUozDHeC/4ZbnMQxA3w+zqPh+58buq/1jRdhQX\n\
GOGGj2W2oE4Z0ETjIVryRGzZgKSWuJOtFQ==\n\
-----END EC PRIVATE KEY-----\n\
";

    /// Feed a real cert/key pair through the loaders into rustls. `with_single_cert`
    /// parses the DER and checks the key matches the certificate's public key, so
    /// this fails if the migration mangles or mislabels either one.
    fn assert_builds_rustls_config(key_pem: &str) {
        let cert_file = TempPem::new(REAL_CERT_PEM);
        let key_file = TempPem::new(key_pem);

        let certs = load_certs(cert_file.path()).unwrap();
        let key = load_key(key_file.path()).unwrap();
        assert_eq!(certs.len(), 1);

        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let config = RustlsServerConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(certs, key);
        assert!(
            config.is_ok(),
            "rustls rejected the loaded pair: {config:?}"
        );
    }

    #[test]
    fn real_cert_with_pkcs8_key_builds_a_rustls_config() {
        assert_builds_rustls_config(REAL_KEY_PKCS8_PEM);
    }

    #[test]
    fn real_cert_with_sec1_key_builds_a_rustls_config() {
        // Same key, SEC1-encoded: proves the Sec1 branch of load_key hands rustls
        // something it can actually use, not just a correctly-tagged blob.
        assert_builds_rustls_config(REAL_KEY_SEC1_PEM);
    }

    #[test]
    fn real_cert_is_loaded_as_a_single_der_certificate() {
        // DER always starts with a SEQUENCE tag; a base64 slip would not.
        let file = TempPem::new(REAL_CERT_PEM);
        let certs = load_certs(file.path()).unwrap();
        assert_eq!(certs.len(), 1);
        assert_eq!(certs[0].as_ref()[0], 0x30);
        assert!(certs[0].as_ref().len() > 300);
    }

    fn server(listen: &str) -> ServerConfig {
        crate::HttpConfig::from_yaml(&format!("servers:\n  - listen: \"{listen}\"\n"))
            .unwrap()
            .servers
            .remove(0)
    }

    #[test]
    fn bind_reports_a_port_already_in_use_with_the_address() {
        let holder = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let listen = holder.local_addr().unwrap().to_string();
        let error = match bind_listener(&server(&listen)) {
            Ok(_) => panic!("second bind on {listen} succeeded"),
            Err(error) => error,
        };
        assert_eq!(error.source.kind(), ErrorKind::AddrInUse);
        assert!(error.to_string().contains(&listen), "{error}");
    }

    #[test]
    fn bind_reports_an_unparsable_address_built_in_code() {
        // from_yaml rejects this; a config built directly must fail at bind too.
        let mut config = server("127.0.0.1:0");
        config.listen = "127.0.0.1:${HTTP_PORT}".to_string();
        let error = match bind_listener(&config) {
            Ok(_) => panic!("unparsable address bound"),
            Err(error) => error,
        };
        assert_eq!(error.source.kind(), ErrorKind::InvalidInput);
        assert!(error.to_string().contains("${HTTP_PORT}"), "{error}");
    }

    #[test]
    fn bind_reports_a_missing_tls_certificate() {
        let mut config = server("127.0.0.1:0");
        config.tls = Some(TlsConfig {
            reload_seconds: None,
            cert_path: "/nonexistent/siphon-http/server.crt".to_string(),
            key_path: "/nonexistent/siphon-http/server.key".to_string(),
            client_ca: None,
        });
        let error = match bind_listener(&config) {
            Ok(_) => panic!("listener bound without its certificate"),
            Err(error) => error,
        };
        assert_eq!(error.source.kind(), ErrorKind::NotFound);
        assert!(error.to_string().contains("server.crt"), "{error}");
    }

    #[tokio::test]
    async fn a_bound_listener_accepts_nothing_until_it_serves() {
        // Startup hooks run between bind and serve; readiness probes must not
        // see an open socket in that window.
        let bound = bind_listener(&server("127.0.0.1:0")).unwrap();
        let addr = bound.local_addr().unwrap();
        let refused = tokio::net::TcpStream::connect(addr).await.unwrap_err();
        assert_eq!(refused.kind(), ErrorKind::ConnectionRefused);

        let router = axum::Router::new().route("/ping", axum::routing::get(|| async { "pong" }));
        tokio::spawn(async move { bound.serve(router).await });

        let mut body = None;
        for _ in 0..50 {
            if let Ok(response) = reqwest::get(format!("http://{addr}/ping")).await {
                body = Some(response.text().await.unwrap());
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert_eq!(body.as_deref(), Some("pong"));
    }

    #[test]
    fn a_failed_listener_is_fatal_only_when_routes_need_it() {
        assert!(listener_failures_are_fatal(1, 1));
        assert!(listener_failures_are_fatal(3, 2));
        assert!(!listener_failures_are_fatal(0, 1));
        assert!(!listener_failures_are_fatal(2, 0));
    }
}
