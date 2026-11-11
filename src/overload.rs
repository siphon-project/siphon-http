//! Answers for a server that is above its capacity: a per-route cap on
//! requests in progress, and a `Retry-After` on server errors.
//!
//! An overloaded server refuses new work early and says when to come back
//! (RFC 9110 clause 10.2.3), instead of letting requests queue until the
//! client's own timer fires. Both pieces are configuration (see
//! [`crate::config::RouteLimit`] and `error_retry_after_seconds`) and both
//! run in Rust, before or around the handler, without touching Python.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use axum::body::Body;
use axum::extract::Request as AxumRequest;
use axum::http::{header, HeaderValue, Method, StatusCode};
use axum::middleware::Next;
use axum::response::Response as AxumResponse;

use crate::connection::ConnectionInfo;

/// A cap on the requests in progress on one route.
pub(crate) struct Gate {
    inflight: AtomicUsize,
    maximum: usize,
    retry_after_seconds: u32,
    /// Empty: every method.
    methods: Vec<Method>,
    listeners: Option<Vec<String>>,
}

/// Counts one request in progress; released when dropped, whether the
/// response was built or the client went away.
struct Slot(Arc<Gate>);

impl Drop for Slot {
    fn drop(&mut self) {
        self.0.inflight.fetch_sub(1, Ordering::SeqCst);
    }
}

impl Gate {
    pub(crate) fn new(
        maximum: usize,
        retry_after_seconds: u32,
        methods: Vec<Method>,
        listeners: Option<Vec<String>>,
    ) -> Arc<Self> {
        Arc::new(Self {
            inflight: AtomicUsize::new(0),
            maximum,
            retry_after_seconds,
            methods,
            listeners,
        })
    }

    fn covers(&self, request: &AxumRequest) -> bool {
        if !self.methods.is_empty() && !self.methods.contains(request.method()) {
            return false;
        }
        match &self.listeners {
            None => true,
            Some(allowed) => request
                .extensions()
                .get::<ConnectionInfo>()
                .is_some_and(|connection| allowed.contains(&connection.listener.name)),
        }
    }

    fn enter(self: &Arc<Self>) -> Option<Slot> {
        if self.inflight.fetch_add(1, Ordering::SeqCst) >= self.maximum {
            self.inflight.fetch_sub(1, Ordering::SeqCst);
            return None;
        }
        Some(Slot(Arc::clone(self)))
    }
}

/// 503 with `Retry-After` and a problem document with cause `NF_CONGESTION`
/// (TS 29.500 clauses 5.2.7.2 and 6.4.2.2).
fn congested(retry_after_seconds: u32) -> AxumResponse {
    let body = r#"{"title":"Service Unavailable","status":503,"cause":"NF_CONGESTION","detail":"too many requests in progress"}"#;
    let mut response = AxumResponse::new(Body::from(body));
    *response.status_mut() = StatusCode::SERVICE_UNAVAILABLE;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/problem+json"),
    );
    response
        .headers_mut()
        .insert(header::RETRY_AFTER, HeaderValue::from(retry_after_seconds));
    response
}

/// The router layer: refuse with 503 once the gate is full, before the body
/// is read and before any middleware or handler.
pub(crate) async fn limit(gate: Arc<Gate>, request: AxumRequest, next: Next) -> AxumResponse {
    if !gate.covers(&request) {
        return next.run(request).await;
    }
    let Some(slot) = gate.enter() else {
        return congested(gate.retry_after_seconds);
    };
    let response = next.run(request).await;
    drop(slot);
    response
}

