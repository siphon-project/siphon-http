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
//!
//! The file's text goes through siphon's `${VAR}` / `${VAR:-default}`
//! environment expansion before it is parsed, the same as `siphon.yaml`.

use std::collections::HashMap;
use std::net::{AddrParseError, SocketAddr};
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

impl ServerConfig {
    /// `listen` as a socket address. Checked when the config is loaded, and
    /// again when the listener binds, for callers that build a config in code.
    pub fn socket_addr(&self) -> Result<SocketAddr, AddrParseError> {
        self.listen.parse()
    }
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
    #[error(
        "{path}: servers[{index}].listen {listen:?} is not an IP:port socket address: {source}"
    )]
    Listen {
        path: String,
        index: usize,
        listen: String,
        source: AddrParseError,
    },
}

impl HttpConfig {
    pub fn from_file(path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        let path = path.as_ref();
        let raw = std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
            path: path.display().to_string(),
            source,
        })?;
        Self::parse(&raw, &path.display().to_string())
    }

    /// Parse config from a YAML string (used by tests/benches and callers that
    /// already have the document in memory).
    pub fn from_yaml(s: &str) -> Result<Self, ConfigError> {
        Self::parse(s, "<str>")
    }

    /// Expand `${VAR}` / `${VAR:-default}` with siphon's own expander (so the
    /// rules match `siphon.yaml` exactly), deserialize, then reject any
    /// listener address that cannot bind, so a typo is a load error rather
    /// than a listener that never comes up.
    fn parse(raw: &str, path: &str) -> Result<Self, ConfigError> {
        let expanded = siphon::config::expand_env_vars(raw);
        let config: Self =
            serde_yaml::from_str(&expanded).map_err(|source| ConfigError::Parse {
                path: path.to_string(),
                source,
            })?;
        for (index, server) in config.servers.iter().enumerate() {
            server.socket_addr().map_err(|source| ConfigError::Listen {
                path: path.to_string(),
                index,
                listen: server.listen.clone(),
                source,
            })?;
        }
        Ok(config)
    }
}

#[cfg(test)]
mod tests {
    use super::{ConfigError, HttpConfig};

    #[test]
    fn unparsable_listen_is_a_config_error_naming_the_value() {
        let error =
            HttpConfig::from_yaml("servers:\n  - listen: \"127.0.0.1:http\"\n").unwrap_err();
        assert!(matches!(error, ConfigError::Listen { .. }), "{error:?}");
        let message = error.to_string();
        assert!(message.contains("127.0.0.1:http"), "{message}");
        assert!(message.contains("servers[0]"), "{message}");
    }

    #[test]
    fn listen_without_a_port_is_rejected() {
        let error = HttpConfig::from_yaml("servers:\n  - listen: \"127.0.0.1\"\n").unwrap_err();
        assert!(matches!(error, ConfigError::Listen { .. }), "{error:?}");
    }

    #[test]
    fn every_listener_is_checked_not_just_the_first() {
        let error = HttpConfig::from_yaml(
            "servers:\n  - listen: \"127.0.0.1:8080\"\n  - listen: \"localhost:8081\"\n",
        )
        .unwrap_err();
        assert!(error.to_string().contains("servers[1]"), "{error}");
    }

    #[test]
    fn ipv4_and_ipv6_listen_addresses_load() {
        let config = HttpConfig::from_yaml(
            "servers:\n  - listen: \"0.0.0.0:8443\"\n  - listen: \"[::1]:8080\"\n",
        )
        .unwrap();
        assert_eq!(config.servers.len(), 2);
        assert_eq!(config.servers[1].socket_addr().unwrap().port(), 8080);
    }

    // Each test uses its own variable name: tests run in parallel and share
    // the process environment.

    #[test]
    fn unset_variable_takes_its_default() {
        let config = HttpConfig::from_yaml(
            "servers:\n  - listen: \"127.0.0.1:${SIPHON_HTTP_TEST_UNSET_PORT:-8090}\"\n",
        )
        .unwrap();
        assert_eq!(config.servers[0].listen, "127.0.0.1:8090");
    }

    #[test]
    fn set_variable_overrides_the_default() {
        std::env::set_var("SIPHON_HTTP_TEST_SET_PORT", "18443");
        let config = HttpConfig::from_yaml(
            "servers:\n  - listen: \"0.0.0.0:${SIPHON_HTTP_TEST_SET_PORT:-8443}\"\n",
        )
        .unwrap();
        assert_eq!(config.servers[0].socket_addr().unwrap().port(), 18443);
    }

    #[test]
    fn expansion_covers_every_string_value() {
        std::env::set_var("SIPHON_HTTP_TEST_TLS_DIR", "/etc/siphon/tls");
        std::env::set_var("SIPHON_HTTP_TEST_API", "https://api.example.com");
        let config = HttpConfig::from_yaml(
            "servers:\n  - listen: \"127.0.0.1:8443\"\n    tls:\n      cert_path: \"${SIPHON_HTTP_TEST_TLS_DIR}/server.crt\"\n      key_path: \"${SIPHON_HTTP_TEST_TLS_DIR}/server.key\"\nclients:\n  api:\n    base_url: \"${SIPHON_HTTP_TEST_API}\"\n",
        )
        .unwrap();
        let tls = config.servers[0].tls.as_ref().unwrap();
        assert_eq!(tls.cert_path, "/etc/siphon/tls/server.crt");
        assert_eq!(tls.key_path, "/etc/siphon/tls/server.key");
        assert_eq!(
            config.clients["api"].base_url.as_deref(),
            Some("https://api.example.com")
        );
    }

    #[test]
    fn unset_variable_without_default_leaves_an_invalid_listen_rejected() {
        let error = HttpConfig::from_yaml(
            "servers:\n  - listen: \"127.0.0.1:${SIPHON_HTTP_TEST_NEVER_SET}\"\n",
        )
        .unwrap_err();
        assert!(matches!(error, ConfigError::Listen { .. }), "{error:?}");
    }

    #[test]
    fn no_servers_is_valid() {
        assert!(HttpConfig::from_yaml("clients: {}\n")
            .unwrap()
            .servers
            .is_empty());
    }

    #[test]
    fn from_file_validates_listen_too() {
        let path =
            std::env::temp_dir().join(format!("siphon-http-config-{}.yaml", std::process::id()));
        std::fs::write(&path, "servers:\n  - listen: \"not-an-address\"\n").unwrap();
        let result = HttpConfig::from_file(&path);
        let _ = std::fs::remove_file(&path);
        let error = result.unwrap_err();
        assert!(matches!(error, ConfigError::Listen { .. }), "{error:?}");
        assert!(
            error.to_string().contains(path.to_str().unwrap()),
            "{error}"
        );
    }
}
