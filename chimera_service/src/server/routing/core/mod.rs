use axum::{
    Router,
    routing::{get, post},
};
use chimera_ipc::api::core::{
    restart::CORE_RESTART_ENDPOINT,
    start::CORE_START_ENDPOINT,
    stop::CORE_STOP_ENDPOINT,
    v2::{CORE_V2_OPERATION_ENDPOINT, CORE_V2_STATUS_ENDPOINT, CORE_V2_SUBMIT_ENDPOINT},
};

use super::AppState;

pub mod restart;
pub mod start;
pub mod stop;
pub mod v2;

pub fn setup() -> Router<AppState> {
    Router::new()
        .route(CORE_START_ENDPOINT, post(start::start))
        .route(CORE_STOP_ENDPOINT, post(stop::stop))
        .route(CORE_RESTART_ENDPOINT, post(restart::restart))
        .route(CORE_V2_SUBMIT_ENDPOINT, post(v2::submit))
        .route(CORE_V2_OPERATION_ENDPOINT, post(v2::operation))
        .route(CORE_V2_STATUS_ENDPOINT, get(v2::status))
}
