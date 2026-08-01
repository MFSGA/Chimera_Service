use axum::{Json, Router, http::StatusCode};
use chimera_ipc::{
    api::{
        RBuilder,
        contract::{LogsInspect, LogsRetrieve},
        log::{LogsRes, LogsResBody},
    },
    server::RegisterOperation,
};

pub fn setup<S>() -> Router<S>
where
    S: Clone + Send + Sync + 'static,
{
    Router::new()
        .register(LogsRetrieve, retrieve_logs)
        .register(LogsInspect, inspect_logs)
}

pub async fn retrieve_logs() -> (StatusCode, Json<LogsRes<'static>>) {
    let logs = crate::server::logger::Logger::global().retrieve_logs();
    let res = RBuilder::success(LogsResBody { logs });
    (StatusCode::OK, Json(res))
}

pub async fn inspect_logs() -> (StatusCode, Json<LogsRes<'static>>) {
    let logs = crate::server::logger::Logger::global().inspect_logs();
    let res = RBuilder::success(LogsResBody { logs });
    (StatusCode::OK, Json(res))
}
