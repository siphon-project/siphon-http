//! Per-request cost of running a request through its middlewares and route
//! handler, with siphon's Python executor installed as a siphon binary
//! installs it.
//!
//! - `single_job` is the dispatcher: one executor job, one attach, per
//!   request; the tokio worker never attaches. `single_job_async_inline` is
//!   an `async def` handler that never awaits (run inline in the job), and
//!   `single_job_async` one that awaits (run on the asyncio pool).
//! - `per_stage` reproduces the dispatcher this replaced: every middleware
//!   and the handler was its own executor job, and the tokio worker attached
//!   to build the request object, to hand each stage its argument and to read
//!   each stage's result back.
//!
//! The tokio workers keep a persistent interpreter attach from
//! `on_thread_start`, as siphon's do, so `per_stage` is measured at its best.
//! Each row runs at concurrency 1 (latency) and 64 (throughput); the time
//! reported is per request.

// Bench harness: drives the runtime from the bench thread, which never attaches.
#![allow(clippy::disallowed_methods)]

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::extract::Request as AxumRequest;
use axum::http::{Request as HttpRequest, StatusCode};
use axum::response::Response as AxumResponse;
use axum::routing::any;
use axum::Router;
use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion};
use http_body_util::BodyExt;
use pyo3::prelude::*;
use siphon::script::async_pool::AsyncPool;
use siphon::script::py_executor::{try_run, ExecutorConfig, PyExecutor};
use tower::ServiceExt;

const SCRIPT: &str = r#"
import asyncio


def passes(req):
    return None


def handler(req):
    return Response(status=200, body=b"ok")


async def handler_async(req):
    await asyncio.sleep(0)
    return Response(status=200, body=b"ok")


async def handler_inline(req):
    return Response(status=200, body=b"ok")
"#;

fn runtime() -> &'static tokio::runtime::Runtime {
    static RUNTIME: std::sync::OnceLock<tokio::runtime::Runtime> = std::sync::OnceLock::new();
    RUNTIME.get_or_init(|| {
        Python::initialize();
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(4)
            .on_thread_start(|| {
                // What siphon does on its workers: attach once and keep the
                // thread state, so a later attach on the worker is a cheap
                // nested one.
                unsafe {
                    pyo3::ffi::PyGILState_Ensure();
                    pyo3::ffi::PyEval_SaveThread();
                }
            })
            .enable_all()
            .build()
            .expect("the bench runtime builds");
        PyExecutor::install(
            runtime.handle().clone(),
            ExecutorConfig {
                core_threads: 8,
                max_threads: 8,
                queue_capacity: 8192,
                stall_abort: None,
            },
        );
        AsyncPool::install(4, runtime.handle().clone(), Duration::from_secs(10));
        runtime
    })
}

struct Functions {
    passes: Py<PyAny>,
    handler: Py<PyAny>,
    handler_async: Py<PyAny>,
    handler_inline: Py<PyAny>,
}

fn functions() -> Functions {
    Python::attach(|py| {
        let module = PyModule::new(py, "siphon_http_dispatch_bench").unwrap();
        module
            .add("Response", py.get_type::<siphon_http::Response>())
            .unwrap();
        let code = std::ffi::CString::new(SCRIPT).unwrap();
        py.run(code.as_c_str(), Some(&module.dict()), None).unwrap();
        let get = |name: &str| module.getattr(name).unwrap().unbind();
        Functions {
            passes: get("passes"),
            handler: get("handler"),
            handler_async: get("handler_async"),
            handler_inline: get("handler_inline"),
        }
    })
}

fn clone(py: Python<'_>, function: &Py<PyAny>) -> Py<PyAny> {
    function.clone_ref(py)
}

fn single_job(
    functions: &Functions,
    middlewares: usize,
    handler: &Py<PyAny>,
    is_async: bool,
) -> Router {
    Python::attach(|py| {
        let middlewares = (0..middlewares)
            .map(|_| (clone(py, &functions.passes), false))
            .collect();
        siphon_http::__bench::router(middlewares, (clone(py, handler), is_async))
            .expect("the router builds")
    })
}

