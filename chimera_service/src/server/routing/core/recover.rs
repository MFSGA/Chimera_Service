use std::borrow::Cow;

use axum::{Json, extract::State, http::StatusCode};
use chimera_ipc::api::{RBuilder, core::recover::CoreRecoverRes};

use crate::server::routing::AppState;

pub async fn recover(State(state): State<AppState>) -> (StatusCode, Json<CoreRecoverRes<'static>>) {
    match state.core_manager.recover().await {
        Ok(()) => (StatusCode::OK, Json(RBuilder::success(()))),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(RBuilder::other_error_with_kind(
                Cow::Owned(error.to_string()),
                error.kind().map(Cow::Borrowed),
            )),
        ),
    }
}
