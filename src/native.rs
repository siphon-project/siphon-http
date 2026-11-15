//! Routes served by Rust handlers.
//!
//! A route a script registers with `@http.route` runs its handler in Python.
//! A native route runs a Rust `async fn` on the tokio task that accepted the
//! request instead, and never touches the interpreter: no executor job, no
//! attach, no asyncio. Use it for a route whose handler is plumbing nobody
//! scripts and that carries the traffic, and keep scripted routes for
//! everything an operator may want to change.
//!
//! Native and scripted routes share one router, so a native route gets the
//! same path matching, `listeners` scoping, 404/405 answers, route limits
//! (`limits`), body cap, request timeout, `Retry-After` and timing metrics.
//! Where a native route and a scripted one serve the same path, method and
//! listener, the native route answers and the script's handler is not called;
//! that is reported at startup.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use axum::body::{Body, Bytes};
use axum::http::{HeaderName, HeaderValue, Method, StatusCode};
use axum::response::Response as AxumResponse;

use crate::Request;

/// What a native handler answers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeResponse {
    pub status: u16,
    /// Header fields, in order. A name or value that is not a valid HTTP
    /// field is left out.
    pub headers: Vec<(String, String)>,
    pub body: Bytes,
}

impl NativeResponse {
    /// A response with `status`, no header fields and an empty body.
    pub fn new(status: u16) -> Self {
        Self {
            status,
            headers: Vec::new(),
            body: Bytes::new(),
        }
    }

    /// Add a header field.
    pub fn header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }

    /// Set the body.
    pub fn body(mut self, body: impl Into<Bytes>) -> Self {
        self.body = body.into();
        self
    }

    pub(crate) fn into_response(self) -> AxumResponse {
        let status = StatusCode::from_u16(self.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        let mut builder = AxumResponse::builder().status(status);
        for (name, value) in &self.headers {
            if let (Ok(name), Ok(value)) = (
                HeaderName::from_bytes(name.as_bytes()),
                HeaderValue::from_str(value),
            ) {
                builder = builder.header(name, value);
            }
        }
        builder.body(Body::from(self.body)).unwrap_or_else(|_| {
            crate::dispatch::error_response(StatusCode::INTERNAL_SERVER_ERROR, "build failed")
        })
    }
}

/// The future a native handler returns.
pub type NativeFuture = Pin<Box<dyn Future<Output = NativeResponse> + Send + 'static>>;

/// A native handler: the request in, the response out. It runs on a tokio
/// worker, so it must not block and must not attach to the interpreter.
pub type NativeHandler = Arc<dyn Fn(Request) -> NativeFuture + Send + Sync + 'static>;

/// One native route.
#[derive(Clone)]
pub(crate) struct NativeRoute {
    pub(crate) path: String,
    pub(crate) methods: Vec<Method>,
    /// The listeners the route is served on; `None` is every listener.
    pub(crate) listeners: Option<Vec<String>>,
    pub(crate) handler: NativeHandler,
}

/// The native routes a composing binary hands to the HTTP runtime.
#[derive(Clone, Default)]
pub struct NativeRoutes {
    pub(crate) routes: Vec<NativeRoute>,
}

impl NativeRoutes {
    pub fn new() -> Self {
        Self::default()
    }

    /// Serve `methods` of `path` (the same `{name}` placeholders as
    /// `@http.route`) with `handler`, on `listeners` (None: every listener).
    /// A method that is not a valid HTTP method is an error.
    pub fn route<F, Fut>(
        mut self,
        path: &str,
        methods: &[&str],
        listeners: Option<&[&str]>,
        handler: F,
    ) -> Result<Self, NativeRouteError>
    where
        F: Fn(Request) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = NativeResponse> + Send + 'static,
    {
        if !path.starts_with('/') {
            return Err(NativeRouteError::Path(path.to_string()));
        }
        let methods = methods
            .iter()
            .map(|method| {
                Method::from_bytes(method.to_ascii_uppercase().as_bytes())
                    .map_err(|_| NativeRouteError::Method((*method).to_string()))
            })
            .collect::<Result<Vec<_>, _>>()?;
        if methods.is_empty() {
            return Err(NativeRouteError::NoMethod(path.to_string()));
        }
        self.routes.push(NativeRoute {
            path: path.to_string(),
            methods,
            listeners: listeners.map(|names| names.iter().map(|name| name.to_string()).collect()),
            handler: Arc::new(move |request| Box::pin(handler(request))),
        });
        Ok(self)
    }

    pub fn is_empty(&self) -> bool {
        self.routes.is_empty()
    }
}

/// Why a native route was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum NativeRouteError {
    #[error("native route path {0:?} does not start with '/'")]
    Path(String),
    #[error("native route method {0:?} is not an HTTP method")]
    Method(String),
    #[error("native route {0:?} names no method")]
    NoMethod(String),
}

/// Rust access to the parts of a [`Request`] that scripts read through
/// methods.
impl Request {
    /// The buffered request body.
    pub fn body_bytes(&self) -> &[u8] {
        &self.body_bytes
    }

    /// A header field by name, case-insensitive.
    pub fn header_value(&self, name: &str) -> Option<&str> {
        self.headers
            .get(&name.to_ascii_lowercase())
            .map(String::as_str)
    }

    /// The path parameters of the matched route.
    pub fn path_param(&self, name: &str) -> Option<&str> {
        self.path_params.get(name).map(String::as_str)
    }

    /// The query parameters.
    pub fn query(&self) -> &HashMap<String, String> {
        &self.query_params
    }

    /// Whether the request arrived over TLS on one of the addon's
    /// listeners (what `request.tls is not None` tells a script).
    pub fn is_tls(&self) -> bool {
        self.connection
            .as_ref()
            .is_some_and(|connection| connection.tls.is_some())
    }
}
