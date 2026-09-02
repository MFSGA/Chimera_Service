use std::sync::Arc;

use super::{CoreManager, Logger, consts::RuntimeInfos, events::EventHub};
use axum::Router;
use tracing_attributes::instrument;

pub mod core;
pub mod logs;
mod middleware;
pub mod network;
pub mod status;
pub mod ws;

#[cfg(test)]
mod tests;

#[derive(Clone)]
pub struct AppState {
    pub core_manager: CoreManager,
    pub hub: EventHub,
    pub runtime: Arc<RuntimeInfos>,
    pub logger: Logger<'static>,
}

#[instrument(skip(state))]
pub fn create_router(state: AppState) -> Router {
    tracing::info!("Applying routes...");
    let tracing_layer =
        tower_http::trace::TraceLayer::new_for_http().make_span_with(middleware::RequestSpan);
    let operations = Router::new()
        .merge(status::setup())
        .merge(core::setup())
        .merge(logs::setup())
        .merge(network::setup())
        .layer(axum::middleware::from_fn(middleware::enforce_timeout));
    Router::new()
        .merge(operations)
        .merge(ws::setup())
        .fallback(middleware::not_found)
        .method_not_allowed_fallback(middleware::method_not_allowed)
        .with_state(state)
        .layer(tower_http::catch_panic::CatchPanicLayer::custom(
            middleware::PanicEnvelope,
        ))
        .layer(tower_http::request_id::PropagateRequestIdLayer::x_request_id())
        .layer(tracing_layer)
        .layer(tower_http::request_id::SetRequestIdLayer::x_request_id(
            tower_http::request_id::MakeRequestUuid,
        ))
}
