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

pub mod client;
pub mod config;
pub mod install;
pub mod parse;
pub mod request;
pub mod response;
pub mod runtime;

pub use client::Client;
pub use config::{ClientConfig, HttpConfig, ServerConfig, TlsConfig};
pub use install::{namespace, task};
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
