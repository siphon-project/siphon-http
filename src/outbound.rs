//! The outbound HTTP client for Rust callers.
//!
//! [`HttpClient::named`] hands out the pool of a `clients.<name>` entry: the
//! same pool, built once per process, that `http.Client("<name>")` gives a
//! script. Rust code that links this crate can therefore send through the
//! configured TLS / mutual-TLS / HTTP/2 settings before (or without) the
//! server task and with no Python interpreter.
//!
//! ```no_run
//! # async fn example(configuration: &siphon_http::HttpConfig) -> Result<(), siphon_http::ClientError> {
//! use siphon_http::HttpClient;
//!
//! let client = HttpClient::named(configuration, "api")?;
//! let reply = client
//!     .request(
//!         http::Method::GET,
//!         "/v1/things",
//!         &[("accept".into(), "application/json".into())],
//!         None,
//!     )
//!     .await?;
//! assert_eq!(reply.status, 200);
//! # Ok(())
//! # }
//! ```
//!
//! A pool's connections run on the tokio runtime of the request that opened
//! them. When that runtime shuts down they close, and a later request, on any
//! runtime, opens new ones.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock, PoisonError};

use http::header::{HeaderName, HeaderValue};
use http::{HeaderMap, Method};

use crate::config::{ClientConfig, HttpConfig};

/// Why a client could not be had or a request got no response.
///
/// No variant carries a header value, a body or the request target, so the
/// text is safe to log.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ClientError {
    /// The configuration has no `clients.<name>`.
    #[error("no client named {name:?} in the http addon config (clients.{name})")]
    UnknownClient { name: String },
    /// A pool of this name is already built from other settings.
    #[error("clients.{name}: a pool of that name is already built from other settings")]
    Conflict { name: String },
    /// The settings do not make a client: an unreadable CA bundle or
    /// identity, `verify_only` without `verify`.
    #[error("client configuration: {reason}")]
    Configuration { reason: String },
    /// The target, joined onto `base_url`, is not an absolute URL.
    #[error("request target: {reason}")]
    Target { reason: String },
    /// A request header name or value is not a valid field.
    #[error("request header {name:?} is not a valid field")]
    Header { name: String },
    /// No connection: refused, unresolvable, or the TLS handshake failed.
    #[error("connect: {reason}")]
    Connect { reason: String },
    /// The client's `timeout_ms` passed before the whole response arrived.
    #[error("timed out")]
    Timeout,
    /// The response body could not be read.
    #[error("body: {reason}")]
    Body { reason: String },
    /// Any other failure of the exchange.
    #[error("request failed: {reason}")]
    Request { reason: String },
}

impl ClientError {
    /// Classify a reqwest error; the text leaves out the URL.
    fn exchange(error: reqwest::Error) -> Self {
        let error = error.without_url();
        let reason = || describe(&error);
        if error.is_timeout() {
            Self::Timeout
        } else if error.is_connect() {
            Self::Connect { reason: reason() }
        } else if error.is_body() || error.is_decode() {
            Self::Body { reason: reason() }
        } else {
            Self::Request { reason: reason() }
        }
    }
}

/// `error` and its sources, joined with ": ".
fn describe(error: &dyn std::error::Error) -> String {
    let mut text = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        text.push_str(": ");
        text.push_str(&cause.to_string());
        source = cause.source();
    }
    text
}

/// A response: status, header fields and the whole body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reply {
    pub status: u16,
    /// Lower-cased names, in the order received; the values of a repeated
    /// field are joined with ", " (RFC 9110 clause 5.3).
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Reply {
    /// The value of header field `name`, compared case-insensitively.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(field, _)| field.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
}

/// A pooled outbound HTTP client. Cloning is cheap and shares the pool.
#[derive(Debug, Clone)]
pub struct HttpClient {
    inner: reqwest::Client,
    base_url: Option<Arc<str>>,
}

/// The pools built so far, by client name, with the settings each was built
/// from.
type Pools = HashMap<String, (ClientConfig, HttpClient)>;

fn pools() -> &'static Mutex<Pools> {
    static POOLS: OnceLock<Mutex<Pools>> = OnceLock::new();
    POOLS.get_or_init(|| Mutex::new(HashMap::new()))
}

impl HttpClient {
    /// The pool of `clients.<name>` in `configuration`.
    ///
    /// Built on first use and shared with every later caller of the same
    /// name, `http.Client("<name>")` of a script included. A second
    /// configuration giving the name other settings gets
    /// [`ClientError::Conflict`], never a pool it did not configure.
    pub fn named(configuration: &HttpConfig, name: &str) -> Result<Self, ClientError> {
        let settings =
            configuration
                .clients
                .get(name)
                .ok_or_else(|| ClientError::UnknownClient {
                    name: name.to_string(),
                })?;
        Self::pooled(name, settings)
    }

    /// The shared pool of `name`, built from `settings` if there is none.
    pub(crate) fn pooled(name: &str, settings: &ClientConfig) -> Result<Self, ClientError> {
        // Only inserts happen under the lock, so a panic elsewhere leaves
        // the map intact.
        let mut pools = pools().lock().unwrap_or_else(PoisonError::into_inner);
        if let Some((built_from, client)) = pools.get(name) {
            return if built_from == settings {
                Ok(client.clone())
            } else {
                Err(ClientError::Conflict {
                    name: name.to_string(),
                })
            };
        }
        let client = Self::from_parts(
            crate::client::Client::pool(settings)?,
            settings.base_url.as_deref(),
        );
        pools.insert(name.to_string(), (settings.clone(), client.clone()));
        Ok(client)
    }

    pub(crate) fn from_parts(inner: reqwest::Client, base_url: Option<&str>) -> Self {
        Self {
            inner,
            base_url: base_url.map(Arc::from),
        }
    }

    /// Send one request and read the whole response.
    ///
    /// `target` is joined onto the client's `base_url` (a plain
    /// concatenation, as for `http.Client`), or is the whole URL when the
    /// client has none. `headers` are sent in order; a repeated name sends
    /// the field repeatedly. Any status is a [`Reply`]; only a failed
    /// exchange is an error.
    pub async fn request(
        &self,
        method: Method,
        target: &str,
        headers: &[(String, String)],
        body: Option<Vec<u8>>,
    ) -> Result<Reply, ClientError> {
        let url = match &self.base_url {
            Some(base) => format!("{}{}", base.trim_end_matches('/'), target),
            None => target.to_string(),
        };
        let url = reqwest::Url::parse(&url).map_err(|error| ClientError::Target {
            reason: error.to_string(),
        })?;
        let mut fields = HeaderMap::with_capacity(headers.len());
        for (name, value) in headers {
            let invalid = || ClientError::Header { name: name.clone() };
            let field = HeaderName::from_bytes(name.as_bytes()).map_err(|_| invalid())?;
            let value = HeaderValue::from_str(value).map_err(|_| invalid())?;
            fields.append(field, value);
        }

        let mut request = self.inner.request(method, url).headers(fields);
        if let Some(body) = body {
            request = request.body(body);
        }
        let response = request.send().await.map_err(ClientError::exchange)?;
        let status = response.status().as_u16();
        let headers = crate::runtime::joined_headers(response.headers());
        let body = response
            .bytes()
            .await
            .map_err(ClientError::exchange)?
            .to_vec();
        Ok(Reply {
            status,
            headers,
            body,
        })
    }
}
