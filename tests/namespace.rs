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

/// Every test here builds the namespace, and CPython files it in
/// `sys.modules` as `siphon.http`: building it again re-runs `python/http.py`
/// in that same module object, putting the prototype classes back for a
/// moment. Tests running in parallel threads would see each other's module
/// mid-build, so they take turns. The lock is taken before attaching to the
/// interpreter, never while attached.
static NAMESPACE: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn attach<R>(body: impl for<'py> FnOnce(Python<'py>) -> R) -> R {
    let _turn = NAMESPACE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    Python::attach(body)
}

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
    attach(|py| {
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
    attach(|py| {
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

fn client_error(py: Python<'_>, arguments: &str) -> Option<String> {
    let namespace = build_namespace(py);
    let locals = PyDict::new(py);
    locals.set_item("http", &namespace).unwrap();
    let code = std::ffi::CString::new(format!(
        "try:\n    http.Client({arguments})\n    out = None\nexcept Exception as e:\n    out = type(e).__name__ + ': ' + str(e)\n"
    ))
    .unwrap();
    py.run(code.as_c_str(), None, Some(&locals)).unwrap();
    locals
        .get_item("out")
        .unwrap()
        .unwrap()
        .extract::<Option<String>>()
        .unwrap()
}

#[test]
fn an_inline_client_takes_resolve_and_redirects() {
    attach(|py| {
        for arguments in [
            r#"resolve={"host.example.com": ["192.0.2.10:443", "[2001:db8::1]:443"]}"#,
            r#"resolve={"host.example.com": "192.0.2.10:443"}"#,
            "redirects=False",
            "redirects=0",
            "redirects=3",
            r#"resolve={"host.example.com": ["192.0.2.10:443"]}, redirects=False"#,
        ] {
            assert_eq!(client_error(py, arguments), None, "{arguments}");
        }
    });
}

#[test]
fn an_inline_client_refuses_bad_resolve_and_redirects_at_construction() {
    attach(|py| {
        for (arguments, expected) in [
            (
                r#"resolve={"host.example.com": ["not-an-address"]}"#,
                "host.example.com",
            ),
            (
                r#"resolve={"host.example.com": ["192.0.2.10"]}"#,
                "host.example.com",
            ),
            (r#"resolve={"host.example.com": []}"#, "host.example.com"),
            (r#"resolve={"": ["192.0.2.10:443"]}"#, "host"),
            (r#"resolve={"host.example.com": [443]}"#, "host.example.com"),
            (r#"resolve=["192.0.2.10:443"]"#, "resolve"),
            ("redirects=-1", "redirects"),
            (r#"redirects="no""#, "redirects"),
        ] {
            let error = client_error(py, arguments)
                .unwrap_or_else(|| panic!("{arguments} should be refused"));
            assert!(error.contains(expected), "{arguments}: {error}");
        }
    });
}
