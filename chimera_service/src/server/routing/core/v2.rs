use std::borrow::Cow;

use axum::{Json, extract::State, http::StatusCode};
use chimera_ipc::api::{
    R, RBuilder,
    core::v2::{CoreOperationReq, CoreOperationRes, CoreSubmitReq, CoreSubmitRes},
    status::CoreInfos,
};

use crate::server::routing::AppState;

pub async fn submit(
    State(state): State<AppState>,
    Json(payload): Json<CoreSubmitReq<'_>>,
) -> (StatusCode, Json<CoreSubmitRes<'static>>) {
    match state.core_manager.submit_v2(&payload) {
        Ok(info) => (StatusCode::OK, Json(RBuilder::success(info))),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(RBuilder::other_error_with_kind_and_retryable(
                Cow::Owned(error.to_string()),
                error.kind(),
                error.retryable(),
            )),
        ),
    }
}

pub async fn operation(
    State(state): State<AppState>,
    Json(payload): Json<CoreOperationReq<'_>>,
) -> (StatusCode, Json<CoreOperationRes<'static>>) {
    match state.core_manager.operation_v2(&payload).await {
        Ok(info) => (StatusCode::OK, Json(RBuilder::success(info))),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(RBuilder::other_error_with_kind_and_retryable(
                Cow::Owned(error.to_string()),
                error.kind(),
                error.retryable(),
            )),
        ),
    }
}

pub async fn status(State(state): State<AppState>) -> (StatusCode, Json<R<'static, CoreInfos>>) {
    (
        StatusCode::OK,
        Json(RBuilder::success(state.core_manager.status().await)),
    )
}
