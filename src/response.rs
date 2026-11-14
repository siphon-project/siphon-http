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
//! is a lazily created, cached dict with case-sensitive lookups. In-place
//! mutations affect the response sent on the wire. Assigning `headers` replaces
//! the contents and resets the cache. Keys and values must be strings; invalid
//! in-place mutations make a handler's response fail with HTTP 500.
//!
//! Rust construction needs no interpreter. Once the dict exists it is the source
//! of truth; Rust readers must use `header_pairs` while attached to Python.

use std::collections::HashMap;

use pyo3::exceptions::PyTypeError;
use pyo3::prelude::*;
use pyo3::sync::PyOnceLock;
use pyo3::types::{PyBytes, PyDict, PyString};

#[pyclass(module = "siphon", name = "Response")]
pub struct Response {
    #[pyo3(get, set)]
    pub status: u16,

    // Used only until the first Python read (or after replacement).
    headers: HashMap<String, String>,
    headers_dict: PyOnceLock<Py<PyDict>>,

    pub(crate) body_bytes: Vec<u8>,
}

impl Response {
    /// Build plain Rust data; safe on a tokio worker without attaching.
    pub(crate) fn from_parts(
        status: u16,
        headers: HashMap<String, String>,
        body_bytes: Vec<u8>,
    ) -> Self {
        Self {
            status,
            headers,
            headers_dict: PyOnceLock::new(),
            body_bytes,
        }
    }

    /// The headers as they are now: the dict a script reads and may have
    /// changed in place, or the ones the `Response` was built with. Fails
    /// when the dict holds a key or value that is not a `str`.
    pub fn header_pairs(&self, py: Python<'_>) -> PyResult<Vec<(String, String)>> {
        let Some(dict) = self.headers_dict.get(py) else {
            return Ok(self
                .headers
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect());
        };
        // Snapshot first: another Python thread may change the dictionary's keys.
        // Iterating a live dict could panic if its size changed during iteration.
        dict.bind(py)
            .items()
            .iter()
            .map(|item| {
                let (key, value): (Bound<'_, PyAny>, Bound<'_, PyAny>) = item.extract()?;
                let name = key.extract::<String>().map_err(|_| {
                    let key = key
                        .repr()
                        .map(|s| s.to_string_lossy().into_owned())
                        .unwrap_or_else(|_| "<unrepresentable key>".into());
                    PyTypeError::new_err(format!("Response header key {key} must be str"))
                })?;
                let value = value.extract::<String>().map_err(|_| {
                    PyTypeError::new_err(format!("Response header {name:?} value must be str"))
                })?;
                Ok((name, value))
            })
            .collect()
    }
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
        Ok(Self::from_parts(
            status,
            headers.unwrap_or_default(),
            body_bytes,
        ))
    }

    #[getter]
    fn headers<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyDict>> {
        let dict = self.headers_dict.get_or_try_init(py, || -> PyResult<_> {
            Ok((&self.headers).into_pyobject(py)?.unbind())
        })?;
        Ok(dict.bind(py).clone())
    }

    #[setter]
    fn set_headers(&mut self, headers: HashMap<String, String>) {
        self.headers = headers;
        self.headers_dict.take();
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

    fn __repr__(&self, py: Python<'_>) -> String {
        format!(
            "Response(status={}, headers={} keys, body={} bytes)",
            self.status,
            self.headers_dict
                .get(py)
                .map_or_else(|| self.headers.len(), |dict| dict.bind(py).len()),
            self.body_bytes.len()
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check(source: &str) {
        let _serial = crate::dispatch_tests::serial();
        crate::dispatch::attach(|py| {
            let locals = pyo3::types::PyDict::new(py);
            locals
                .set_item("Response", py.get_type::<Response>())
                .unwrap();
            py.run(
                &std::ffi::CString::new(source).unwrap(),
                Some(&locals),
                None,
            )
            .unwrap();
        });
    }

    #[test]
    fn headers_reads_return_same_dict() {
        check("a = Response(headers={'x': 'one'})\nassert a.headers is a.headers");
    }

    #[test]
    fn headers_setter_replaces_and_validates() {
        check(
            r#"
a = Response(headers={'old': 'one'})
old = a.headers
a.headers = {'new': 'two'}
assert a.headers == {'new': 'two'}
assert a.headers is a.headers
assert a.headers is not old
old['stale'] = 'ignored'
for invalid in ({'x': 3}, {3: 'x'}):
    try:
        a.headers = invalid
    except TypeError:
        pass
    else:
        raise AssertionError('invalid headers accepted')
assert a.headers == {'new': 'two'}
"#,
        );
    }

    #[test]
    fn headers_repr_uses_mutated_dict() {
        check("a = Response()\na.headers['x'] = 'y'\nassert 'headers=1 keys' in repr(a)");
    }

    #[test]
    fn headers_rust_constructed_first_read() {
        let _serial = crate::dispatch_tests::serial();
        let response = Response::from_parts(
            200,
            HashMap::from([("x-upstream".into(), "yes".into())]),
            vec![],
        );
        crate::dispatch::attach(|py| {
            let response = Py::new(py, response).unwrap();
            let first = response.getattr(py, "headers").unwrap();
            assert_eq!(
                first
                    .bind(py)
                    .get_item("x-upstream")
                    .unwrap()
                    .extract::<String>()
                    .unwrap(),
                "yes"
            );
            assert!(first.is(response.getattr(py, "headers").unwrap()));
        });
    }
}
