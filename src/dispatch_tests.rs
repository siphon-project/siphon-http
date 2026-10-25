//! A request through the router the addon builds, with siphon's Python
//! executor and asyncio pool installed the way a siphon binary installs them.
//!
//! The router runs on a tokio runtime whose workers mark themselves (from
//! `on_thread_park`, which only workers run), so a test can tell whether
//! Python ran on one. Several tests also force garbage-collector
//! stop-the-world pauses from other threads while requests are in flight: a
//! thread that blocks while attached, or a tokio worker parked in an attach,
//! turns those pauses into a stall, which shows up here as a missed deadline.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::http::{Method, Request as HttpRequest};
use axum::Router;
use http_body_util::BodyExt;
use pyo3::prelude::*;
use siphon::script::async_pool::AsyncPool;
use siphon::script::py_executor::{ExecutorConfig, PyExecutor};
use tower::ServiceExt;

use crate::dispatch::{probe, Stage};
use crate::runtime::{build_router, RouteOptions};
use crate::HttpConfig;

const WORKERS: usize = 4;

/// How long any batch of requests here may take. Generous: a healthy run
/// finishes in well under a second, a stalled one never does.
const DEADLINE: Duration = Duration::from_secs(30);

static PARKED_WORKERS: AtomicUsize = AtomicUsize::new(0);

