//! Running a request through its middlewares and route handler.
//!
//! A route whose stages are all plain `def` runs as one job on siphon's
//! Python executor. That job builds the `Request` object, runs the
//! middlewares in order and then the route handler, and turns what they
//! return into the HTTP response, all inside a single attach to the
//! interpreter. The axum task that accepted the request never touches
//! Python: it submits the job and awaits its answer.
//!
//! A route with an `async def` stage runs as one coroutine (`drive` below)
//! started through siphon's `ScriptHandle::start_coroutine`, the path a
//! script's own `async def` handlers take. The coroutine is built and first
//! stepped in an executor job; one that never suspends finishes there. One
//! that awaits only siphon's awaitables (`http.Client`, siphon's own async
//! APIs) is resumed by the executor as each completes, with no asyncio task
//! and no hop to an asyncio thread. One that awaits anything else (asyncio
//! itself) is handed to siphon's asyncio driver intact. Every one of them is
//! bounded by siphon's coroutine timeout: on expiry it is cancelled and the
//! request is answered 500.
//!
//! Keeping Python off the tokio workers matters on free-threaded CPython. A
//! thread that attaches takes part in every stop-the-world pause the garbage
//! collector makes, and a tokio worker parked in an attach stalls every task
//! scheduled on it until the pause ends.

use std::sync::{Arc, OnceLock};
use std::time::Instant;

use axum::body::Body;
use axum::http::{HeaderName, HeaderValue, StatusCode};
use axum::response::Response as AxumResponse;
use pyo3::prelude::*;
use pyo3::sync::PyOnceLock;
use pyo3::types::PyTuple;
use siphon::script::{HandlerHandle, ScriptHandle};

use crate::timing::HandlerStamps;

use crate::{Request as PyRequest, Response as PyResponse};

/// Attach to the interpreter. Every attach this crate makes goes through
/// here, so the tests can tell whether one happened on a tokio worker.
pub(crate) fn attach<F, R>(f: F) -> R
where
    F: for<'py> FnOnce(Python<'py>) -> R,
{
    #[cfg(test)]
    probe::note_attach();
    Python::attach(f)
}

/// One script function a request passes through: a middleware or a route
/// handler.
pub(crate) struct Stage {
    callable: Py<PyAny>,
    is_async: bool,
}

impl Stage {
    pub(crate) fn new(callable: Py<PyAny>, is_async: bool) -> Self {
        Self { callable, is_async }
    }

    /// The function the façade registered, read from the options it
    /// registered it with (`python/http.py` stores it under `handler`).
    pub(crate) fn from_handle(py: Python<'_>, handle: &HandlerHandle) -> PyResult<Self> {
        let options = handle.options(py).ok_or_else(|| {
            pyo3::exceptions::PyValueError::new_err(format!("{} has no options", handle.kind()))
        })?;
        let callable = options.get_item("handler")?.ok_or_else(|| {
            pyo3::exceptions::PyValueError::new_err(format!(
                "{} options carry no handler",
                handle.kind()
            ))
        })?;
        Ok(Self::new(callable.unbind(), handle.is_async()))
    }
}

/// A route handler together with the middlewares that run before it.
pub(crate) struct Route {
    middlewares: Arc<Vec<Stage>>,
    handler: Stage,
    /// Every stage as `(function, is_async, is_middleware)`, built once, for
    /// the coroutine of a route with an `async def` stage.
    stages: Py<PyTuple>,
    /// Some stage is an `async def`: the route runs as a coroutine of this
    /// route's own `drive` (see [`HELPERS_SOURCE`]).
    is_async: bool,
    drive: Py<PyAny>,
    /// Where a coroutine is started.
    script: ScriptHandle,
}

