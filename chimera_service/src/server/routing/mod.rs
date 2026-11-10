use std::sync::Arc;

use axum::Router;
use tracing_attributes::instrument;

use super::{
    CoreManager, consts::RuntimeInfos, events::EventHub, transparent_proxy::TransparentProxy,
};

pub mod core;
pub mod logs;
pub mod network;
pub mod status;
pub mod transparent_proxy;
pub mod ws;

#[derive(Clone)]
pub struct AppState {
    pub core_manager: CoreManager,
    pub hub: EventHub,
    pub runtime: Arc<RuntimeInfos>,
    pub transparent_proxy: TransparentProxy,
}

#[instrument(skip(state))]
pub fn create_router(state: AppState) -> Router {
    tracing::info!("todo Applying routes...");
    let tracing_layer = tower_http::trace::TraceLayer::new_for_http();
    Router::new()
        .merge(status::setup())
        .merge(core::setup())
        .merge(logs::setup())
        .merge(network::setup())
        .merge(transparent_proxy::setup())
        .merge(ws::setup())
        .with_state(state)
        .layer(tracing_layer)
}