/// One stage as the replaced dispatcher ran it: the caller attaches to
/// clone the function, then an executor job calls it.
async fn per_stage_call(function: &Py<PyAny>, argument: Py<PyAny>) -> PyResult<Py<PyAny>> {
    let function = Python::attach(|py| function.clone_ref(py));
    match try_run(move || {
        Python::attach(|py| function.bind(py).call1((argument,)).map(Bound::unbind))
    })
    .await
    {
        Ok(result) => result,
        Err(_) => Err(pyo3::exceptions::PyRuntimeError::new_err(
            "dispatch panicked",
        )),
    }
}

fn internal_error() -> AxumResponse {
    let mut response = AxumResponse::new(Body::empty());
    *response.status_mut() = StatusCode::INTERNAL_SERVER_ERROR;
    response
}

fn per_stage(functions: &Functions, middlewares: usize) -> Router {
    let (middlewares, handler) = Python::attach(|py| {
        let middlewares: Vec<Py<PyAny>> = (0..middlewares)
            .map(|_| clone(py, &functions.passes))
            .collect();
        (
            Arc::new(middlewares),
            Arc::new(clone(py, &functions.handler)),
        )
    });
    Router::new().route(
        "/bench",
        any(move |request: AxumRequest| {
            let middlewares = Arc::clone(&middlewares);
            let handler = Arc::clone(&handler);
            async move {
                let (parts, body) = request.into_parts();
                let body = body.collect().await.unwrap().to_bytes();
                let request = siphon_http::__bench::request(&parts, body.to_vec());
                let Ok(request) = Python::attach(|py| Py::new(py, request).map(Py::into_any))
                else {
                    return internal_error();
                };
                for middleware in middlewares.iter() {
                    let argument = Python::attach(|py| request.clone_ref(py));
                    let Ok(returned) = per_stage_call(middleware, argument).await else {
                        return internal_error();
                    };
                    if !Python::attach(|py| returned.bind(py).is_none()) {
                        return internal_error();
                    }
                }
                let argument = Python::attach(|py| request.clone_ref(py));
                let Ok(returned) = per_stage_call(&handler, argument).await else {
                    return internal_error();
                };
                Python::attach(|py| {
                    match returned
                        .bind(py)
                        .extract::<PyRef<'_, siphon_http::Response>>()
                    {
                        Ok(response) => siphon_http::__bench::response(&response),
                        Err(_) => internal_error(),
                    }
                })
            }
        }),
    )
}

/// Time `iterations` requests through `router`, `concurrency` at a time.
fn drive(router: &Router, iterations: u64, concurrency: usize) -> Duration {
    let router = router.clone();
    runtime().block_on(async move {
        let started = Instant::now();
        let mut remaining = iterations as usize;
        while remaining > 0 {
            let batch = remaining.min(concurrency);
            remaining -= batch;
            let tasks: Vec<_> = (0..batch)
                .map(|_| {
                    let router = router.clone();
                    tokio::spawn(async move {
                        let request = HttpRequest::get("/bench").body(Body::empty()).unwrap();
                        let response = router.oneshot(request).await.unwrap();
                        assert_eq!(response.status(), StatusCode::OK);
                        response.into_body().collect().await.unwrap();
                    })
                })
                .collect();
            for task in tasks {
                task.await.unwrap();
            }
        }
        started.elapsed()
    })
}

fn bench(c: &mut Criterion) {
    runtime();
    let functions = functions();
    let mut group = c.benchmark_group("dispatch");
    for middlewares in [0usize, 3] {
        let rows = [
            ("per_stage", per_stage(&functions, middlewares)),
            (
                "single_job",
                single_job(&functions, middlewares, &functions.handler, false),
            ),
            (
                "single_job_async_inline",
                single_job(&functions, middlewares, &functions.handler_inline, true),
            ),
            (
                "single_job_async",
                single_job(&functions, middlewares, &functions.handler_async, true),
            ),
        ];
        for (name, router) in rows {
            for concurrency in [1usize, 64] {
                group.bench_with_input(
                    BenchmarkId::new(format!("{name}/mw{middlewares}"), format!("c{concurrency}")),
                    &concurrency,
                    |b, &concurrency| {
                        b.iter_custom(|iterations| drive(&router, iterations, concurrency))
                    },
                );
            }
        }
    }
    group.finish();
}

criterion_group!(benches, bench);
criterion_main!(benches);
