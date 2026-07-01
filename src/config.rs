//! HTTP addon configuration.
//!
//! Loaded from a separate file referenced by siphon's main config so that
//! siphon-core doesn't need to know about HTTP-shaped options at compile
//! time. Convention:
//!
//! ```yaml
//! # in siphon.yaml
//! extensions:
//!   http: http.yaml
//! ```
//!
//! The addon reads its own file; siphon's main config parser only cares that
//! the `extensions.http` value is a path string. See [`HttpConfig`].

use std::collections::HashMap;
use std::path::Path;

use serde::Deserialize;

#[derive(Debug, Deserialize, Clone, Default)]
pub struct HttpConfig {
    /// Inbound listeners. Multiple are allowed (e.g. one HTTPS listener for
    /// the public API, one plain HTTP on localhost for `/metrics`).
    #[serde(default)]
    pub servers: Vec<ServerConfig>,

    /// Named outbound clients. The script side instantiates them by name
    /// (`http.Client("default")`) or constructs ad-hoc clients with inline
    /// params; named clients are for shared connection pools.
    #[serde(default)]
    pub clients: HashMap<String, ClientConfig>,
}

/// A single inbound listener.
///
/// Each listener auto-negotiates the HTTP version: over TLS via ALPN
/// (`h2` preferred, `http/1.1` fallback), and on cleartext by detecting the
/// HTTP/2 connection preface (h2c prior knowledge) — otherwise HTTP/1.1. No
/// per-listener switch is required to accept HTTP/2.
#[derive(Debug, Deserialize, Clone)]
pub struct ServerConfig {
    /// `host:port`, e.g. `"0.0.0.0:443"`.
    pub listen: String,

    /// Optional TLS config. Without it, the server speaks plain HTTP — fine
    /// for a localhost-only metrics endpoint, never for external listeners.
    #[serde(default)]
    pub tls: Option<TlsConfig>,

    /// Maximum request body size in bytes. Bodies larger than this get 413.
    /// Default: 1 MiB.
    #[serde(default = "default_max_body")]
    pub max_body_bytes: usize,

    /// Per-request handler timeout in milliseconds. Hit this and the server
    /// returns 504. Default: 30s.
    #[serde(default = "default_request_timeout")]
    pub request_timeout_ms: u64,

    /// IP addresses trusted to set `X-Forwarded-For` (exact match; CIDR ranges
    /// are not supported and are ignored with a warning). When a request's
    /// socket peer is listed here, its left-most `X-Forwarded-For` entry is
    /// reported to scripts as the client address. Empty = socket peer only.
    #[serde(default)]
    pub trusted_proxies: Vec<String>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct TlsConfig {
    pub cert_path: String,
    pub key_path: String,

    /// CA bundle for verifying client certificates. Presence enables mutual
    /// TLS; absence disables it.
    #[serde(default)]
    pub client_ca: Option<String>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct ClientConfig {
    /// Optional base URL prepended to relative request paths.
    #[serde(default)]
    pub base_url: Option<String>,

    /// Per-request timeout in milliseconds. Default: 5s.
    #[serde(default = "default_client_timeout")]
    pub timeout_ms: u64,

    /// CA bundle for verifying server certs. Empty = system roots.
    #[serde(default)]
    pub verify: Option<String>,

    /// Client cert + key for mutual TLS (optional).
    #[serde(default)]
    pub cert: Option<String>,
    #[serde(default)]
    pub key: Option<String>,

    /// Connection pool size. Default: 8.
    #[serde(default = "default_pool_size")]
    pub pool_size: usize,

    /// Speak HTTP/2 with prior knowledge — skip the HTTP/1 `Upgrade` dance and,
    /// on TLS, ALPN negotiation, opening every connection directly as HTTP/2.
    /// Needed to reach cleartext peers that expect HTTP/2 immediately. Default:
    /// off, i.e. negotiate the version per scheme (ALPN on TLS, HTTP/1 on
    /// cleartext).
    #[serde(default)]
    pub http2_prior_knowledge: bool,
}

fn default_max_body() -> usize {
    1 << 20
} // 1 MiB
fn default_request_timeout() -> u64 {
    30_000
}
fn default_client_timeout() -> u64 {
    5_000
}
fn default_pool_size() -> usize {
    8
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("read {path}: {source}")]
    Read {
        path: String,
        source: std::io::Error,
    },
    #[error("parse {path}: {source}")]
    Parse {
        path: String,
        source: serde_yaml::Error,
    },
}

impl HttpConfig {
    pub fn from_file(path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        let path = path.as_ref();
        let raw = std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
            path: path.display().to_string(),
            source,
        })?;
        serde_yaml::from_str(&raw).map_err(|source| ConfigError::Parse {
            path: path.display().to_string(),
            source,
        })
    }

    /// Parse config from a YAML string (used by tests/benches and callers that
    /// already have the document in memory).
    pub fn from_yaml(s: &str) -> Result<Self, ConfigError> {
        serde_yaml::from_str(s).map_err(|source| ConfigError::Parse {
            path: "<str>".to_string(),
            source,
        })
    }
}
