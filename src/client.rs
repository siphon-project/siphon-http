//! `http.Client` pyclass — the outbound HTTP client.
//!
//! Wraps a pooled `reqwest::Client` so scripts can make REST calls without
//! spinning up their own connection pool. Two construction modes:
//!
//! 1. **Named, from config** — `http.Client("api")` looks up `clients.api`
//!    in the addon config for a shared, pre-configured pool.
//! 2. **Inline** — `http.Client(base_url=…, verify=…, cert=…)` builds a
//!    fresh client from explicit parameters.
//!
//! All methods are coroutines returning a [`crate::Response`].

use std::collections::HashMap;
use std::time::Duration;

use pyo3::exceptions::{PyRuntimeError, PyTypeError};
use pyo3::prelude::*;
use pyo3::types::{PyBytes, PyString};
use reqwest::Method;
use siphon::script::blocking::awaitable;

use crate::config::ClientConfig;
use crate::outbound::{ClientError, HttpClient};
use crate::Response as PyResponse;

/// The server certificates a client trusts: every certificate of the
/// `verify` bundle, beside the built-in roots or, with `verify_only`,
/// instead of them.
pub(crate) fn trusting(
    mut builder: reqwest::ClientBuilder,
    verify: Option<&str>,
    verify_only: bool,
) -> Result<reqwest::ClientBuilder, String> {
    match verify {
        Some(path) => {
            let pem = std::fs::read(path).map_err(|e| format!("read verify CA: {e}"))?;
            let certificates = reqwest::Certificate::from_pem_bundle(&pem)
                .map_err(|e| format!("parse verify CA: {e}"))?;
            if certificates.is_empty() {
                return Err(format!("verify CA: no certificate in {path}"));
            }
            for certificate in certificates {
                builder = builder.add_root_certificate(certificate);
            }
        }
        None if verify_only => return Err("verify_only needs verify".into()),
        None => {}
    }
    if verify_only {
        builder = builder.tls_built_in_root_certs(false);
    }
    Ok(builder)
}

/// Hand a 3xx back as received instead of following it, unless `follow`.
fn redirecting(builder: reqwest::ClientBuilder, follow: bool) -> reqwest::ClientBuilder {
    if follow {
        builder
    } else {
        builder.redirect(reqwest::redirect::Policy::none())
    }
}

#[pyclass(module = "siphon", name = "Client")]
pub struct Client {
    outbound: HttpClient,
}

#[pymethods]
impl Client {
    #[new]
    #[pyo3(signature = (
        name=None,
        base_url=None,
        verify=None,
        cert=None,
        timeout_ms=None,
        http2_prior_knowledge=false,
        verify_only=false,
        follow_redirects=true,
    ))]
    fn new(
        name: Option<&str>,
        base_url: Option<&str>,
        verify: Option<&str>,
        cert: Option<Bound<'_, PyAny>>,
        timeout_ms: Option<u64>,
        http2_prior_knowledge: bool,
        verify_only: bool,
        follow_redirects: bool,
    ) -> PyResult<Self> {
        // Named-client lookup resolves against the install-time HttpConfig
        // stashed by the runtime (see `crate::runtime::set_named_clients`).
        if let Some(name) = name {
            return Self::from_named(name);
        }

        let mut builder =
            reqwest::Client::builder().timeout(Duration::from_millis(timeout_ms.unwrap_or(5_000)));

        builder = trusting(builder, verify, verify_only).map_err(PyRuntimeError::new_err)?;

        if let Some(cert) = cert {
            // Tuple `(cert_path, key_path)` or single combined-PEM path.
            let identity = build_identity(cert)?;
            builder = builder.identity(identity);
        }

        if http2_prior_knowledge {
            builder = builder.http2_prior_knowledge();
        }

        let inner = redirecting(builder, follow_redirects)
            .build()
            .map_err(|e| PyRuntimeError::new_err(format!("build reqwest client: {e}")))?;

        Ok(Self {
            outbound: HttpClient::from_parts(inner, base_url),
        })
    }

    #[pyo3(signature = (path, headers=None))]
    fn get<'py>(
        &self,
        py: Python<'py>,
        path: &str,
        headers: Option<HashMap<String, String>>,
    ) -> PyResult<Bound<'py, PyAny>> {
        self.send(py, Method::GET, path, None, headers)
    }

    #[pyo3(signature = (path, body=None, headers=None))]
    fn put<'py>(
        &self,
        py: Python<'py>,
        path: &str,
        body: Option<Bound<'_, PyAny>>,
        headers: Option<HashMap<String, String>>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let body_bytes = body.map(extract_body).transpose()?;
        self.send(py, Method::PUT, path, body_bytes, headers)
    }

    #[pyo3(signature = (path, body=None, headers=None))]
    fn post<'py>(
        &self,
        py: Python<'py>,
        path: &str,
        body: Option<Bound<'_, PyAny>>,
        headers: Option<HashMap<String, String>>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let body_bytes = body.map(extract_body).transpose()?;
        self.send(py, Method::POST, path, body_bytes, headers)
    }

    #[pyo3(signature = (path, body=None, headers=None))]
    fn patch<'py>(
        &self,
        py: Python<'py>,
        path: &str,
        body: Option<Bound<'_, PyAny>>,
        headers: Option<HashMap<String, String>>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let body_bytes = body.map(extract_body).transpose()?;
        self.send(py, Method::PATCH, path, body_bytes, headers)
    }

    #[pyo3(signature = (path, headers=None))]
    fn delete<'py>(
        &self,
        py: Python<'py>,
        path: &str,
        headers: Option<HashMap<String, String>>,
    ) -> PyResult<Bound<'py, PyAny>> {
        self.send(py, Method::DELETE, path, None, headers)
    }

    /// `async with http.Client(...) as c:`. Python's `async with` awaits both
    /// __aenter__ and __aexit__, so they must return awaitables (produced by
    /// siphon's `awaitable`) rather than the bare value.
    ///
    /// reqwest's drop closes the pool, so __aexit__ has nothing to do beyond
    /// resolving to None — Python interprets a falsy return as "don't swallow
    /// the exception", which is what we want. Both futures hold Rust values
    /// only; they are converted when the awaitable completes, on siphon's
    /// completion threads.
    fn __aenter__<'py>(slf: Py<Self>, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        awaitable(py, async move { Ok(slf) })
    }

    fn __aexit__<'py>(
        &self,
        py: Python<'py>,
        _exc_type: Bound<'_, PyAny>,
        _exc: Bound<'_, PyAny>,
        _tb: Bound<'_, PyAny>,
    ) -> PyResult<Bound<'py, PyAny>> {
        awaitable(py, async move { Ok(()) })
    }
}

