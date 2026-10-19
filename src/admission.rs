//! A Rust guard of inbound requests.
//!
//! A composing binary that hands an [`Admission`] to
//! [`crate::task_with_admission`] decides on every request a route serves
//! before its body is read and before any `http.middleware` or handler runs:
//! it either refuses the request with a response of its own (an overloaded
//! server answering 503, say) or lets it through, and then adds header fields
//! to the response the handler produced. What it lets through it holds on to
//! until the response is built, so it can count the requests in progress.
//!
//! ```no_run
//! use std::sync::atomic::{AtomicUsize, Ordering};
//! use std::sync::Arc;
//! use siphon_http::admission::{Admission, Admitted, Decision, Inbound, Refusal};
//!
//! struct Limit { busy: Arc<AtomicUsize>, maximum: usize }
//! struct Busy(Arc<AtomicUsize>);
//! impl Admitted for Busy {
//!     fn respond(&mut self, _status: u16) -> Vec<(String, String)> { Vec::new() }
//! }
//! impl Drop for Busy {
//!     fn drop(&mut self) { self.0.fetch_sub(1, Ordering::SeqCst); }
//! }
//! impl Admission for Limit {
//!     fn admit(&self, _request: &Inbound<'_>) -> Decision {
//!         if self.busy.fetch_add(1, Ordering::SeqCst) >= self.maximum {
//!             self.busy.fetch_sub(1, Ordering::SeqCst);
//!             return Decision::Refuse(Refusal {
//!                 status: 503,
//!                 headers: vec![("retry-after".into(), "1".into())],
//!                 body: Vec::new(),
//!             });
//!         }
//!         Decision::Admit(Box::new(Busy(self.busy.clone())))
//!     }
//! }
//! ```

use axum::body::Body;
use axum::extract::Request as AxumRequest;
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode};
use axum::middleware::Next;
use axum::response::Response as AxumResponse;
use std::sync::Arc;

use crate::connection::ConnectionInfo;

/// What an [`Admission`] sees of a request: no body, which is not read yet.
pub struct Inbound<'a> {
    /// The listener's name (`servers[].name`, else its `listen` value).
    pub listener: &'a str,
    pub method: &'a str,
    pub path: &'a str,
    headers: &'a HeaderMap,
}

impl Inbound<'_> {
    /// The value of header field `name` (any case), when it is visible
    /// ASCII; the values of a repeated field are not joined, the first is
    /// returned.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|value| value.to_str().ok())
    }
}

/// A response that answers a request instead of its handler.
pub struct Refusal {
    pub status: u16,
    /// Sent in this order; a name that is not a valid field name, or a
    /// value that is not a valid field value, is left out.
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

/// What a request let through carries until its response is built.
pub trait Admitted: Send {
    /// Header fields to add to the handler's response, given its status.
    /// Called once, when the handler answered; not called when the request
    /// ended without a response (the client went away, the handler timed
    /// out). The value is dropped right after either way.
    fn respond(&mut self, status: u16) -> Vec<(String, String)>;
}

pub enum Decision {
    /// Serve the request.
    Admit(Box<dyn Admitted>),
    /// Answer it with this instead.
    Refuse(Refusal),
}

/// The guard. It runs on the runtime's worker threads, for many requests at
/// once, and must not block.
pub trait Admission: Send + Sync + 'static {
    fn admit(&self, request: &Inbound<'_>) -> Decision;
}

/// Append `fields` to `headers`, leaving out any that is not a valid field.
fn append(headers: &mut HeaderMap, fields: Vec<(String, String)>) {
    for (name, value) in fields {
        match (
            HeaderName::from_bytes(name.as_bytes()),
            HeaderValue::from_str(&value),
        ) {
            (Ok(name), Ok(value)) => {
                headers.append(name, value);
            }
            _ => tracing::warn!(
                target: "siphon_http",
                field = %name,
                "admission header field is not valid; left out"
            ),
        }
    }
}

/// The router layer: [`Admission::admit`] before the request goes on,
/// [`Admitted::respond`] on what comes back.
pub(crate) async fn guard(
    admission: Arc<dyn Admission>,
    request: AxumRequest,
    next: Next,
) -> AxumResponse {
    let listener = request
        .extensions()
        .get::<ConnectionInfo>()
        .map(|connection| connection.listener.name.clone())
        .unwrap_or_default();
    let decision = admission.admit(&Inbound {
        listener: &listener,
        method: request.method().as_str(),
        path: request.uri().path(),
        headers: request.headers(),
    });
    let mut admitted = match decision {
        Decision::Admit(admitted) => admitted,
        Decision::Refuse(refusal) => {
            let mut response = AxumResponse::new(Body::from(refusal.body));
            *response.status_mut() =
                StatusCode::from_u16(refusal.status).unwrap_or(StatusCode::SERVICE_UNAVAILABLE);
            append(response.headers_mut(), refusal.headers);
            return response;
        }
    };
    let mut response = next.run(request).await;
    let fields = admitted.respond(response.status().as_u16());
    append(response.headers_mut(), fields);
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::routing::any;
    use axum::Router;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;
    use std::time::Duration;

    /// (method, path, x-test) of each request the guard saw.
    type Seen = Arc<Mutex<Vec<(String, String, Option<String>)>>>;

    /// Admits up to `maximum` requests at once, refuses the rest with 503,
    /// and stamps each response with the number in progress when it was
    /// admitted and the status it saw.
    struct Limit {
        busy: Arc<AtomicUsize>,
        maximum: usize,
        seen: Seen,
    }
    struct Busy {
        busy: Arc<AtomicUsize>,
        at: usize,
    }
    impl Admitted for Busy {
        fn respond(&mut self, status: u16) -> Vec<(String, String)> {
            vec![
                ("x-admitted-at".into(), self.at.to_string()),
                ("x-status".into(), status.to_string()),
                ("bad name".into(), "x".into()),
            ]
        }
    }
    impl Drop for Busy {
        fn drop(&mut self) {
            self.busy.fetch_sub(1, Ordering::SeqCst);
        }
    }
    impl Admission for Limit {
        fn admit(&self, request: &Inbound<'_>) -> Decision {
            self.seen.lock().unwrap().push((
                request.method.to_owned(),
                request.path.to_owned(),
                request.header("x-test").map(str::to_owned),
            ));
            let at = self.busy.fetch_add(1, Ordering::SeqCst);
            if at >= self.maximum {
                self.busy.fetch_sub(1, Ordering::SeqCst);
                return Decision::Refuse(Refusal {
                    status: 503,
                    headers: vec![("retry-after".into(), "2".into())],
                    body: b"busy".to_vec(),
                });
            }
            Decision::Admit(Box::new(Busy {
                busy: self.busy.clone(),
                at,
            }))
        }
    }

    async fn serve(router: Router) -> std::net::SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        address
    }

