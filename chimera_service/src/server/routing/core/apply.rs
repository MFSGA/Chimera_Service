use std::borrow::Cow;

use axum::{Json, extract::State, http::StatusCode};
use chimera_ipc::api::{
    RBuilder, error_kind,
    core::apply::{CoreApplyReq, CoreApplyRes},
};

use crate::server::routing::AppState;

pub async fn apply(
    State(state): State<AppState>,
    Json(payload): Json<CoreApplyReq<'_>>,
) -> (StatusCode, Json<CoreApplyRes<'static>>) {
    let Some(config_file) = camino::Utf8Path::from_path(payload.config_file.as_ref()) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(RBuilder::other_error_with_kind(
                Cow::Borrowed("config path is not valid UTF-8"),
                Some(Cow::Borrowed(error_kind::INVALID_CONFIG)),
            )),
        );
    };

    match state
        .core_manager
        .apply(
            &payload.core_type,
            config_file,
            payload.expected_revision.as_ref(),
        )
        .await
    {
        Ok(data) => (StatusCode::OK, Json(RBuilder::success(data))),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(RBuilder::other_error_with_kind(
                Cow::Owned(error.to_string()),
                error.kind().map(Cow::Borrowed),
            )),
        ),
    }
}
