use std::borrow::Cow;

use axum::{Json, Router, extract::State, http::StatusCode};

use chimera_ipc::{
    api::{
        RBuilder,
        contract::Status,
        status::{LogPathsInfo, RuntimeInfos, StatusRes, StatusResBody},
    },
    server::RegisterOperation,
};

use super::AppState;

pub fn setup() -> Router<AppState> {
    Router::new().register(Status, status)
}

pub async fn status(State(state): State<AppState>) -> (StatusCode, Json<StatusRes<'static>>) {
    let status = state.core_manager.status().await;
    let runtime_infos = state.runtime.as_ref();
    let res = RBuilder::success(StatusResBody {
        version: Cow::Borrowed(crate::consts::APP_VERSION),
        core_infos: status,
        runtime_infos: RuntimeInfos {
            service_data_dir: Cow::Owned(runtime_infos.service_data_dir.clone()),
            service_config_dir: Cow::Owned(runtime_infos.service_config_dir.clone()),
            nyanpasu_config_dir: Cow::Owned(runtime_infos.nyanpasu_config_dir.clone()),
            nyanpasu_data_dir: Cow::Owned(runtime_infos.nyanpasu_data_dir.clone()),
        },
        logs: Some(LogPathsInfo {
            service_dir: crate::utils::dirs::service_logs_dir(),
            core_dir: state.core_manager.core_log_dir(),
        }),
    });

    (StatusCode::OK, Json(res))
}
