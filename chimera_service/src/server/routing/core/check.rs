use std::borrow::Cow;

use axum::{Json, extract::State, http::StatusCode};
use chimera_ipc::api::{
    CoreErrorKind, RBuilder,
    core::check::{CoreCheckReq, CoreCheckRes},
};

use crate::server::routing::AppState;

pub async fn check(
    State(state): State<AppState>,
    Json(payload): Json<CoreCheckReq<'_>>,
) -> (StatusCode, Json<CoreCheckRes<'static>>) {
    let Some(config_file) = camino::Utf8Path::from_path(payload.config_file.as_ref()) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(RBuilder::other_error_with_kind(
                Cow::Borrowed("config path is not valid UTF-8"),
                Some(CoreErrorKind::InvalidConfig),
            )),
        );
    };

    match state.core_manager.check(&payload.core_type, config_file).await {
        Ok(()) => (StatusCode::OK, Json(RBuilder::success(()))),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(RBuilder::other_error_with_kind(
                Cow::Owned(error.to_string()),
                error.kind(),
            )),
        ),
    }
}
