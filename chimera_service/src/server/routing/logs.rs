use axum::{
    Json, Router,
    extract::State,
    http::StatusCode,
    routing::{get, post},
};
use chimera_ipc::api::{
    RBuilder,
    log::{LOGS_INSPECT_ENDPOINT, LOGS_RETRIEVE_ENDPOINT, LogsRes, LogsResBody},
};

pub fn setup<S>() -> Router<S>
where
    S: Clone + Send + Sync + 'static,
{
    Router::new()
        .route(LOGS_RETRIEVE_ENDPOINT, get(retrieve_logs))
        .route(LOGS_INSPECT_ENDPOINT, get(inspect_logs))
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

use super::AppState;
use chimera_ipc::api::{
    R,
    log::{
        LOG_CLOSE_ENDPOINT, LOG_FILES_ENDPOINT, LOG_OPEN_ENDPOINT, LOG_QUERY_ENDPOINT,
        OwnedLogRequest,
    },
};
use chimera_logging::{LogError, LogFileInfo, LogPage, LogResult, LogSession, OpenLogs, QueryLogs};

pub fn sessions() -> Router<AppState> {
    Router::new()
        .route(LOG_FILES_ENDPOINT, get(list_files))
        .route(LOG_OPEN_ENDPOINT, post(open_session))
        .route(LOG_QUERY_ENDPOINT, post(query_session))
        .route(LOG_CLOSE_ENDPOINT, post(close_session))
        .layer(axum::extract::DefaultBodyLimit::max(16 * 1024))
}
async fn bounded<T: serde::Serialize + serde::de::DeserializeOwned + std::fmt::Debug>(
    future: impl std::future::Future<Output = LogResult<T>>,
) -> Json<R<'static, LogResult<T>>> {
    Json(RBuilder::success(
        tokio::time::timeout(std::time::Duration::from_secs(4), future)
            .await
            .unwrap_or(Err(LogError::Unavailable)),
    ))
}
async fn list_files(
    State(state): State<AppState>,
) -> Json<R<'static, LogResult<Vec<LogFileInfo>>>> {
    bounded(state.logs.catalog()).await
}
async fn open_session(
    State(state): State<AppState>,
    Json(req): Json<OwnedLogRequest<OpenLogs>>,
) -> Json<R<'static, LogResult<LogSession>>> {
    bounded(state.logs.open(req.owner, req.request)).await
}
async fn query_session(
    State(state): State<AppState>,
    Json(req): Json<OwnedLogRequest<QueryLogs>>,
) -> Json<R<'static, LogResult<LogPage>>> {
    bounded(state.logs.query(req.owner, req.request)).await
}
async fn close_session(
    State(state): State<AppState>,
    Json(req): Json<OwnedLogRequest<String>>,
) -> Json<R<'static, LogResult<()>>> {
    bounded(state.logs.close(req.owner, req.request)).await
}