impl Route {
    pub(crate) fn new(
        py: Python<'_>,
        middlewares: Arc<Vec<Stage>>,
        handler: Stage,
        script: ScriptHandle,
    ) -> PyResult<Self> {
        let mut entries = Vec::with_capacity(middlewares.len() + 1);
        for stage in middlewares.iter() {
            entries
                .push((stage.callable.bind(py).clone(), stage.is_async, true).into_pyobject(py)?);
        }
        entries
            .push((handler.callable.bind(py).clone(), handler.is_async, false).into_pyobject(py)?);
        let stages = PyTuple::new(py, entries)?.unbind();
        let is_async = handler.is_async || middlewares.iter().any(|stage| stage.is_async);
        let drive = helpers(py)?
            .make_drive
            .bind(py)
            .call1((handler.callable.bind(py),))?
            .unbind();
        Ok(Self {
            middlewares,
            handler,
            stages,
            is_async,
            drive,
            script,
        })
    }
}

/// A response as the job hands it back. A script's `Response` is copied out
/// as plain data and turned into an HTTP response on the tokio worker, so the
/// executor spends no time on header parsing or response building.
pub(crate) enum Reply {
    Script(ScriptReply),
    Ready(AxumResponse),
}

/// The fields of a script's `Response`.
pub(crate) struct ScriptReply {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl ScriptReply {
    fn from(py: Python<'_>, response: &PyResponse) -> PyResult<Self> {
        Ok(Self {
            status: response.status,
            headers: response.header_pairs(py)?,
            body: response.body_bytes.clone(),
        })
    }

    fn into_response(self) -> AxumResponse {
        let status = StatusCode::from_u16(self.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        let mut builder = AxumResponse::builder().status(status);
        for (name, value) in &self.headers {
            if let (Ok(name), Ok(value)) = (
                HeaderName::from_bytes(name.as_bytes()),
                HeaderValue::from_str(value),
            ) {
                builder = builder.header(name, value);
            }
        }
        builder
            .body(Body::from(self.body))
            .unwrap_or_else(|_| error_response(StatusCode::INTERNAL_SERVER_ERROR, "build failed"))
    }
}

impl Reply {
    fn into_response(self) -> AxumResponse {
        match self {
            Reply::Script(reply) => reply.into_response(),
            Reply::Ready(response) => response,
        }
    }