impl Client {
    /// The shared pool of a named entry in the install-time config.
    pub(crate) fn from_named(name: &str) -> PyResult<Self> {
        let cfg = crate::runtime::named_client(name).ok_or_else(|| ClientError::UnknownClient {
            name: name.to_string(),
        });
        let outbound = cfg
            .and_then(|cfg| HttpClient::pooled(name, &cfg))
            .map_err(|e| PyRuntimeError::new_err(e.to_string()))?;
        Ok(Self { outbound })
    }

    #[cfg(test)]
    pub(crate) fn outbound(&self) -> &HttpClient {
        &self.outbound
    }

    /// Build the pool of a `clients.<name>` entry. Called once per name, by
    /// [`HttpClient::named`] and `http.Client("<name>")` alike.
    pub(crate) fn pool(cfg: &ClientConfig) -> Result<reqwest::Client, ClientError> {
        let mut builder = reqwest::Client::builder()
            .timeout(Duration::from_millis(cfg.timeout_ms))
            .pool_max_idle_per_host(cfg.pool_size);

        builder = trusting(builder, cfg.verify.as_deref(), cfg.verify_only)
            .map_err(|reason| ClientError::Configuration { reason })?;

        if let (Some(cert_path), Some(key_path)) = (cfg.cert.as_deref(), cfg.key.as_deref()) {
            let identity = identity_from_paths(cert_path, key_path)
                .map_err(|reason| ClientError::Configuration { reason })?;
            builder = builder.identity(identity);
        }

        if cfg.http2_prior_knowledge {
            builder = builder.http2_prior_knowledge();
        }

        redirecting(builder, cfg.follow_redirects)
            .build()
            .map_err(|e| ClientError::Configuration {
                reason: format!("build reqwest client: {e}"),
            })
    }

    fn send<'py>(
        &self,
        py: Python<'py>,
        method: Method,
        path: &str,
        body: Option<Vec<u8>>,
        headers: Option<HashMap<String, String>>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let outbound = self.outbound.clone();
        let path = path.to_string();
        let headers: Vec<(String, String)> = headers.unwrap_or_default().into_iter().collect();

        // The future runs on a tokio worker and holds no Python object: it
        // resolves to the `Response` as Rust data, and siphon's `awaitable`
        // makes the Python object when it completes the awaitable, on one of
        // siphon's fixed completion threads rather than on the worker.
        awaitable(py, async move {
            let reply = outbound
                .request(method, path.as_str(), &headers, body)
                .await
                .map_err(|e| PyRuntimeError::new_err(e.to_string()))?;
            Ok(PyResponse {
                status: reply.status,
                headers: reply.headers.into_iter().collect(),
                body_bytes: reply.body,
            })
        })
    }
}

fn extract_body(obj: Bound<'_, PyAny>) -> PyResult<Vec<u8>> {
    if let Ok(b) = obj.cast::<PyBytes>() {
        Ok(b.as_bytes().to_vec())
    } else if let Ok(s) = obj.cast::<PyString>() {
        Ok(s.to_str()?.as_bytes().to_vec())
    } else {
        Err(PyTypeError::new_err("body must be bytes or str"))
    }
}

fn build_identity(cert: Bound<'_, PyAny>) -> PyResult<reqwest::Identity> {
    // Accept either `(cert_path, key_path)` tuple or single combined-PEM
    // path. PEM with cert-then-key is reqwest's expected format for
    // `Identity::from_pem`.
    if let Ok((c, k)) = cert.extract::<(String, String)>() {
        identity_from_paths(&c, &k).map_err(PyRuntimeError::new_err)
    } else if let Ok(path) = cert.extract::<String>() {
        let pem = std::fs::read(&path)
            .map_err(|e| PyRuntimeError::new_err(format!("read cert {path}: {e}")))?;
        reqwest::Identity::from_pem(&pem)
            .map_err(|e| PyRuntimeError::new_err(format!("parse identity: {e}")))
    } else {
        Err(PyTypeError::new_err(
            "cert= must be a path string or a (cert_path, key_path) tuple",
        ))
    }
}

fn identity_from_paths(cert_path: &str, key_path: &str) -> Result<reqwest::Identity, String> {
    let mut buf = std::fs::read(cert_path).map_err(|e| format!("read cert {cert_path}: {e}"))?;
    let mut key = std::fs::read(key_path).map_err(|e| format!("read key {key_path}: {e}"))?;
    buf.append(&mut key);
    reqwest::Identity::from_pem(&buf).map_err(|e| format!("parse identity: {e}"))
}
