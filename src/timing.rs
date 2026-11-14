//! Server-side request timing, on siphon's custom metrics registry.
//!
//! The outer router layer stamps a request when its head reaches the router
//! and holds a guard until the response body is done (sent, or dropped when
//! the client went away). The executor job and the asyncio hand-off only take
//! `Instant`s; they travel back with the response as [`HandlerStamps`] and are
//! turned into observations here, on the tokio task. No metric is ever
//! updated while attached to Python.
//!
//! Labels are bounded: the listener name, the method (the nine standard ones,
//! anything else is `other`) and the status class. siphon caps every custom
//! metric at 128 label sets; past that, observations are dropped and logged
//! once.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;

use axum::body::Body;
use axum::extract::Request as AxumRequest;
use axum::http::{Method, StatusCode};
use axum::middleware::Next;
use axum::response::Response as AxumResponse;
use axum::Router;
use http_body_util::BodyExt;
use siphon::metrics::custom::CustomMetrics;

use crate::connection::{ConnectionInfo, ListenerIdentity};

/// Request head received to response body done.
pub(crate) const REQUEST_SECONDS: &str = "http_server_request_seconds";
/// Request head received to the executor job starting the stages.
pub(crate) const HANDLER_WAIT_SECONDS: &str = "http_server_handler_wait_seconds";
/// First stage called to the reply being produced.
pub(crate) const HANDLER_SECONDS: &str = "http_server_handler_seconds";
/// Hand-off to siphon's asyncio pool to the coroutine starting there.
pub(crate) const ASYNC_HANDOFF_WAIT_SECONDS: &str = "http_server_async_handoff_wait_seconds";
/// Requests between head received and body done, per listener.
pub(crate) const REQUESTS_IN_FLIGHT: &str = "http_server_requests_in_flight";

/// Dense from 5 to 50 ms, where an SBI call's overhead sits.
const BUCKETS: [f64; 17] = [
    0.0005, 0.001, 0.0025, 0.005, 0.01, 0.015, 0.02, 0.03, 0.05, 0.075, 0.1, 0.25, 0.5, 1.0, 2.5,
    5.0, 10.0,
];

const LABELS: [&str; 3] = ["listener", "method", "status_class"];

/// The listener label of a request that carries no [`ConnectionInfo`]: only
/// a router driven without siphon-http's acceptor (tests) has none.
const UNKNOWN_LISTENER: &str = "unknown";

/// The histograms a request feeds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Metric {
    Request,
    HandlerWait,
    Handler,
    AsyncHandoffWait,
}

impl Metric {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Metric::Request => REQUEST_SECONDS,
            Metric::HandlerWait => HANDLER_WAIT_SECONDS,
            Metric::Handler => HANDLER_SECONDS,
            Metric::AsyncHandoffWait => ASYNC_HANDOFF_WAIT_SECONDS,
        }
    }
}

/// The label values of one observation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Labels<'a> {
    pub(crate) listener: &'a str,
    pub(crate) method: &'static str,
    pub(crate) status_class: &'static str,
}

/// Where observations go. Production writes to siphon's [`CustomMetrics`];
/// tests collect them.
pub(crate) trait Recorder: Send + Sync {
    fn observe(&self, metric: Metric, labels: &Labels<'_>, seconds: f64);
    fn in_flight(&self, listener: &str, delta: f64);
}

/// The timestamps of a request's run through its stages, taken in the
/// executor job and the asyncio hand-off and read back on the tokio task.
#[derive(Debug, Clone, Copy)]
pub(crate) struct HandlerStamps {
    /// The executor job started running the stages.
    pub(crate) job_started: Instant,
    /// An `async def` stage suspended and the rest ran on the asyncio pool.
    pub(crate) handoff: Option<Handoff>,
    /// The reply was produced.
    pub(crate) finished: Instant,
}

/// The hop of a suspending stage onto siphon's asyncio pool.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Handoff {
    /// The coroutine was submitted to the pool.
    pub(crate) submitted: Instant,
    /// The coroutine started running on a pool loop.
    pub(crate) began: Instant,
}

/// The `method` label: a standard method by name, anything else `other`.
pub(crate) fn method_label(method: &Method) -> &'static str {
    match *method {
        Method::GET => "GET",
        Method::HEAD => "HEAD",
        Method::POST => "POST",
        Method::PUT => "PUT",
        Method::DELETE => "DELETE",
        Method::CONNECT => "CONNECT",
        Method::OPTIONS => "OPTIONS",
        Method::TRACE => "TRACE",
        Method::PATCH => "PATCH",
        _ => "other",
    }
}