    fn error(status: StatusCode, message: &str) -> Self {
        Reply::Ready(error_response(status, message))
    }
}

/// Answer `request` through `route`. Called on a tokio worker; does not
/// attach.
pub(crate) async fn answer(route: Arc<Route>, request: PyRequest) -> AxumResponse {
    if route.is_async {
        return answer_coroutine(route, request).await;
    }
    let job = siphon::script::py_executor::try_run(move || attach(|py| start(py, &route, request)));
    match job.await {
        Ok((reply, stamps)) => stamped(reply, stamps),
        Err(_) => {
            tracing::error!(
                target: "siphon_http",
                "http request could not run on the Python executor (queue full or job panicked)"
            );
            error_response(StatusCode::INTERNAL_SERVER_ERROR, "internal error")
        }
    }
}

/// Answer `request` through a route with an `async def` stage: its stages
/// run as one `drive` coroutine, started the way siphon starts a script's
/// own `async def` handler.
async fn answer_coroutine(route: Arc<Route>, request: PyRequest) -> AxumResponse {
    // Set by `build` when the executor job starts, read by `finish`.
    let job_started = Arc::new(OnceLock::new());
    let started = Arc::clone(&job_started);
    let script = route.script.clone();
    let outcome = script
        .start_coroutine(
            // siphon keys what it learns about asyncio use on the coroutine's
            // code object; every route has its own `drive`, so that is the
            // route. A key object would have to be cloned here, on a tokio
            // worker, which cannot be done without attaching.
            None,
            move |py| {
                // A clock read, nothing else: the timing layer turns it into
                // metrics on the tokio side.
                let _ = started.set(Instant::now());
                let request = Py::new(py, request)?;
                route.drive.bind(py).call1((route.stages.bind(py), request))
            },
            move |_py, outcome| {
                let finished = Instant::now();
                let reply = match outcome {
                    Ok(value) => drive_outcome(&value),
                    // Building or starting the coroutine failed, or it ran
                    // past siphon's coroutine timeout and was cancelled.
                    Err(error) => stage_raised(&error, false),
                };
                let stamps = HandlerStamps {
                    job_started: job_started.get().copied().unwrap_or(finished),
                    finished,
                };
                (reply, stamps)
            },
        )
        .await;
    match outcome {
        Ok((reply, stamps)) => stamped(reply, stamps),
        Err(error) => {
            tracing::error!(
                target: "siphon_http",
                %error,
                "http handler coroutine could not run on the Python executor"
            );
            error_response(StatusCode::INTERNAL_SERVER_ERROR, "internal error")
        }
    }
}

/// The reply for what `drive` returned: `(value, is_middleware, raised)`.
fn drive_outcome(outcome: &Bound<'_, PyAny>) -> Reply {
    match outcome.extract::<(Bound<'_, PyAny>, bool, bool)>() {
        Ok((error, is_middleware, true)) => stage_raised(&PyErr::from_value(error), is_middleware),
        Ok((value, is_middleware, false)) => stage_response(&value, is_middleware),
        Err(error) => {
            tracing::error!(target: "siphon_http", %error, "http handler coroutine returned no outcome");
            Reply::error(StatusCode::INTERNAL_SERVER_ERROR, "internal error")
        }
    }
}

/// The response for `reply`, carrying its stamps to the timing layer.
fn stamped(reply: Reply, stamps: HandlerStamps) -> AxumResponse {
    let mut response = reply.into_response();
    response.extensions_mut().insert(stamps);
    response
}

/// The executor job of a route whose stages are all `def`: build the
/// `Request` object and run the stages until one answers.
pub(crate) fn start(py: Python<'_>, route: &Route, request: PyRequest) -> (Reply, HandlerStamps) {
    // A clock read, nothing else: the timing layer turns it into metrics on
    // the tokio side.
    let job_started = Instant::now();
    let reply = run_stages(py, route, request);
    let stamps = HandlerStamps {
        job_started,
        finished: Instant::now(),
    };
    (reply, stamps)
}

fn run_stages(py: Python<'_>, route: &Route, request: PyRequest) -> Reply {
    let request = match Py::new(py, request) {
        Ok(request) => request.into_any().into_bound(py),
        Err(error) => {
            tracing::error!(target: "siphon_http", %error, "Py::new(Request) failed");
            return Reply::error(StatusCode::INTERNAL_SERVER_ERROR, "internal error");
        }
    };
    let middlewares = route.middlewares.len();
    let stages = route
        .middlewares
        .iter()
        .chain(std::iter::once(&route.handler));
    for (index, stage) in stages.enumerate() {
        let is_middleware = index < middlewares;
        match stage.callable.bind(py).call1((&request,)) {
            Ok(value) if is_middleware && value.is_none() => continue,
            Ok(value) => return stage_response(&value, is_middleware),
            Err(error) => return stage_raised(&error, is_middleware),
        }
    }
    // The route handler is the last stage and always answers.
    Reply::error(StatusCode::INTERNAL_SERVER_ERROR, "internal error")
}

/// The coroutine a route with an `async def` stage runs as. It returns
/// `(value, is_middleware, raised)` rather than raising, so the reply can
/// tell a middleware's failure from the handler's. Cancellation (siphon's
/// coroutine timeout) is not an `Exception` and passes through.
///
/// Every route gets its own copy of `drive` (`make_drive`): siphon sends a
/// coroutine whose code once used asyncio straight to its asyncio driver from
/// then on, and it knows the coroutine by its code object. With one shared
/// `drive`, the first handler to use asyncio would move every route there.
/// The copy's qualified name carries the handler's, which is what siphon logs
/// when it moves one.
const HELPERS_SOURCE: &str = r#"
import types


def make_drive(handler):
    name = getattr(handler, "__qualname__", "handler")
    code = drive.__code__.replace(co_qualname=f"drive[{name}]")
    return types.FunctionType(code, drive.__globals__, "drive")


async def drive(stages, request):
    is_middleware = False
    try:
        for handler, is_async, is_middleware in stages:
            value = handler(request)
            if is_async:
                value = await value
            if is_middleware and value is None:
                continue
            return (value, is_middleware, False)
    except Exception as error:
        return (error, is_middleware, True)
    return (None, False, False)
"#;

struct Helpers {
    make_drive: Py<PyAny>,
}

static HELPERS: PyOnceLock<Helpers> = PyOnceLock::new();

fn helpers(py: Python<'_>) -> PyResult<&Helpers> {
    HELPERS.get_or_try_init(py, || {
        let module = PyModule::from_code(
            py,
            &std::ffi::CString::new(HELPERS_SOURCE)?,
            c"siphon_http_dispatch.py",
            c"siphon_http_dispatch",
        )?;
        Ok(Helpers {
            make_drive: module.getattr("make_drive")?.unbind(),
        })
    })
}

/// The response for what a stage returned. A middleware's `None` never gets
/// here: it means "continue".
fn stage_response(value: &Bound<'_, PyAny>, is_middleware: bool) -> Reply {
    match value.extract::<PyRef<'_, PyResponse>>() {
        Ok(response) => script_reply(value.py(), &response),
        Err(error) if is_middleware => {
            tracing::warn!(
                target: "siphon_http",
                %error,
                "http.middleware returned neither Response nor None"
            );
            Reply::error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "middleware must return Response or None",
            )
        }
        Err(error) => {
            tracing::warn!(target: "siphon_http", %error, "handler did not return Response");
            Reply::error(StatusCode::INTERNAL_SERVER_ERROR, "bad response")
        }
    }
}

