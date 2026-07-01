//! `http.Response` pyclass — what handlers return.
//!
//! Construction:
//!
//! ```python
//! return http.Response(
//!     status=200,
//!     headers={"Content-Type": "application/json"},
//!     body=b'{"ok": true}',
//! )
//! ```
//!
//! `body` accepts `bytes` or `str` (UTF-8 encoded automatically). `headers`
//! is a plain dict; case is preserved on the wire but lookups are
//! case-insensitive.

use std::collections::HashMap;

use pyo3::exceptions::PyTypeError;
use pyo3::prelude::*;
use pyo3::types::{PyBytes, PyString};

#[pyclass(module = "siphon", name = "Response")]
pub struct Response {
    #[pyo3(get, set)]
    pub status: u16,

    #[pyo3(get, set)]
    pub headers: HashMap<String, String>,

    pub(crate) body_bytes: Vec<u8>,
}

#[pymethods]
impl Response {
    #[new]
    #[pyo3(signature = (status=200, headers=None, body=None))]
    fn new(
        status: u16,
        headers: Option<HashMap<String, String>>,
        body: Option<Bound<'_, PyAny>>,
    ) -> PyResult<Self> {
        let body_bytes = match body {
            None => Vec::new(),
            Some(obj) => {
                if let Ok(b) = obj.cast::<PyBytes>() {
                    b.as_bytes().to_vec()
                } else if let Ok(s) = obj.cast::<PyString>() {
                    s.to_str()?.as_bytes().to_vec()
                } else {
                    return Err(PyTypeError::new_err("Response body must be bytes or str"));
                }
            }
        };
        Ok(Self {
            status,
            headers: headers.unwrap_or_default(),
            body_bytes,
        })
    }

    #[getter]
    fn body<'py>(&self, py: Python<'py>) -> Bound<'py, PyBytes> {
        PyBytes::new(py, &self.body_bytes)
    }

    /// Raise an exception if `status >= 400`. Useful on the client side.
    fn raise_for_status(&self) -> PyResult<()> {
        if self.status >= 400 {
            Err(pyo3::exceptions::PyRuntimeError::new_err(format!(
                "HTTP {} response",
                self.status
            )))
        } else {
            Ok(())
        }
    }

    fn __repr__(&self) -> String {
        format!(
            "Response(status={}, headers={} keys, body={} bytes)",
            self.status,
            self.headers.len(),
            self.body_bytes.len()
        )
    }
}
