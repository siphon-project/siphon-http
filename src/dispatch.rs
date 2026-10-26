//! Running a request through its middlewares and route handler.
//!
//! Every request is one job on siphon's Python executor. That job builds the
//! `Request` object, runs the middlewares in order and then the route
//! handler, and turns what they return into the HTTP response, all inside a
//! single attach to the interpreter. The axum task that accepted the request
//! never touches Python: it submits the job and awaits its answer.
//!
//! Keeping Python off the tokio workers matters on free-threaded CPython. A
//! thread that attaches takes part in every stop-the-world pause the garbage
//! collector makes, and a tokio worker parked in an attach stalls every task
//! scheduled on it until the pause ends. Attaching once per request instead
//! of once per stage also saves the attach and detach themselves.
//!
//! An `async def` stage whose coroutine provably cannot suspend also runs in
//! the job, inline, without an asyncio task (siphon's
//! `run_coroutine_inline`). One that can suspend cannot finish inside the job
//! without blocking an attached thread on its awaits, so the job hands it to
//! siphon's asyncio pool: that stage and every stage after it run as one
//! coroutine on a pool loop, which delivers the response through a
//! [`ResponseSink`]. Neither the executor worker nor the axum task waits on
//! the coroutine while attached; the axum task awaits a channel holding the
//! finished response.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{HeaderName, HeaderValue, StatusCode};
use axum::response::Response as AxumResponse;
use pyo3::prelude::*;
use pyo3::sync::PyOnceLock;
use pyo3::types::PyTuple;
use siphon::script::async_pool::AsyncPool;
use siphon::script::inline_dispatch::run_coroutine_inline;
use siphon::script::HandlerHandle;
use tokio::sync::oneshot;

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
    /// the coroutine that runs the stages from the first `async def` on.
    stages: Py<PyTuple>,
}

impl Route {
    pub(crate) fn new(
        py: Python<'_>,
        middlewares: Arc<Vec<Stage>>,
        handler: Stage,
    ) -> PyResult<Self> {
        let mut entries = Vec::with_capacity(middlewares.len() + 1);
        for stage in middlewares.iter() {
            entries
                .push((stage.callable.bind(py).clone(), stage.is_async, true).into_pyobject(py)?);
        }
        entries
            .push((handler.callable.bind(py).clone(), handler.is_async, false).into_pyobject(py)?);
        let stages = PyTuple::new(py, entries)?.unbind();
        Ok(Self {
            middlewares,
            handler,
            stages,
        })
    }
}

/// What the executor job made of a request.
pub(crate) enum Started {
    /// Every stage ran in the job; this is the response.
    Answered(Reply),
    /// An `async def` stage took over; the response arrives here.
    Pending(oneshot::Receiver<Reply>),
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

impl From<&PyResponse> for ScriptReply {
    fn from(response: &PyResponse) -> Self {
        Self {
            status: response.status,
            headers: response
                .headers
                .iter()
                .map(|(name, value)| (name.clone(), value.clone()))
                .collect(),
            body: response.body_bytes.clone(),
        }
    }
}

impl ScriptReply {
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
    let job = siphon::script::py_executor::try_run(move || attach(|py| start(py, &route, request)));
    match job.await {
        Ok(Started::Answered(reply)) => reply.into_response(),
        Ok(Started::Pending(receiver)) => {
            receiver
                .await
                .map(Reply::into_response)
                .unwrap_or_else(|_| {
                    tracing::warn!(
                        target: "siphon_http",
                        "http handler coroutine ended without a response"
                    );
                    error_response(StatusCode::INTERNAL_SERVER_ERROR, "handler error")
                })
        }
        Err(_) => {
            tracing::error!(
                target: "siphon_http",
                "http request could not run on the Python executor (queue full or job panicked)"
            );
            error_response(StatusCode::INTERNAL_SERVER_ERROR, "internal error")
        }
    }
}

/// The executor job: build the `Request` object and run the stages until one
/// answers or an `async def` that can suspend takes over.
pub(crate) fn start(py: Python<'_>, route: &Route, request: PyRequest) -> Started {
    let request = match Py::new(py, request) {
        Ok(request) => request.into_any().into_bound(py),
        Err(error) => {
            tracing::error!(target: "siphon_http", %error, "Py::new(Request) failed");
            return Started::Answered(Reply::error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal error",
            ));
        }
    };
    let middlewares = route.middlewares.len();
    let stages = route
        .middlewares
        .iter()
        .chain(std::iter::once(&route.handler));
    for (index, stage) in stages.enumerate() {
        let is_middleware = index < middlewares;
        let returned = match stage.callable.bind(py).call1((&request,)) {
            Ok(coroutine) if stage.is_async => match run_coroutine_inline(py, &coroutine) {
                Ok(Some(value)) => Ok(value.into_bound(py)),
                Ok(None) => return hand_off(py, route, index, &request, &coroutine),
                Err(error) => Err(error),
            },
            returned => returned,
        };
        match returned {
            Ok(value) if is_middleware && value.is_none() => continue,
            Ok(value) => return Started::Answered(stage_response(&value, is_middleware)),
            Err(error) => return Started::Answered(stage_raised(&error, is_middleware)),
        }
    }
    // The route handler is the last stage and always answers.
    Started::Answered(Reply::error(
        StatusCode::INTERNAL_SERVER_ERROR,
        "internal error",
    ))
}