    /// The guard runs before the handler and refuses beyond its limit; what
    /// it admitted is released once answered; its fields are added to the
    /// handler's response; a path no route has never reaches it.
    #[tokio::test]
    async fn the_guard_refuses_beyond_its_limit_and_stamps_what_it_admits() {
        let busy = Arc::new(AtomicUsize::new(0));
        let seen = Arc::new(Mutex::new(Vec::new()));
        let admission: Arc<dyn Admission> = Arc::new(Limit {
            busy: busy.clone(),
            maximum: 1,
            seen: seen.clone(),
        });
        let handled = Arc::new(AtomicUsize::new(0));
        let count = handled.clone();
        let router = Router::new()
            .route(
                "/slow",
                any(move || {
                    let count = count.clone();
                    async move {
                        count.fetch_add(1, Ordering::SeqCst);
                        tokio::time::sleep(Duration::from_millis(300)).await;
                        (StatusCode::CREATED, "made")
                    }
                }),
            )
            .route_layer(axum::middleware::from_fn(move |request, next| {
                guard(admission.clone(), request, next)
            }));
        let address = serve(router).await;
        let client = reqwest::Client::new();
        let url = format!("http://{address}/slow");
        let first = tokio::spawn({
            let client = client.clone();
            let url = url.clone();
            async move { client.post(url).header("x-test", "1").send().await.unwrap() }
        });
        tokio::time::sleep(Duration::from_millis(100)).await;
        let refused = client.get(&url).send().await.unwrap();
        assert_eq!(refused.status(), 503);
        assert_eq!(refused.headers()["retry-after"], "2");
        assert_eq!(refused.text().await.unwrap(), "busy");
        let first = first.await.unwrap();
        assert_eq!(first.status(), 201);
        assert_eq!(first.headers()["x-admitted-at"], "0");
        assert_eq!(first.headers()["x-status"], "201");
        assert!(first.headers().get("bad name").is_none());
        assert_eq!(first.text().await.unwrap(), "made");
        // Released: the next one is admitted again.
        let again = client.get(&url).send().await.unwrap();
        assert_eq!(again.status(), 201);
        assert_eq!(again.headers()["x-admitted-at"], "0");
        assert_eq!(busy.load(Ordering::SeqCst), 0);
        // The refused request never reached the handler.
        assert_eq!(handled.load(Ordering::SeqCst), 2);
        // No route: 404 without the guard.
        let missing = client
            .get(format!("http://{address}/other"))
            .send()
            .await
            .unwrap();
        assert_eq!(missing.status(), 404);
        assert_eq!(
            *seen.lock().unwrap(),
            vec![
                ("POST".to_owned(), "/slow".to_owned(), Some("1".to_owned())),
                ("GET".to_owned(), "/slow".to_owned(), None),
                ("GET".to_owned(), "/slow".to_owned(), None),
            ]
        );
    }
}
