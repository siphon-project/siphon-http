//! The `http` namespace module, built the way a composing binary builds it,
//! inside a real interpreter.
//!
//! The namespace is `siphon.http`. The standard library has a package named
//! `http` too (`http.client`, `http.server`, `http.cookies`), which
//! `urllib.request`, `xml.sax.saxutils` and many others import, so the
//! namespace must never be installed under the bare name `http`.

use pyo3::prelude::*;
use pyo3::types::PyDict;

use siphon_http::HttpConfig;

fn build_namespace(py: Python<'_>) -> Bound<'_, PyAny> {
    siphon_http::namespace(HttpConfig::default())(py)
        .expect("namespace builds")
        .into_bound(py)
}

fn sys_modules(py: Python<'_>) -> Bound<'_, PyDict> {
    py.import("sys")
        .unwrap()
        .getattr("modules")
        .unwrap()
        .cast_into::<PyDict>()
        .unwrap()
}

#[test]
fn the_namespace_leaves_the_standard_library_http_package_importable() {
    Python::attach(|py| {
        let namespace = build_namespace(py);

        // Each of these fails with "No module named 'http.client'; 'http' is
        // not a package" when `sys.modules["http"]` is the namespace.
        for statement in [
            c"import http.client",
            c"import urllib.request",
            c"import xml.sax.saxutils",
            c"from http import HTTPStatus",
        ] {
            py.run(statement, None, None)
                .unwrap_or_else(|error| panic!("{statement:?} failed: {error}"));
        }

        let top_level = sys_modules(py)
            .get_item("http")
            .unwrap()
            .expect("the standard library http package is imported");
        assert!(
            !top_level.is(&namespace),
            "sys.modules['http'] is the siphon namespace"
        );
        assert!(
            top_level.hasattr("__path__").unwrap(),
            "sys.modules['http'] is not a package"
        );
    });
}

#[test]
fn the_namespace_is_the_module_siphon_http() {
    Python::attach(|py| {
        let namespace = build_namespace(py);
        let name: String = namespace.getattr("__name__").unwrap().extract().unwrap();
        assert_eq!(name, "siphon.http");
        let registered = sys_modules(py)
            .get_item("siphon.http")
            .unwrap()
            .expect("the namespace is registered as siphon.http");
        assert!(registered.is(&namespace));
        // What a script reaches through `from siphon import http`.
        for attribute in [
            "route",
            "middleware",
            "on_startup",
            "Request",
            "Response",
            "Client",
        ] {
            assert!(namespace.hasattr(attribute).unwrap(), "{attribute}");
        }
    });
}
