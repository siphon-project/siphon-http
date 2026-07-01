//! `http.Request` pyclass — what handlers receive.
//!
//! Carries method, URL parts, headers, path params (extracted by the
//! router), and body bytes. Mirrors the shape of Starlette/FastAPI's
//! Request object so anyone who's written ASGI code finds the API familiar.

use std::collections::HashMap;

use pyo3::prelude::*;
use pyo3::types::PyBytes;

#[pyclass(module = "siphon", name = "Request")]
pub struct Request {
    #[pyo3(get)]
    pub method: String,

    /// Full request path including any query string.
    #[pyo3(get)]
    pub path: String,

    /// Path parameters extracted from the matched route, e.g. the route
    /// `/users/{id}/profile` matched against `/users/42/profile` yields
    /// `{"id": "42"}` (already URL-decoded).
    #[pyo3(get)]
    pub path_params: HashMap<String, String>,

    /// Query string parameters.
    #[pyo3(get)]
    pub query_params: HashMap<String, String>,

    /// Lower-cased header map. Multi-value headers are joined with ", " per
    /// RFC 9110 §5.2; scripts that need the structured form can re-split.
    #[pyo3(get)]
    pub headers: HashMap<String, String>,

    /// Body bytes. Buffered server-side subject to `max_body_bytes`.
    pub(crate) body_bytes: Vec<u8>,

    /// Remote socket address as `ip:port`.
    #[pyo3(get)]
    pub client: String,
}

#[pymethods]
impl Request {
    /// Read the request body. Returns bytes synchronously today — the body is
    /// already buffered. The API stays a coroutine on the Python side (see
    /// `python/http.py`) so future streaming versions don't break callers.
    fn body<'py>(&self, py: Python<'py>) -> Bound<'py, PyBytes> {
        PyBytes::new(py, &self.body_bytes)
    }

    /// Convenience: header by name, case-insensitive. Returns None if absent.
    fn header(&self, name: &str) -> Option<String> {
        self.headers.get(&name.to_ascii_lowercase()).cloned()
    }

    fn __repr__(&self) -> String {
        format!("Request(method={:?}, path={:?})", self.method, self.path)
    }
}

impl Request {
    /// Internal constructor used by the dispatcher when an axum request
    /// arrives. Not exposed to Python — scripts never build their own
    /// `Request` instances.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn from_parts(
        method: String,
        path: String,
        path_params: HashMap<String, String>,
        query_params: HashMap<String, String>,
        headers: HashMap<String, String>,
        body_bytes: Vec<u8>,
        client: String,
    ) -> Self {
        Self {
            method,
            path,
            path_params,
            query_params,
            headers,
            body_bytes,
            client,
        }
    }
}