/// The tokio runtime the tests serve on, with the executor, the asyncio pool
/// and pyo3-async-runtimes all bound to it.
fn runtime() -> &'static tokio::runtime::Runtime {
    static RUNTIME: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    static POOLS: OnceLock<()> = OnceLock::new();
    let runtime = RUNTIME.get_or_init(|| {
        Python::initialize();
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(WORKERS)
            .thread_name("http-test-runtime")
            .on_thread_park(|| {
                if probe::mark_async_worker() {
                    PARKED_WORKERS.fetch_add(1, Ordering::SeqCst);
                }
            })
            .enable_all()
            .build()
            .expect("the test runtime builds")
    });
    POOLS.get_or_init(|| {
        let handle = runtime.handle().clone();
        PyExecutor::install(
            handle.clone(),
            ExecutorConfig {
                core_threads: 4,
                max_threads: 8,
                queue_capacity: 8192,
                stall_abort: None,
            },
        );
        AsyncPool::install(2, handle, Duration::from_secs(10));
        let _ = pyo3_async_runtimes::tokio::init_with_runtime(runtime);
        // Every worker has to have marked itself before a test relies on the
        // mark; an idle worker parks at once.
        let started = Instant::now();
        while PARKED_WORKERS.load(Ordering::SeqCst) < WORKERS {
            assert!(
                started.elapsed() < Duration::from_secs(5),
                "the test runtime's workers never parked"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    });
    runtime
}

/// The tests that count attaches must not overlap.
fn serial() -> MutexGuard<'static, ()> {
    static SERIAL: Mutex<()> = Mutex::new(());
    SERIAL
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Whether the calling thread is a tokio worker, for scripts to report.
#[pyfunction]
fn on_async_worker() -> bool {
    probe::is_async_worker()
}

/// The name of the calling thread, for scripts to report.
#[pyfunction]
fn thread_name() -> String {
    std::thread::current().name().unwrap_or("").to_string()
}

const SCRIPT: &str = r#"
import asyncio


def passes(req):
    return None


async def passes_async(req):
    await asyncio.sleep(0)
    return None


def blocks(req):
    if req.header("x-block"):
        return Response(status=403, body=b"blocked")
    return None


async def blocks_async(req):
    await asyncio.sleep(0)
    if req.header("x-block"):
        return Response(status=403, body=b"blocked async")
    return None


def returns_junk(req):
    return 42


def raises(req):
    raise ValueError("nope")


def where(req):
    return Response(status=200, body=f"worker={on_async_worker()}")


async def where_async(req):
    await asyncio.sleep(0)
    return Response(status=200, body=f"worker={on_async_worker()}")


async def never_awaits(req):
    return Response(status=201, body=thread_name())


async def passes_without_awaiting(req):
    return None


async def raises_without_awaiting(req):
    raise ValueError("nope")


async def suspends(req):
    await asyncio.sleep(0)
    return Response(status=202, body=thread_name())


async def raises_async(req):
    await asyncio.sleep(0)
    raise ValueError("nope")


def not_a_response(req):
    return "text"


async def via_client(req):
    async with Client(base_url=req.header("x-upstream")) as client:
        reply = await client.get("/pong")
    return Response(status=reply.status, body=reply.body)


async def script_handler(base):
    # What a siphon script handler does when it calls out over HTTP.
    async with Client(base_url=base) as client:
        reply = await client.get("/pong")
    return (reply.status, reply.body)
"#;

/// The test script's functions.
fn script(py: Python<'_>) -> Bound<'_, PyModule> {
    static MODULE: pyo3::sync::PyOnceLock<Py<PyModule>> = pyo3::sync::PyOnceLock::new();
    MODULE
        .get_or_init(py, || {
            let module = PyModule::new(py, "siphon_http_dispatch_test").unwrap();
            module
                .add("Response", py.get_type::<crate::Response>())
                .unwrap();
            module
                .add("Client", py.get_type::<crate::Client>())
                .unwrap();
            module
                .add_function(wrap_pyfunction!(on_async_worker, &module).unwrap())
                .unwrap();
            module
                .add_function(wrap_pyfunction!(thread_name, &module).unwrap())
                .unwrap();
            let code = std::ffi::CString::new(SCRIPT).unwrap();
            py.run(code.as_c_str(), Some(&module.dict()), None)
                .expect("the test script runs");
            module.unbind()
        })
        .bind(py)
        .clone()
}

fn stage(py: Python<'_>, name: &str) -> Stage {
    let function = script(py).getattr(name).expect("the script defines it");
    let is_async = py
        .import("inspect")
        .unwrap()
        .call_method1("iscoroutinefunction", (&function,))
        .unwrap()
        .extract::<bool>()
        .unwrap();
    Stage::new(function.unbind(), is_async)
}

/// A router serving `GET /<handler>` for each handler, behind `middlewares`.
fn router(middlewares: &[&str], handlers: &[&str]) -> Router {
    let (routes, middlewares) = Python::attach(|py| {
        let routes = handlers
            .iter()
            .map(|name| {
                (
                    RouteOptions {
                        path: format!("/{name}"),
                        methods: vec![Method::GET],
                        listeners: None,
                    },
                    stage(py, name),
                )
            })
            .collect::<Vec<_>>();
        let middlewares = middlewares.iter().map(|name| stage(py, name)).collect();
        (routes, Arc::new(middlewares))
    });
    build_router(routes, middlewares, &HttpConfig::default())
        .expect("the router builds")
        .0
}

/// Send one request through `router` from a task on the runtime's workers.
async fn send(router: Router, path: String, headers: Vec<(&'static str, String)>) -> (u16, String) {
    let task = tokio::spawn(async move {
        let mut request = HttpRequest::get(path);
        for (name, value) in headers {
            request = request.header(name, value);
        }
        let response = router
            .oneshot(request.body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = response.status().as_u16();
        let body = response.into_body().collect().await.unwrap().to_bytes();
        (status, String::from_utf8_lossy(&body).into_owned())
    });
    task.await.expect("the request task completes")
}

fn get(router: &Router, path: &str) -> (u16, String) {
    get_with(router, path, Vec::new())
}

fn get_with(router: &Router, path: &str, headers: Vec<(&'static str, String)>) -> (u16, String) {
    let router = router.clone();
    let path = path.to_string();
    runtime().block_on(async move {
        tokio::time::timeout(DEADLINE, send(router, path, headers))
            .await
            .expect("the request completes within the deadline")
    })
}

/// Collect garbage over and over from `threads` threads until dropped. Each
/// `gc.collect()` on free-threaded CPython stops the world.
struct GarbageCollector {
    stop: Arc<AtomicBool>,
    threads: Vec<std::thread::JoinHandle<usize>>,
}

impl GarbageCollector {
    fn start(threads: usize) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let threads = (0..threads)
            .map(|_| {
                let stop = Arc::clone(&stop);
                std::thread::spawn(move || {
                    let mut collections = 0;
                    while !stop.load(Ordering::Relaxed) {
                        Python::attach(|py| {
                            py.import("gc")
                                .and_then(|gc| gc.call_method0("collect"))
                                .expect("gc.collect runs");
                        });
                        collections += 1;
                        std::thread::sleep(Duration::from_micros(200));
                    }
                    collections
                })
            })
            .collect();
        Self { stop, threads }
    }

    /// Stop, and return how many collections ran.
    fn finish(mut self) -> usize {
        self.stop.store(true, Ordering::Relaxed);
        self.threads
            .drain(..)
            .map(|thread| thread.join().expect("the collector thread"))
            .sum()
    }
}

impl Drop for GarbageCollector {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// `count` concurrent requests to each path, all within the deadline.
fn hammer(router: &Router, paths: &[&str], count: usize) -> Vec<(u16, String)> {
    let router = router.clone();
    let paths: Vec<String> = paths.iter().map(|path| path.to_string()).collect();
    runtime().block_on(async move {
        let mut tasks = Vec::new();
        for index in 0..count {
            for path in &paths {
                let router = router.clone();
                let path = path.clone();
                tasks.push(tokio::spawn(async move {
                    send(router, path, vec![("x-index", index.to_string())]).await
                }));
            }
        }
        let all = async {
            let mut answers = Vec::new();
            for task in tasks {
                answers.push(task.await.expect("the request task completes"));
            }
            answers
        };
        tokio::time::timeout(DEADLINE, all)
            .await
            .expect("every request completes within the deadline")
    })
}

#[test]
fn a_sync_chain_runs_off_the_tokio_workers_in_one_attach() {
    let _serial = serial();
    let router = router(&["passes", "passes", "passes"], &["where"]);
    // Warm up: the first request compiles the dispatch helpers.
    assert_eq!(get(&router, "/where"), (200, "worker=False".to_string()));

    let before = probe::all();
    const REQUESTS: usize = 50;
    for _ in 0..REQUESTS {
        assert_eq!(get(&router, "/where"), (200, "worker=False".to_string()));
    }
    assert_eq!(
        probe::all() - before,
        REQUESTS,
        "a request through three middlewares and a handler attaches exactly once"
    );
    assert_eq!(probe::on_async_workers(), 0);
}

#[test]
fn an_async_handler_runs_on_the_asyncio_pool_not_a_tokio_worker() {
    let _serial = serial();
    let router = router(
        &["passes", "passes_async", "passes"],
        &["where", "where_async"],
    );
    assert_eq!(get(&router, "/where"), (200, "worker=False".to_string()));
    assert_eq!(
        get(&router, "/where_async"),
        (200, "worker=False".to_string())
    );

    let before = probe::all();
    assert_eq!(
        get(&router, "/where_async"),
        (200, "worker=False".to_string())
    );
    assert_eq!(
        probe::all() - before,
        1,
        "the job hands the async stages over within its one attach"
    );
    assert_eq!(probe::on_async_workers(), 0);
}

#[test]
fn an_async_stage_that_cannot_suspend_runs_inline_in_the_job() {
    let _serial = serial();
    let router = router(&["passes_without_awaiting"], &["never_awaits", "suspends"]);
    let (status, thread) = get(&router, "/never_awaits");
    assert_eq!(status, 201);
    assert!(
        thread.starts_with("siphon-pyexec-"),
        "ran on {thread:?}, not inline on the executor"
    );
    // One that can suspend still goes to an asyncio driver.
    let (status, thread) = get(&router, "/suspends");
    assert_eq!(status, 202);
    assert!(
        thread.starts_with("siphon-asyncio-"),
        "ran on {thread:?}, not on the asyncio pool"
    );
    // A raise from the inline path is the handler's error, as on the pool.
    let raising = self::router(&["raises_without_awaiting"], &["where"]);
    assert_eq!(
        get(&raising, "/where"),
        (500, "middleware error\n".to_string())
    );
}

#[test]
fn a_middleware_answers_instead_of_the_handler() {
    let _serial = serial();
    for middleware in ["blocks", "blocks_async"] {
        let router = router(&["passes", middleware], &["where"]);
        let blocked = get_with(&router, "/where", vec![("x-block", "1".to_string())]);
        assert_eq!(blocked.0, 403, "{middleware}");
        assert!(
            blocked.1.starts_with("blocked"),
            "{middleware}: {}",
            blocked.1
        );
        assert_eq!(
            get(&router, "/where"),
            (200, "worker=False".to_string()),
            "{middleware}"
        );
    }
}

#[test]
fn script_errors_are_answered_with_500() {
    let _serial = serial();
    let cases: [(&[&str], &str, &str); 6] = [
        (
            &["returns_junk"],
            "where",
            "middleware must return Response or None\n",
        ),
        (&["raises"], "where", "middleware error\n"),
        (&[], "raises", "handler error\n"),
        (&[], "raises_async", "handler error\n"),
        (&[], "not_a_response", "bad response\n"),
        (
            &["passes_async", "returns_junk"],
            "where",
            "middleware must return Response or None\n",
        ),
    ];
    for (middlewares, handler, body) in cases {
        let router = router(middlewares, &[handler]);
        assert_eq!(
            get(&router, &format!("/{handler}")),
            (500, body.to_string()),
            "{middlewares:?} -> {handler}"
        );
    }
}

#[test]
fn requests_complete_while_the_garbage_collector_stops_the_world() {
    let _serial = serial();
    let bare = router(&[], &["where", "where_async"]);
    let guarded = router(
        &["passes", "passes_async", "blocks"],
        &["where", "where_async"],
    );
    // Compile the helpers before the clock starts.
    get(&bare, "/where");

    let collector = GarbageCollector::start(2);
    let started = Instant::now();
    let mut answers = hammer(&bare, &["/where", "/where_async"], 200);
    answers.extend(hammer(&guarded, &["/where", "/where_async"], 200));
    let elapsed = started.elapsed();
    let collections = collector.finish();

    assert_eq!(answers.len(), 800);
    for answer in &answers {
        assert_eq!(answer, &(200, "worker=False".to_string()));
    }
    assert!(collections > 0, "the collector never ran");
    assert!(elapsed < DEADLINE, "took {elapsed:?}");
    assert_eq!(probe::on_async_workers(), 0);
}

/// An upstream for `http.Client` to call: `GET /pong` answers `pong`.
fn upstream() -> String {
    static UPSTREAM: OnceLock<String> = OnceLock::new();
    UPSTREAM
        .get_or_init(|| {
            let listener = runtime()
                .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
                .expect("the upstream binds");
            let address = listener.local_addr().expect("the upstream address");
            let app = Router::new().route("/pong", axum::routing::get(|| async { "pong" }));
            runtime().spawn(async move { axum::serve(listener, app).await });
            format!("http://{address}")
        })
        .clone()
}

#[test]
fn a_handler_awaiting_the_client_completes_while_the_world_stops() {
    let _serial = serial();
    let base = upstream();
    let router = router(&["passes"], &["via_client"]);
    assert_eq!(
        get_with(&router, "/via_client", vec![("x-upstream", base.clone())]),
        (200, "pong".to_string())
    );

    let collector = GarbageCollector::start(2);
    let router_for_load = router.clone();
    let answers = runtime().block_on(async move {
        let mut tasks = Vec::new();
        for _ in 0..200 {
            let router = router_for_load.clone();
            let base = base.clone();
            tasks.push(tokio::spawn(async move {
                send(
                    router,
                    "/via_client".to_string(),
                    vec![("x-upstream", base)],
                )
                .await
            }));
        }
        let all = async {
            let mut answers = Vec::new();
            for task in tasks {
                answers.push(task.await.expect("the request task completes"));
            }
            answers
        };
        tokio::time::timeout(DEADLINE, all)
            .await
            .expect("every request completes within the deadline")
    });
    assert!(collector.finish() > 0, "the collector never ran");
    for answer in answers {
        assert_eq!(answer, (200, "pong".to_string()));
    }
    assert_eq!(probe::on_async_workers(), 0);
}

/// A script handler outside this addon (siphon's own, say a REGISTER
/// handler) awaiting `http.Client` on siphon's asyncio pool, while the world
/// stops.
#[test]
fn a_script_coroutine_awaiting_the_client_completes_while_the_world_stops() {
    let _serial = serial();
    let base = upstream();
    let pool = {
        runtime();
        AsyncPool::global().expect("the asyncio pool is installed")
    };

    let collector = GarbageCollector::start(2);
    let receivers = Python::attach(|py| {
        let handler = script(py).getattr("script_handler").unwrap();
        (0..200)
            .map(|_| {
                let coroutine = handler.call1((base.as_str(),)).unwrap();
                let (receiver, _future) = pool.submit(py, &coroutine).unwrap();
                receiver
            })
            .collect::<Vec<_>>()
    });
    let results = runtime().block_on(async move {
        let all = async {
            let mut results = Vec::new();
            for receiver in receivers {
                results.push(receiver.await.expect("the coroutine settles"));
            }
            results
        };
        tokio::time::timeout(DEADLINE, all)
            .await
            .expect("every coroutine completes within the deadline")
    });
    assert!(collector.finish() > 0, "the collector never ran");
    Python::attach(|py| {
        for result in results {
            let (status, body): (u16, Vec<u8>) = result
                .expect("the coroutine returns")
                .bind(py)
                .extract()
                .unwrap();
            assert_eq!((status, body.as_slice()), (200, b"pong".as_slice()));
        }
    });
    assert_eq!(probe::on_async_workers(), 0);
}

/// No Python function runs on a tokio worker for a request, through the
/// dispatcher or `http.Client`: `sys.monitoring` reports every Python frame
/// started on any thread, and the hook asks whether its thread is a worker.
#[test]
fn no_python_runs_on_a_tokio_worker() {
    let _serial = serial();
    let base = upstream();
    let router = router(
        &["passes", "passes_async", "blocks"],
        &["where", "where_async", "via_client"],
    );
    // Warm up before watching.
    get(&router, "/where");

    const TOOL: &std::ffi::CStr = c"
import sys

TOOL_ID = 4
seen = []


def install(on_async_worker):
    def started(code, offset):
        if on_async_worker():
            seen.append(code.co_qualname)
    sys.monitoring.use_tool_id(TOOL_ID, 'siphon-http-test')
    sys.monitoring.register_callback(TOOL_ID, sys.monitoring.events.PY_START, started)
    sys.monitoring.set_events(TOOL_ID, sys.monitoring.events.PY_START)


def remove():
    sys.monitoring.set_events(TOOL_ID, 0)
    sys.monitoring.register_callback(TOOL_ID, sys.monitoring.events.PY_START, None)
    sys.monitoring.free_tool_id(TOOL_ID)
";
    let monitor = Python::attach(|py| {
        let module = PyModule::from_code(py, TOOL, c"monitor.py", c"siphon_http_monitor").unwrap();
        let probe_function = script(py).getattr("on_async_worker").unwrap();
        module
            .getattr("install")
            .unwrap()
            .call1((probe_function,))
            .unwrap();
        module.unbind()
    });

    let mut answers = hammer(&router, &["/where", "/where_async"], 50);
    for _ in 0..20 {
        answers.push(get_with(
            &router,
            "/via_client",
            vec![("x-upstream", base.clone())],
        ));
    }

    let seen: Vec<String> = Python::attach(|py| {
        let module = monitor.bind(py);
        module.getattr("remove").unwrap().call0().unwrap();
        module.getattr("seen").unwrap().extract().unwrap()
    });
    for answer in &answers {
        assert_eq!(answer.0, 200, "{answer:?}");
    }
    assert!(seen.is_empty(), "Python ran on a tokio worker: {seen:?}");
}

/// Every attach in the crate goes through the counted helper, so the probe
/// above sees all of them.
#[test]
fn every_attach_goes_through_the_counted_helper() {
    let source = concat!(env!("CARGO_MANIFEST_DIR"), "/src");
    let mut offenders = Vec::new();
    for entry in std::fs::read_dir(source).unwrap() {
        let path = entry.unwrap().path();
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        if !name.ends_with(".rs") || name.ends_with("_tests.rs") {
            continue;
        }
        let text = std::fs::read_to_string(&path).unwrap();
        // Inline test modules may attach as they like.
        let production = text.split("#[cfg(test)]\nmod tests").next().unwrap_or("");
        let count = production.matches("Python::attach(").count();
        let allowed = usize::from(name == "dispatch.rs");
        if count != allowed {
            offenders.push(format!("{name}: {count}"));
        }
    }
    assert!(
        offenders.is_empty(),
        "attach outside crate::dispatch::attach: {offenders:?}"
    );
}

/// The façade registers each route and middleware with its function in the
/// options, which is where the runtime reads it from.
#[test]
fn the_decorators_register_the_function_they_wrap() {
    Python::attach(|py| {
        let namespace = crate::namespace(HttpConfig::default())(py).expect("the namespace builds");
        let globals = pyo3::types::PyDict::new(py);
        globals.set_item("http", namespace).unwrap();
        py.run(
            c"import sys, types
registry = types.ModuleType('_siphon_registry')
registry.calls = []
registry.register = lambda *args: registry.calls.append(args)
sys.modules['_siphon_registry'] = registry

@http.route('/a')
def route_handler(req):
    return None

@http.middleware
async def guard(req):
    return None

registered = [(call[0], call[4]['handler'] is call[2], call[3]) for call in registry.calls]
",
            Some(&globals),
            None,
        )
        .expect("the decorators run");
        let registered: Vec<(String, bool, bool)> = globals
            .get_item("registered")
            .unwrap()
            .expect("registered")
            .extract()
            .unwrap();
        assert_eq!(
            registered,
            vec![
                ("http.route".to_string(), true, false),
                ("http.middleware".to_string(), true, true),
            ]
        );
    });
}

/// Every awaitable the crate hands Python goes through siphon's `awaitable`,
/// which completes on siphon's fixed, pinned completion threads. A direct
/// `pyo3_async_runtimes` `future_into_py` completes on tokio's blocking pool
/// instead, whose threads come and go, and sets up an interpreter thread state
/// on each call.
#[test]
fn no_awaitable_bypasses_siphon() {
    let source = concat!(env!("CARGO_MANIFEST_DIR"), "/src");
    let mut offenders = Vec::new();
    for entry in std::fs::read_dir(source).unwrap() {
        let path = entry.unwrap().path();
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        if !name.ends_with(".rs") || name.ends_with("_tests.rs") {
            continue;
        }
        let text = std::fs::read_to_string(&path).unwrap();
        let production = text.split("#[cfg(test)]\nmod tests").next().unwrap_or("");
        for (number, line) in production.lines().enumerate() {
            let code = line.split("//").next().unwrap_or("");
            if code.contains("future_into_py") {
                offenders.push(format!("{name}:{}", number + 1));
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "use siphon::script::blocking::awaitable instead: {offenders:?}"
    );
}