/// Run the stages from `first` on as one coroutine on siphon's asyncio pool.
/// `started` is the coroutine stage `first` already returned.
fn hand_off(
    py: Python<'_>,
    route: &Route,
    first: usize,
    request: &Bound<'_, PyAny>,
    started: &Bound<'_, PyAny>,
) -> Started {
    let (sender, receiver) = oneshot::channel();
    let submitted = (|| -> PyResult<()> {
        let helpers = helpers(py)?;
        let sink = Py::new(
            py,
            ResponseSink {
                sender: Some(sender),
            },
        )?;
        let pool = AsyncPool::global();
        let timeout = pool
            .as_ref()
            .map(|pool| pool.coroutine_timeout().as_secs_f64());
        let coroutine = helpers.drive.bind(py).call1((
            route.stages.bind(py),
            first,
            request,
            sink,
            timeout,
            started,
        ))?;
        let outcome = match &pool {
            // The pool hands back the coroutine's result channel and its
            // future. The sink carries the response, and the coroutine
            // bounds itself with the pool's timeout, so neither is kept.
            Some(pool) => pool.submit(py, &coroutine).map(drop),
            // No pool (an embedding that never installed one): run the
            // coroutine to completion here, on the executor thread. Python
            // code reaches its safe points while it runs.
            None => helpers.run.bind(py).call1((&coroutine,)).map(drop),
        };
        if outcome.is_err() {
            let _ = coroutine.call_method0("close");
        }
        outcome
    })();
    match submitted {
        Ok(()) => Started::Pending(receiver),
        Err(error) => {
            // Never awaited: close it so it is not reported as such.
            let _ = started.call_method0("close");
            tracing::error!(target: "siphon_http", %error, "could not start the http handler coroutine");
            Started::Answered(Reply::error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal error",
            ))
        }
    }
}

/// The Python side of [`hand_off`]: one coroutine per request.
const HELPERS_SOURCE: &str = r#"
import asyncio


async def drive(stages, first, request, sink, timeout, started):
    is_middleware = stages[first][2]
    try:
        async with asyncio.timeout(timeout):
            for index in range(first, len(stages)):
                handler, is_async, is_middleware = stages[index]
                if started is not None:
                    value = await started
                    started = None
                else:
                    value = handler(request)
                    if is_async:
                        value = await value
                if is_middleware and value is None:
                    continue
                sink.respond(value, is_middleware)
                return
    except Exception as error:
        sink.raised(error, is_middleware)


def run(coroutine):
    asyncio.run(coroutine)
"#;

struct Helpers {
    drive: Py<PyAny>,
    run: Py<PyAny>,
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
            drive: module.getattr("drive")?.unbind(),
            run: module.getattr("run")?.unbind(),
        })
    })
}

/// Where a handler coroutine delivers its response. The conversion to an
/// HTTP response happens here, on the asyncio thread that is already
/// attached.
#[pyclass(module = "siphon_http", name = "_ResponseSink")]
pub(crate) struct ResponseSink {
    sender: Option<oneshot::Sender<Reply>>,
}

#[pymethods]
impl ResponseSink {
    fn respond(&mut self, value: &Bound<'_, PyAny>, is_middleware: bool) {
        self.deliver(stage_response(value, is_middleware));
    }

    fn raised(&mut self, error: &Bound<'_, PyAny>, is_middleware: bool) {
        let error = PyErr::from_value(error.clone());
        self.deliver(stage_raised(&error, is_middleware));
    }
}

impl ResponseSink {
    fn deliver(&mut self, response: Reply) {
        if let Some(sender) = self.sender.take() {
            // The receiver is gone when the client was already answered (the
            // request timeout); nothing is waiting for this response.
            let _ = sender.send(response);
        }
    }
}

/// The response for what a stage returned. A middleware's `None` never gets
/// here: it means "continue".
fn stage_response(value: &Bound<'_, PyAny>, is_middleware: bool) -> Reply {
    match value.extract::<PyRef<'_, PyResponse>>() {
        Ok(response) => Reply::Script(ScriptReply::from(&*response)),
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
pub(crate) fn build_axum_response(response: &PyResponse) -> AxumResponse {
    ScriptReply::from(response).into_response()
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