/// The `status_class` label.
pub(crate) fn status_class(status: StatusCode) -> &'static str {
    match status.as_u16() {
        100..=199 => "1xx",
        200..=299 => "2xx",
        300..=399 => "3xx",
        400..=499 => "4xx",
        _ => "5xx",
    }
}

/// [`Recorder`] over siphon's custom metrics registry.
struct SiphonRecorder {
    metrics: Arc<CustomMetrics>,
}

/// Set once the first failed update has been logged.
static UPDATE_FAILED: AtomicBool = AtomicBool::new(false);

fn update_failed(name: &str, error: &str) {
    if !UPDATE_FAILED.swap(true, Ordering::Relaxed) {
        tracing::warn!(
            target: "siphon_http",
            metric = name,
            %error,
            "an http timing metric could not be updated; further failures are not logged"
        );
    }
}

impl Recorder for SiphonRecorder {
    fn observe(&self, metric: Metric, labels: &Labels<'_>, seconds: f64) {
        let pairs = [
            ("listener", labels.listener),
            ("method", labels.method),
            ("status_class", labels.status_class),
        ];
        if let Err(error) = self
            .metrics
            .histogram_observe(metric.name(), &pairs, seconds)
        {
            update_failed(metric.name(), &error);
        }
    }

    fn in_flight(&self, listener: &str, delta: f64) {
        let labels = [("listener", listener)];
        let updated = if delta >= 0.0 {
            self.metrics.gauge_inc(REQUESTS_IN_FLIGHT, &labels, delta)
        } else {
            self.metrics.gauge_dec(REQUESTS_IN_FLIGHT, &labels, -delta)
        };
        if let Err(error) = updated {
            update_failed(REQUESTS_IN_FLIGHT, &error);
        }
    }
}

/// Register the timing metrics on `metrics` and return the recorder that
/// writes to them.
pub(crate) fn register(metrics: Arc<CustomMetrics>) -> Result<Arc<dyn Recorder>, String> {
    let histograms = [
        (
            REQUEST_SECONDS,
            "HTTP server requests, from the request head reaching the router to the response body done",
        ),
        (
            HANDLER_WAIT_SECONDS,
            "HTTP server requests, from the request head reaching the router to the Python executor starting its handler",
        ),
        (
            HANDLER_SECONDS,
            "HTTP server handlers, middlewares included, from the first one called to the reply produced",
        ),
        (
            ASYNC_HANDOFF_WAIT_SECONDS,
            "HTTP server handlers that suspend, from the hand-off to the asyncio pool to the coroutine starting there",
        ),
    ];
    for (name, help) in histograms {
        metrics.register_histogram(name, help, &LABELS, BUCKETS.to_vec())?;
    }
    metrics.register_gauge(
        REQUESTS_IN_FLIGHT,
        "HTTP server requests between the head reaching the router and the response body done",
        &["listener"],
    )?;
    Ok(Arc::new(SiphonRecorder { metrics }))
}

/// The recorder on siphon's registry, or `None` (logged) when there is no
/// registry or the metrics cannot be registered.
pub(crate) fn siphon_recorder() -> Option<Arc<dyn Recorder>> {
    let Some(metrics) = siphon::metrics::custom_metrics() else {
        tracing::warn!(
            target: "siphon_http",
            "siphon's metrics registry is not initialised; http server timing metrics are off"
        );
        return None;
    };
    match register(Arc::clone(metrics)) {
        Ok(recorder) => Some(recorder),
        Err(error) => {
            tracing::error!(
                target: "siphon_http",
                %error,
                "http server timing metrics could not be registered; they are off"
            );
            None
        }
    }
}

/// `router` with request timing as its outermost layer, so every answer it
/// gives is timed: refusals of the admission guard and the limits, the 504
/// of the request timeout and the 404 of an unknown path included.
pub(crate) fn timed(router: Router, recorder: Arc<dyn Recorder>) -> Router {
    router.layer(axum::middleware::from_fn(move |request, next| {
        layer(Arc::clone(&recorder), request, next)
    }))
}

async fn layer(recorder: Arc<dyn Recorder>, request: AxumRequest, next: Next) -> AxumResponse {
    let mut guard = RequestGuard::new(recorder, &request);
    let mut response = next.run(request).await;
    let status_class = status_class(response.status());
    guard.status_class = Some(status_class);
    if let Some(stamps) = response.extensions_mut().remove::<HandlerStamps>() {
        guard.observe_handler(&stamps, status_class);
    }
    let (parts, body) = response.into_parts();
    // The guard lives as long as the body: hyper drops the body once the last
    // frame is written, or when the client goes away.
    let body = body.map_frame(move |frame| {
        let _until_the_body_is_done = &guard;
        frame
    });
    AxumResponse::from_parts(parts, Body::new(body))
}

