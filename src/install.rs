//! Public addon API.
//!
//! Exposes [`namespace`] (the `http` Python module) and [`task`] (the HTTP
//! runtime) for a composing siphon binary.
//!
//! - [`namespace`] loads `python/http.py` as the actual `siphon.http` module
//!   (the decorator façade), injects the addon config as `_config` for
//!   cfg-readout helpers, and installs the real Rust `Request` / `Response` /
//!   `Client` pyclasses over the module's Python prototypes.
//! - [`task`] spawns the axum listener(s) on `script.tokio_handle()` and
//!   dispatches inbound requests into registered handlers via
//!   `script.call_handler(...)`.
//!
//! Both must be registered: the namespace alone is inert (no listener), the
//! task alone has no script handlers to dispatch to.

use std::ffi::CString;

use pyo3::prelude::*;
use pyo3::types::{PyDict, PyList, PyModule};

use crate::HttpConfig;

const NAMESPACE_SOURCE: &str = include_str!("../python/http.py");

/// Build the `http` namespace-module closure.
///
/// On call, the closure compiles `python/http.py` into a Python module,
/// injects the addon config as `_config`, overrides the prototype
/// `Request` / `Response` / `Client` classes with the real Rust pyclasses,
/// and returns the module — which siphon then attaches as `siphon.http`.
pub fn namespace(
    cfg: HttpConfig,
) -> impl FnOnce(Python<'_>) -> PyResult<Py<PyAny>> + Send + 'static {
    move |py| {
        let source = CString::new(NAMESPACE_SOURCE).expect("python/http.py contains no NUL bytes");
        let module =
            PyModule::from_code(py, source.as_c_str(), c"siphon_http/__init__.py", c"http")?;

        let cfg_dict = PyDict::new(py);
        let listens = PyList::new(py, cfg.servers.iter().map(|s| s.listen.clone()))?;
        cfg_dict.set_item("listen_addrs", listens)?;
        let client_names = PyList::new(py, cfg.clients.keys().cloned())?;
        cfg_dict.set_item("client_names", client_names)?;
        module.setattr("_config", cfg_dict)?;

        // Override the python/http.py prototype Request/Response/Client
        // classes with the real Rust pyclasses. Scripts do
        // `http.Response(status=200, ...)`; without this they'd hit the
        // prototype's `__init__` which raises NotImplementedError.
        module.setattr("Request", py.get_type::<crate::Request>())?;
        module.setattr("Response", py.get_type::<crate::Response>())?;
        module.setattr("Client", py.get_type::<crate::Client>())?;

        Ok(module.into_any().unbind())
    }
}

/// Build the HTTP runtime task closure.
///
/// Spawns the axum listeners (per [`HttpConfig::servers`]) + the outbound
/// client pool on the script's tokio runtime, wired to the script's
/// registered `@http.route` / `@http.middleware` / `@http.on_startup`
/// handlers.
pub fn task(cfg: HttpConfig) -> impl FnOnce(siphon::script::ScriptHandle) + Send + 'static {
    move |script| {
        crate::runtime::spawn(cfg, script);
    }
}
