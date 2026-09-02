use axum::{Json, Router, extract::State, http::StatusCode};
use chimera_ipc::{
    api::{
        RBuilder,
        contract::{LogsInspect, LogsRetrieve},
        log::{LogsRes, LogsResBody},
    },
    server::RegisterOperation,
};

use super::AppState;

pub fn setup() -> Router<AppState> {
    Router::new()
        .register(LogsRetrieve, retrieve_logs)
        .register(LogsInspect, inspect_logs)
}

pub async fn retrieve_logs(State(state): State<AppState>) -> (StatusCode, Json<LogsRes<'static>>) {
    let logs = state.logger.retrieve_logs();
    let res = RBuilder::success(LogsResBody { logs });
    (StatusCode::OK, Json(res))
}

pub async fn inspect_logs(State(state): State<AppState>) -> (StatusCode, Json<LogsRes<'static>>) {
    let logs = state.logger.inspect_logs();
    let res = RBuilder::success(LogsResBody { logs });
    (StatusCode::OK, Json(res))
}