/// One request in flight. Dropped when the response body is done, or with
/// the router's future when the request was abandoned before a response.
struct RequestGuard {
    recorder: Arc<dyn Recorder>,
    started: Instant,
    listener: Option<Arc<ListenerIdentity>>,
    method: &'static str,
    /// Known once the router answered.
    status_class: Option<&'static str>,
}

impl RequestGuard {
    fn new(recorder: Arc<dyn Recorder>, request: &AxumRequest) -> Self {
        let guard = Self {
            recorder,
            started: Instant::now(),
            listener: request
                .extensions()
                .get::<ConnectionInfo>()
                .map(|connection| Arc::clone(&connection.listener)),
            method: method_label(request.method()),
            status_class: None,
        };
        guard.recorder.in_flight(guard.listener(), 1.0);
        guard
    }

    fn listener(&self) -> &str {
        self.listener
            .as_deref()
            .map_or(UNKNOWN_LISTENER, |listener| listener.name.as_str())
    }

    fn labels(&self, status_class: &'static str) -> Labels<'_> {
        Labels {
            listener: self.listener(),
            method: self.method,
            status_class,
        }
    }

    fn observe_handler(&self, stamps: &HandlerStamps, status_class: &'static str) {
        let labels = self.labels(status_class);
        let wait = stamps.job_started.saturating_duration_since(self.started);
        self.recorder
            .observe(Metric::HandlerWait, &labels, wait.as_secs_f64());
        let run = stamps
            .finished
            .saturating_duration_since(stamps.job_started);
        self.recorder
            .observe(Metric::Handler, &labels, run.as_secs_f64());
        if let Some(handoff) = stamps.handoff {
            let wait = handoff.began.saturating_duration_since(handoff.submitted);
            self.recorder
                .observe(Metric::AsyncHandoffWait, &labels, wait.as_secs_f64());
        }
    }
}

impl Drop for RequestGuard {
    fn drop(&mut self) {
        if let Some(status_class) = self.status_class {
            let elapsed = self.started.elapsed().as_secs_f64();
            self.recorder
                .observe(Metric::Request, &self.labels(status_class), elapsed);
        }
        self.recorder.in_flight(self.listener(), -1.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn standard_methods_are_their_own_label_and_the_rest_is_other() {
        for (method, label) in [
            (Method::GET, "GET"),
            (Method::HEAD, "HEAD"),
            (Method::POST, "POST"),
            (Method::PUT, "PUT"),
            (Method::DELETE, "DELETE"),
            (Method::CONNECT, "CONNECT"),
            (Method::OPTIONS, "OPTIONS"),
            (Method::TRACE, "TRACE"),
            (Method::PATCH, "PATCH"),
        ] {
            assert_eq!(method_label(&method), label);
        }
        let custom = Method::from_bytes(b"PURGE").unwrap();
        assert_eq!(method_label(&custom), "other");
    }

    #[test]
    fn status_codes_fall_into_their_class() {
        for (code, class) in [
            (100, "1xx"),
            (101, "1xx"),
            (200, "2xx"),
            (204, "2xx"),
            (301, "3xx"),
            (404, "4xx"),
            (499, "4xx"),
            (500, "5xx"),
            (503, "5xx"),
            (599, "5xx"),
        ] {
            assert_eq!(status_class(StatusCode::from_u16(code).unwrap()), class);
        }
    }

    #[test]
    fn the_metrics_register_on_siphons_registry() {
        siphon::metrics::init().unwrap();
        let metrics = Arc::clone(siphon::metrics::custom_metrics().unwrap());
        let recorder = register(metrics).expect("the timing metrics register");
        let labels = Labels {
            listener: "sbi",
            method: "GET",
            status_class: "2xx",
        };
        for metric in [
            Metric::Request,
            Metric::HandlerWait,
            Metric::Handler,
            Metric::AsyncHandoffWait,
        ] {
            recorder.observe(metric, &labels, 0.004);
        }
        recorder.in_flight("sbi", 1.0);
        recorder.in_flight("sbi", -1.0);
        assert!(
            !UPDATE_FAILED.load(Ordering::Relaxed),
            "an update on the registered metrics failed"
        );
    }
}