fn stage_raised(error: &PyErr, is_middleware: bool) -> Reply {
    if is_middleware {
        tracing::warn!(target: "siphon_http", %error, "http.middleware raised");
        Reply::error(StatusCode::INTERNAL_SERVER_ERROR, "middleware error")
    } else {
        tracing::warn!(target: "siphon_http", %error, "handler raised");
        Reply::error(StatusCode::INTERNAL_SERVER_ERROR, "handler error")
    }
}

/// The HTTP response for a script's `Response`.
pub(crate) fn build_axum_response(py: Python<'_>, response: &PyResponse) -> AxumResponse {
    script_reply(py, response).into_response()
}

fn script_reply(py: Python<'_>, response: &PyResponse) -> Reply {
    match ScriptReply::from(py, response) {
        Ok(reply) => Reply::Script(reply),
        Err(error) => {
            tracing::error!(target: "siphon_http", %error, "invalid Response headers");
            Reply::error(StatusCode::INTERNAL_SERVER_ERROR, "bad response")
        }
    }
}

pub(crate) fn error_response(status: StatusCode, message: &str) -> AxumResponse {
    let mut response = AxumResponse::new(Body::from(format!("{message}\n")));
    *response.status_mut() = status;
    response.headers_mut().insert(
        axum::http::header::CONTENT_TYPE,
        HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    response
}

/// Tells the tests whether an attach happened on a tokio worker.
#[cfg(test)]
pub(crate) mod probe {
    use std::cell::Cell;
    use std::sync::atomic::{AtomicUsize, Ordering};

    thread_local! {
        static ASYNC_WORKER: Cell<bool> = const { Cell::new(false) };
    }

    static ON_ASYNC_WORKERS: AtomicUsize = AtomicUsize::new(0);
    static ALL: AtomicUsize = AtomicUsize::new(0);

    /// Mark the calling thread as a tokio worker. The test runtime calls this
    /// from `on_thread_park`, which only workers run.
    pub(crate) fn mark_async_worker() -> bool {
        ASYNC_WORKER.with(|flag| !flag.replace(true))
    }

    pub(crate) fn is_async_worker() -> bool {
        ASYNC_WORKER.with(Cell::get)
    }

    pub(crate) fn note_attach() {
        ALL.fetch_add(1, Ordering::SeqCst);
        if is_async_worker() {
            ON_ASYNC_WORKERS.fetch_add(1, Ordering::SeqCst);
        }
    }

    /// Attaches this crate made on a tokio worker, ever.
    pub(crate) fn on_async_workers() -> usize {
        ON_ASYNC_WORKERS.load(Ordering::SeqCst)
    }

    /// Attaches this crate made, ever.
    pub(crate) fn all() -> usize {
        ALL.load(Ordering::SeqCst)
    }
}
