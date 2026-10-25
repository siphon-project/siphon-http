//! `siphon-http` — HTTP/HTTPS addon for the siphon scripting platform.
//!
//! This crate is a **siphon addon**, not a standalone server. It plugs an
//! `http` Python namespace into a siphon binary so user scripts can handle
//! inbound HTTP the same way they handle SIP, and call out over HTTP from
//! inside the same asyncio loop:
//!
//! ```python
//! from siphon import http
//!
//! @http.route("/hello/{name}", methods=["GET"])
//! async def hello(req):
//!     return http.Response(status=200, body=f"hi {req.path_params['name']}".encode())
//! ```
//!
//! ## Hooks
//!
//! A composing binary wires two paired hooks at startup — [`namespace`] and
//! [`task`]. The namespace exposes `siphon.http` to scripts; the task spawns
//! the axum listener(s) + outbound client pool against the script's
//! registered routes. Both are needed: the namespace alone is inert, the task
//! alone has no handlers to dispatch to.
//!
//! ## Config
//!
//! [`HttpConfig`] is loaded from a separate YAML file referenced by siphon's
//! main config (`extensions.http = "http.yaml"`). Keeping addon config out of
//! siphon's main schema means siphon doesn't need to know each addon's options
//! at compile time. See [`config`].
//!
//! ## What stays in Rust vs. Python
//!
//! Rust handles: TCP/TLS termination, HTTP/1.1 framing, path routing, capped
//! body buffering, the outbound connection pool. Python handles: request
//! dispatch, auth, content negotiation, business logic, response building.

// The test harnesses drive tokio runtimes from the test thread, which never
// attaches (`#[tokio::test]` itself expands to `Runtime::block_on`). The
// library proper is still linted against parking waits in the non-test build.
#![cfg_attr(test, allow(clippy::disallowed_methods))]

pub mod client;
pub mod config;
pub mod connection;
#[cfg(test)]
mod connection_tests;
mod dispatch;
#[cfg(test)]
mod dispatch_tests;
pub mod install;
pub mod outbound;
#[cfg(test)]
mod outbound_tests;
pub mod parse;
pub mod request;
pub mod response;
pub mod runtime;
mod x509;

pub use client::Client;
pub use config::{ClientConfig, HttpConfig, ServerConfig, TlsConfig};
pub use connection::{Listener, PeerCertificate, TlsInfo};
pub use install::{namespace, task};
pub use outbound::{ClientError, HttpClient, Reply};
pub use request::Request;
pub use response::Response;

#[derive(Debug, thiserror::Error)]
pub enum InstallError {
    #[error("python error: {0}")]
    Python(String),
    #[error("siphon namespace registration: {0}")]
    Siphon(String),
    #[error("config: {0}")]
    Config(#[from] config::ConfigError),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

/// Hooks for `benches/dispatch.rs`. Not part of the API.
#[doc(hidden)]
pub mod __bench {
    use std::sync::Arc;

    use axum::http::Method;
    use pyo3::prelude::*;

    use crate::dispatch::Stage;
    use crate::runtime::RouteOptions;

    /// A router serving `GET /bench` with `handler` behind `middlewares`, each
    /// a `(function, is_async)` pair.
    pub fn router(
        middlewares: Vec<(Py<PyAny>, bool)>,
        handler: (Py<PyAny>, bool),
    ) -> PyResult<axum::Router> {
        let route = RouteOptions {
            path: "/bench".to_string(),
            methods: vec![Method::GET],
            listeners: None,
        };
        let middlewares = middlewares
            .into_iter()
            .map(|(function, is_async)| Stage::new(function, is_async))
            .collect();
        let (router, _) = crate::runtime::build_router(
            vec![(route, Stage::new(handler.0, handler.1))],
            Arc::new(middlewares),
            &crate::HttpConfig::default(),
        )?;
        Ok(router)
    }

    /// The `Request` a handler receives for these parts.
    pub fn request(parts: &axum::http::request::Parts, body: Vec<u8>) -> crate::Request {
        crate::runtime::request_from_parts(parts, body)
    }

    /// The HTTP response for a handler's `Response`.
    pub fn response(response: &crate::Response) -> axum::response::Response {
        crate::dispatch::build_axum_response(response)
    }
}
