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
use std::sync::Arc;
use std::time::Duration;

use pyo3::exceptions::{PyRuntimeError, PyTypeError};
use pyo3::prelude::*;
use pyo3::types::{PyBytes, PyString};
use reqwest::Method;

use crate::config::ClientConfig;
use crate::Response as PyResponse;

#[pyclass(module = "siphon", name = "Client")]
pub struct Client {
    inner: Arc<reqwest::Client>,
    base_url: Option<String>,
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
    ))]
    fn new(
        name: Option<&str>,
        base_url: Option<&str>,
        verify: Option<&str>,
        cert: Option<Bound<'_, PyAny>>,
        timeout_ms: Option<u64>,
        http2_prior_knowledge: bool,
    ) -> PyResult<Self> {
        // Named-client lookup resolves against the install-time HttpConfig
        // stashed by the runtime (see `crate::runtime::set_named_clients`).
        if let Some(name) = name {
            return Self::from_named(name);
        }

        let mut builder =
            reqwest::Client::builder().timeout(Duration::from_millis(timeout_ms.unwrap_or(5_000)));

        if let Some(ca_path) = verify {
            let ca_bytes = std::fs::read(ca_path)
                .map_err(|e| PyRuntimeError::new_err(format!("read verify CA: {e}")))?;
            let cert = reqwest::Certificate::from_pem(&ca_bytes)
                .map_err(|e| PyRuntimeError::new_err(format!("parse verify CA: {e}")))?;
            builder = builder.add_root_certificate(cert);
        }

        if let Some(cert) = cert {
            // Tuple `(cert_path, key_path)` or single combined-PEM path.
            let identity = build_identity(cert)?;
            builder = builder.identity(identity);
        }

        if http2_prior_knowledge {
            builder = builder.http2_prior_knowledge();
        }

        let inner = builder
            .build()
            .map_err(|e| PyRuntimeError::new_err(format!("build reqwest client: {e}")))?;

        Ok(Self {
            inner: Arc::new(inner),
            base_url: base_url.map(|s| s.to_string()),
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
    /// __aenter__ and __aexit__, so they must return coroutines (i.e.
    /// `Py<PyAny>` produced by `future_into_py`) rather than the bare value.
    ///
    /// reqwest's drop closes the pool, so __aexit__ has nothing to do beyond
    /// resolving to None — Python interprets a falsy return as "don't swallow
    /// the exception", which is what we want.
    fn __aenter__<'py>(slf: Py<Self>, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        pyo3_async_runtimes::tokio::future_into_py(py, async move { Ok(slf) })
    }

    fn __aexit__<'py>(
        &self,
        py: Python<'py>,
        _exc_type: Bound<'_, PyAny>,
        _exc: Bound<'_, PyAny>,
        _tb: Bound<'_, PyAny>,
    ) -> PyResult<Bound<'py, PyAny>> {
        pyo3_async_runtimes::tokio::future_into_py(
            py,
            async move { Python::attach(|py| Ok(py.None())) },
        )
    }
}

impl Client {
    /// Build a client from a named entry in the install-time config.
    fn from_named(name: &str) -> PyResult<Self> {
        let cfg: ClientConfig = crate::runtime::named_client(name).ok_or_else(|| {
            PyRuntimeError::new_err(format!(
                "no client named {name:?} in the http addon config (clients.{name})"
            ))
        })?;

        let mut builder = reqwest::Client::builder()
            .timeout(Duration::from_millis(cfg.timeout_ms))
            .pool_max_idle_per_host(cfg.pool_size);

        if let Some(ca_path) = cfg.verify.as_deref() {
            let ca_bytes = std::fs::read(ca_path)
                .map_err(|e| PyRuntimeError::new_err(format!("read verify CA: {e}")))?;
            let cert = reqwest::Certificate::from_pem(&ca_bytes)
                .map_err(|e| PyRuntimeError::new_err(format!("parse verify CA: {e}")))?;
            builder = builder.add_root_certificate(cert);
        }

        if let (Some(cert_path), Some(key_path)) = (cfg.cert.as_deref(), cfg.key.as_deref()) {
            let identity = identity_from_paths(cert_path, key_path)?;
            builder = builder.identity(identity);
        }

        if cfg.http2_prior_knowledge {
            builder = builder.http2_prior_knowledge();
        }

        let inner = builder
            .build()
            .map_err(|e| PyRuntimeError::new_err(format!("build reqwest client: {e}")))?;

        Ok(Self {
            inner: Arc::new(inner),
            base_url: cfg.base_url,
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
        let url = match &self.base_url {
            Some(base) => format!("{}{}", base.trim_end_matches('/'), path),
            None => path.to_string(),
        };
        let inner = Arc::clone(&self.inner);

        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            let mut req = inner.request(method, &url);
            if let Some(h) = headers {
                for (k, v) in h {
                    req = req.header(&k, &v);
                }
            }
            if let Some(b) = body {
                req = req.body(b);
            }
            let resp = req
                .send()
                .await
                .map_err(|e| PyRuntimeError::new_err(format!("request failed: {e}")))?;

            let status = resp.status().as_u16();
            let headers: HashMap<String, String> = resp
                .headers()
                .iter()
                .filter_map(|(k, v)| {
                    v.to_str()
                        .ok()
                        .map(|s| (k.as_str().to_string(), s.to_string()))
                })
                .collect();
            let body_bytes = resp
                .bytes()
                .await
                .map_err(|e| PyRuntimeError::new_err(format!("read body: {e}")))?
                .to_vec();

            let py_resp = PyResponse {
                status,
                headers,
                body_bytes,
            };
            Python::attach(|py| Ok(Py::new(py, py_resp)?.into_any()))
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
        identity_from_paths(&c, &k)
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

fn identity_from_paths(cert_path: &str, key_path: &str) -> PyResult<reqwest::Identity> {
    let mut buf = std::fs::read(cert_path)
        .map_err(|e| PyRuntimeError::new_err(format!("read cert {cert_path}: {e}")))?;
    let mut key = std::fs::read(key_path)
        .map_err(|e| PyRuntimeError::new_err(format!("read key {key_path}: {e}")))?;
    buf.append(&mut key);
    reqwest::Identity::from_pem(&buf)
        .map_err(|e| PyRuntimeError::new_err(format!("parse identity: {e}")))
}
