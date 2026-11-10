use std::borrow::Cow;

use axum::{
    Json, Router,
    extract::State,
    http::StatusCode,
    routing::{get, post},
};
use chimera_ipc::api::{
    RBuilder,
    network::transparent_proxy::{
        NETWORK_TRANSPARENT_PROXY_RECONCILE_ENDPOINT, NETWORK_TRANSPARENT_PROXY_STATUS_ENDPOINT,
        NetworkTransparentProxyReconcileRes, NetworkTransparentProxyRequest,
        NetworkTransparentProxyStatusRes,
    },
};

use super::AppState;

pub fn setup() -> Router<AppState> {
    Router::new()
        .route(
            NETWORK_TRANSPARENT_PROXY_RECONCILE_ENDPOINT,
            post(reconcile),
        )
        .route(NETWORK_TRANSPARENT_PROXY_STATUS_ENDPOINT, get(status))
}

async fn reconcile(
    State(state): State<AppState>,
    Json(request): Json<NetworkTransparentProxyRequest>,
) -> (
    StatusCode,
    Json<NetworkTransparentProxyReconcileRes<'static>>,
) {
    match state.transparent_proxy.reconcile(request).await {
        Ok(status) => (StatusCode::OK, Json(RBuilder::success(status))),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(RBuilder::other_error(Cow::Owned(error))),
        ),
    }
}

async fn status(
    State(state): State<AppState>,
) -> (StatusCode, Json<NetworkTransparentProxyStatusRes<'static>>) {
    (
        StatusCode::OK,
        Json(RBuilder::success(state.transparent_proxy.status().await)),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn route_contract_uses_protected_local_service_endpoints() {
        assert_eq!(
            NETWORK_TRANSPARENT_PROXY_RECONCILE_ENDPOINT,
            "/network/transparent-proxy/reconcile"
        );
        assert_eq!(
            NETWORK_TRANSPARENT_PROXY_STATUS_ENDPOINT,
            "/network/transparent-proxy/status"
        );
    }
}
