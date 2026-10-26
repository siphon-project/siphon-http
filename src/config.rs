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

    /// A name for this listener, which a route names to be served on it alone
    /// (`@http.route(path, listeners=["internal"])`) and a handler reads as
    /// `req.listener.name`. Without one the listener is known by its `listen`
    /// value. Names are unique across `servers`.
    #[serde(default)]
    pub name: Option<String>,

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

    /// The DSCP (RFC 2474) of what this listener sends: a codepoint 0 to 63
    /// or its name (`ef`, `af11` to `af43`, `cs0` to `cs7`, `va`, `le`,
    /// `df`). Set on the listening socket, from which accepted connections
    /// take it. IPv4 listeners only.
    #[serde(default)]
    pub dscp: Option<Dscp>,
}

/// A Differentiated Services codepoint (RFC 2474 clause 3): six bits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Dscp(pub u8);

impl Dscp {
    /// The codepoint of a name of RFC 2474 (CS), RFC 2597 (AF), RFC 3246
    /// (EF), RFC 5865 (VOICE-ADMIT) or RFC 8622 (LE), any case.
    pub fn named(name: &str) -> Option<Self> {
        let name = name.to_ascii_lowercase();
        let value = match name.as_str() {
            "df" | "default" => 0,
            "le" => 1,
            "ef" => 46,
            "va" => 44,
            _ => {
                let digits = name
                    .strip_prefix("cs")
                    .or_else(|| name.strip_prefix("af"))?;
                let digits: Vec<u8> = digits.bytes().map(|byte| byte.wrapping_sub(b'0')).collect();
                match (name.starts_with("cs"), digits.as_slice()) {
                    (true, [class]) if *class <= 7 => class * 8,
                    // AFxy: class x 1 to 4, drop precedence y 1 to 3.
                    (false, [class, drop]) if (1..=4).contains(class) && (1..=3).contains(drop) => {
                        class * 8 + drop * 2
                    }
                    _ => return None,
                }
            }
        };
        Some(Self(value))
    }
}

impl<'de> Deserialize<'de> for Dscp {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Given {
            Number(u8),
            Name(String),
        }
        match Given::deserialize(deserializer)? {
            Given::Number(value) if value <= 63 => Ok(Self(value)),
            Given::Number(value) => Err(serde::de::Error::custom(format!(
                "dscp {value} is not a six-bit codepoint (0 to 63)"
            ))),
            Given::Name(name) => Self::named(&name).ok_or_else(|| {
                serde::de::Error::custom(format!("dscp {name:?} is no codepoint name"))
            }),
        }
    }
}

impl ServerConfig {
    /// `listen` as a socket address. Checked when the config is loaded, and
    /// again when the listener binds, for callers that build a config in code.
    pub fn socket_addr(&self) -> Result<SocketAddr, AddrParseError> {
        self.listen.parse()
    }

    /// What routes and handlers call this listener: its `name`, or else its
    /// `listen` value.
    pub fn listener_name(&self) -> &str {
        self.name.as_deref().unwrap_or(&self.listen)
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

    /// Look at `cert_path`, `key_path` and `client_ca` every this many
    /// seconds, and when one changed, serve new connections with the files
    /// as they are now; connections already open keep theirs. Files that
    /// do not load are logged and the previous ones kept. Without it the
    /// files are read once, when the listener binds.
    #[serde(default)]
    pub reload_seconds: Option<u64>,
}

#[derive(Debug, Deserialize, Clone, PartialEq, Eq)]
pub struct ClientConfig {
    /// Optional base URL prepended to relative request paths.
    #[serde(default)]
    pub base_url: Option<String>,

    /// Per-request timeout in milliseconds. Default: 5s.
    #[serde(default = "default_client_timeout")]
    pub timeout_ms: u64,

    /// CA bundle (PEM, every certificate of the file) for verifying server
    /// certs, trusted beside the built-in roots. Empty = built-in roots only.
    #[serde(default)]
    pub verify: Option<String>,

    /// Trust the `verify` bundle only, not the built-in roots: what a
    /// client in an operator PKI wants, where a public CA must not vouch for
    /// a peer. Needs `verify`.
    #[serde(default)]
    pub verify_only: bool,

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

    /// Follow a 3xx to its `Location` (up to 10 hops) inside the request, as
    /// reqwest does by default. `false` hands the 3xx back to the script as
    /// received, `Location` included: what a proxy needs to relay the
    /// redirect or follow it on its own terms. Default: on.
    #[serde(default = "default_follow_redirects")]
    pub follow_redirects: bool,