/// The router layer: a server error that carries no `Retry-After` gets one.
/// 501 is left alone, which no wait cures.
pub(crate) async fn retry_after(seconds: u32, request: AxumRequest, next: Next) -> AxumResponse {
    let mut response = next.run(request).await;
    let status = response.status();
    if status.is_server_error()
        && status != StatusCode::NOT_IMPLEMENTED
        && !response.headers().contains_key(header::RETRY_AFTER)
    {
        response
            .headers_mut()
            .insert(header::RETRY_AFTER, HeaderValue::from(seconds));
    }
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::routing::any;
    use axum::Router;
    use std::time::Duration;

    async fn serve(router: Router) -> std::net::SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        address
    }

    fn slow_router(gate: Arc<Gate>, handled: Arc<AtomicUsize>) -> Router {
        Router::new()
            .route(
                "/slow",
                any(move || {
                    let handled = handled.clone();
                    async move {
                        handled.fetch_add(1, Ordering::SeqCst);
                        tokio::time::sleep(Duration::from_millis(300)).await;
                        "done"
                    }
                }),
            )
            .layer(axum::middleware::from_fn(move |request, next| {
                limit(gate.clone(), request, next)
            }))
    }

    /// Beyond its cap the gate answers 503 with Retry-After and a problem
    /// document with cause NF_CONGESTION, the handler is not run, and the
    /// slot is free again once a request is answered.
    #[tokio::test]
    async fn a_full_gate_answers_503_with_retry_after_and_never_runs_the_handler() {
        let handled = Arc::new(AtomicUsize::new(0));
        let gate = Gate::new(1, 2, Vec::new(), None);
        let address = serve(slow_router(gate.clone(), handled.clone())).await;
        let client = reqwest::Client::new();
        let url = format!("http://{address}/slow");
        let first = tokio::spawn({
            let (client, url) = (client.clone(), url.clone());
            async move { client.get(url).send().await.unwrap() }
        });
        tokio::time::sleep(Duration::from_millis(100)).await;
        let refused = client.get(&url).send().await.unwrap();
        assert_eq!(refused.status(), 503);
        assert_eq!(refused.headers()["retry-after"], "2");
        assert_eq!(
            refused.headers()["content-type"],
            "application/problem+json"
        );
        let problem = refused.text().await.unwrap();
        assert!(problem.contains(r#""cause":"NF_CONGESTION""#), "{problem}");
        assert!(problem.contains(r#""status":503"#), "{problem}");
        assert_eq!(first.await.unwrap().status(), 200);
        assert_eq!(handled.load(Ordering::SeqCst), 1);
        assert_eq!(gate.inflight.load(Ordering::SeqCst), 0);
        assert_eq!(client.get(&url).send().await.unwrap().status(), 200);
    }

    /// A gate scoped to POST leaves a GET alone however many are in progress.
    #[tokio::test]
    async fn a_gate_scoped_to_a_method_does_not_count_other_methods() {
        let handled = Arc::new(AtomicUsize::new(0));
        let gate = Gate::new(0, 1, vec![Method::POST], None);
        let address = serve(slow_router(gate, handled)).await;
        let client = reqwest::Client::new();
        let url = format!("http://{address}/slow");
        assert_eq!(client.get(&url).send().await.unwrap().status(), 200);
        assert_eq!(client.post(&url).send().await.unwrap().status(), 503);
    }

    /// A server error without Retry-After gets one; one that has its own, a
    /// success and a 501 are left as they are.
    #[tokio::test]
    async fn server_errors_without_retry_after_get_one() {
        let router = Router::new()
            .route("/e500", any(|| async { StatusCode::INTERNAL_SERVER_ERROR }))
            .route("/e503", any(|| async { StatusCode::SERVICE_UNAVAILABLE }))
            .route("/e504", any(|| async { StatusCode::GATEWAY_TIMEOUT }))
            .route("/e501", any(|| async { StatusCode::NOT_IMPLEMENTED }))
            .route("/ok", any(|| async { "fine" }))
            .route(
                "/own",
                any(|| async {
                    (
                        StatusCode::SERVICE_UNAVAILABLE,
                        [(header::RETRY_AFTER, "9")],
                        "",
                    )
                }),
            )
            .layer(axum::middleware::from_fn(|request, next| {
                retry_after(3, request, next)
            }));
        let address = serve(router).await;
        let client = reqwest::Client::new();
        let wait = |path: &'static str| {
            let client = client.clone();
            async move {
                client
                    .get(format!("http://{address}/{path}"))
                    .send()
                    .await
                    .unwrap()
                    .headers()
                    .get("retry-after")
                    .map(|value| value.to_str().unwrap().to_owned())
            }
        };
        assert_eq!(wait("e500").await.as_deref(), Some("3"));
        assert_eq!(wait("e503").await.as_deref(), Some("3"));
        assert_eq!(wait("e504").await.as_deref(), Some("3"));
        assert_eq!(wait("own").await.as_deref(), Some("9"));
        assert_eq!(wait("e501").await, None);
        assert_eq!(wait("ok").await, None);
    }
}
