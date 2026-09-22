use axum::{Json, extract::State, http::StatusCode};
use chimera_ipc::api::{
    RBuilder,
    core::v2::{
        CoreApiConnectionRes, CoreOperationReq, CoreOperationRes, CoreStatusRes, CoreSubmitReq,
        CoreSubmitRes,
    },
};

use crate::server::routing::AppState;

pub async fn submit(
    State(state): State<AppState>,
    Json(payload): Json<CoreSubmitReq<'_>>,
) -> (StatusCode, Json<CoreSubmitRes<'static>>) {
    match state.core_manager.submit_v2(&payload).await {
        Ok(info) => (StatusCode::OK, Json(RBuilder::success(info))),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(error.into_envelope()),
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
            Json(error.into_envelope()),
        ),
    }
}

pub async fn status(State(state): State<AppState>) -> (StatusCode, Json<CoreStatusRes<'static>>) {
    (
        StatusCode::OK,
        Json(RBuilder::success(state.core_manager.status().await)),
    )
}

pub async fn api_connection(
    State(state): State<AppState>,
) -> (StatusCode, Json<CoreApiConnectionRes<'static>>) {
    (
        StatusCode::OK,
        Json(RBuilder::success(state.core_manager.api_connection_v2())),
    )
}
