//! Shared protection and observability layers for request/response IPC routes.

use std::{any::Any, borrow::Cow, time::Duration};

use axum::{
    Json,
    extract::Request,
    http::{Response, StatusCode},
    middleware::Next,
    response::IntoResponse,
};
use chimera_ipc::api::{R, RBuilder};
use tower_http::{catch_panic::ResponseForPanic, trace::MakeSpan};

const REQUEST_ID_HEADER: &str = "x-request-id";

/// Upper bound for request/response operations. WebSocket streams are mounted
/// outside this layer and remain unbounded.
pub(super) const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);

fn error_envelope(status: StatusCode, msg: Cow<'static, str>) -> axum::response::Response {
    let body: R<'static, ()> = RBuilder::other_error(msg);
    (status, Json(body)).into_response()
}

pub(super) async fn not_found() -> axum::response::Response {
    error_envelope(StatusCode::NOT_FOUND, Cow::Borrowed("not found"))
}

pub(super) async fn method_not_allowed() -> axum::response::Response {
    error_envelope(
        StatusCode::METHOD_NOT_ALLOWED,
        Cow::Borrowed("method not allowed"),
    )
}

pub(super) async fn enforce_timeout(request: Request, next: Next) -> axum::response::Response {
    match tokio::time::timeout(REQUEST_TIMEOUT, next.run(request)).await {
        Ok(response) => response,
        Err(_) => {
            tracing::error!("request exceeded {REQUEST_TIMEOUT:?}; answering with a timeout");
            error_envelope(
                StatusCode::REQUEST_TIMEOUT,
                Cow::Borrowed("request timed out"),
            )
        }
    }
}

#[derive(Clone, Copy)]
pub(super) struct PanicEnvelope;

impl ResponseForPanic for PanicEnvelope {
    type ResponseBody = axum::body::Body;

    fn response_for_panic(
        &mut self,
        err: Box<dyn Any + Send + 'static>,
    ) -> Response<Self::ResponseBody> {
        let detail = if let Some(message) = err.downcast_ref::<String>() {
            message.as_str()
        } else if let Some(message) = err.downcast_ref::<&str>() {
            message
        } else {
            "unknown panic payload"
        };
        tracing::error!("request handler panicked: {detail}");
        error_envelope(
            StatusCode::INTERNAL_SERVER_ERROR,
            Cow::Borrowed("internal server error"),
        )
    }
}

#[derive(Clone, Copy)]
pub(super) struct RequestSpan;

impl<B> MakeSpan<B> for RequestSpan {
    fn make_span(&mut self, request: &axum::http::Request<B>) -> tracing::Span {
        tracing::debug_span!(
            "request",
            method = %request.method(),
            uri = %request.uri(),
            version = ?request.version(),
            request_id = request
                .headers()
                .get(REQUEST_ID_HEADER)
                .and_then(|value| value.to_str().ok())
                .unwrap_or("-"),
        )
    }
}