    /// Look at `verify`, `cert` and `key` at most every this many seconds,
    /// on the next request, and when one changed, build the pool again
    /// from the files as they are now. Files that do not load are logged
    /// and the previous pool kept. Without it they are read once.
    #[serde(default)]
    pub reload_seconds: Option<u64>,
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
fn default_follow_redirects() -> bool {
    true
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("read {path}: {source}")]
    Read {
        path: String,
        source: std::io::Error,
    },
    #[error("parse {path}: {}{source}", key_prefix(key))]
    Parse {
        path: String,
        /// Where in the document the error is, e.g. `servers[1].tls`
        /// (`serde_path_to_error` notation). `None` when the error is in the
        /// YAML syntax or at the top level rather than under a key.
        key: Option<String>,
        // Boxed: the parser's error carries a source snippet and is large.
        source: Box<serde_saphyr::Error>,
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
    #[error("{path}: servers[{index}] is called {name:?}, and so is an earlier listener")]
    ListenerName {
        path: String,
        index: usize,
        name: String,
    },
}

fn key_prefix(key: &Option<String>) -> String {
    key.as_deref()
        .map(|key| format!("{key}: "))
        .unwrap_or_default()
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
        // Only `true` / `false` (any case) are booleans, so a YAML 1.1 `yes` /
        // `on` stays the error it always was rather than turning a switch on.
        let options = serde_saphyr::options! { strict_booleans: true };
        // serde_path_to_error tracks the key being deserialized, so the error
        // names it next to the line and column the parser reports.
        let mut key = None;
        let config: Self =
            serde_saphyr::with_deserializer_from_str_with_options(&expanded, options, |de| {
                serde_path_to_error::deserialize(de).map_err(|error| {
                    let at = error.path().to_string();
                    key = (at != ".").then_some(at);
                    error.into_inner()
                })
            })
            .map_err(|source| ConfigError::Parse {
                path: path.to_string(),
                key,
                source: Box::new(source),
            })?;
        for (index, server) in config.servers.iter().enumerate() {
            server.socket_addr().map_err(|source| ConfigError::Listen {
                path: path.to_string(),
                index,
                listen: server.listen.clone(),
                source,
            })?;
            // A route restricted to a listener names it; two listeners of one
            // name would make that ambiguous.
            let name = server.listener_name();
            if config.servers[..index].iter().any(|earlier| {
                // Two unnamed entries with one address are left to fail at
                // bind, as they always did.
                (server.name.is_some() || earlier.name.is_some()) && earlier.listener_name() == name
            }) {
                return Err(ConfigError::ListenerName {
                    path: path.to_string(),
                    index,
                    name: name.to_string(),
                });
            }
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
    fn a_listener_is_known_by_its_name_or_else_its_listen_value() {
        let config = HttpConfig::from_yaml(
            "servers:\n  - listen: \"127.0.0.1:8080\"\n  - listen: \"127.0.0.1:8443\"\n    name: internal\n",
        )
        .unwrap();
        assert_eq!(config.servers[0].name, None);
        assert_eq!(config.servers[0].listener_name(), "127.0.0.1:8080");
        assert_eq!(config.servers[1].listener_name(), "internal");
    }

    #[test]
    fn two_listeners_of_one_name_are_a_config_error() {
        for yaml in [
            "servers:\n  - listen: \"127.0.0.1:8080\"\n    name: api\n  - listen: \"127.0.0.1:8081\"\n    name: api\n",
            // A name that is another listener's address.
            "servers:\n  - listen: \"127.0.0.1:8080\"\n  - listen: \"127.0.0.1:8081\"\n    name: \"127.0.0.1:8080\"\n",
        ] {
            let error = HttpConfig::from_yaml(yaml).unwrap_err();
            assert!(matches!(error, ConfigError::ListenerName { .. }), "{error:?}");
            assert!(error.to_string().contains("servers[1]"), "{error}");
        }
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
    fn only_true_and_false_spell_a_boolean() {
        // YAML 1.1 spellings such as `yes` / `on` are not booleans here, as
        // they never were; `True` / `TRUE` are.
        for spelling in ["yes", "on", "y", "no", "off"] {
            let yaml = format!("clients:\n  a:\n    http2_prior_knowledge: {spelling}\n");
            let error = HttpConfig::from_yaml(&yaml).unwrap_err();
            assert!(
                matches!(error, ConfigError::Parse { .. }),
                "{spelling}: {error:?}"
            );
        }
        for (spelling, expected) in [("true", true), ("True", true), ("FALSE", false)] {
            let yaml = format!("clients:\n  a:\n    http2_prior_knowledge: {spelling}\n");
            let config = HttpConfig::from_yaml(&yaml).unwrap();
            assert_eq!(
                config.clients["a"].http2_prior_knowledge, expected,
                "{spelling}"
            );
        }
    }

    #[test]
    fn a_parse_error_names_the_key_path_and_the_line() {
        let error = HttpConfig::from_yaml("clients:\n  api:\n    pool_size: lots\n").unwrap_err();
        assert!(matches!(error, ConfigError::Parse { .. }), "{error:?}");
        let message = error.to_string();
        assert!(message.contains("clients.api.pool_size"), "{message}");
        assert!(message.contains("line 3"), "{message}");

        let error = HttpConfig::from_yaml(
            "servers:\n  - listen: \"127.0.0.1:8080\"\n  - listen: \"127.0.0.1:8081\"\n    tls:\n      key_path: /k\n",
        )
        .unwrap_err();
        let message = error.to_string();
        assert!(message.contains("servers[1].tls"), "{message}");
        assert!(message.contains("cert_path"), "{message}");
    }

    #[test]
    fn a_duplicate_key_is_a_parse_error_naming_its_line() {
        let error =
            HttpConfig::from_yaml("clients:\n  api: {}\n  api:\n    pool_size: 3\n").unwrap_err();
        assert!(matches!(error, ConfigError::Parse { .. }), "{error:?}");
        assert!(error.to_string().contains("line 3"), "{error}");
    }

    #[test]
    fn a_client_follows_redirects_unless_switched_off() {
        let config = HttpConfig::from_yaml(
            "clients:\n  plain: {}\n  relay:\n    follow_redirects: false\n  follow:\n    follow_redirects: true\n",
        )
        .unwrap();
        assert!(config.clients["plain"].follow_redirects);
        assert!(!config.clients["relay"].follow_redirects);
        assert!(config.clients["follow"].follow_redirects);
    }

    #[test]
    fn follow_redirects_takes_a_boolean_only() {
        let error =
            HttpConfig::from_yaml("clients:\n  p:\n    follow_redirects: sometimes\n").unwrap_err();
        assert!(error.to_string().contains("follow_redirects"), "{error}");
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
