pub mod consts;
mod events;
mod instance;
mod logger;
mod manager_bridge;
mod manager_projection;
mod routing;

use chimera_ipc::{
    SERVICE_PLACEHOLDER,
    api::ws::events::{Event as WsEvent, TraceLog},
    server::create_server,
};
pub use instance::CoreManagerService as CoreManager;
pub use logger::Logger;
use events::{EventHub, should_forward_to_hub};
use routing::{AppState, create_router};
use tokio_util::sync::CancellationToken;
use tracing_attributes::instrument;

#[instrument]
pub async fn run(
    token: CancellationToken,
    #[cfg(windows)] sids: &[&str],
    #[cfg(not(windows))] sids: (),
) -> Result<(), anyhow::Error> {
    let (tx, mut rx) = tokio::sync::mpsc::channel(10);
    let core_manager = CoreManager::new_with_notify(tx, token.clone());
    let bridge_manager = core_manager.clone();
    let hub = EventHub::new();
    let state = AppState {
        core_manager,
        hub: hub.clone(),
    };
    let state_hub = hub.clone();
    tokio::spawn(async move {
        while let Some(state) = rx.recv().await {
            state_hub.send(WsEvent::new_core_status_changed(
                bridge_manager.status().await,
            ));
            tracing::info!("State changed: {:?}", state);
            state_hub.send(WsEvent::new_core_state_changed(state));
        }
    });
    Logger::global().set_subscriber(Box::new(move |logging| {
        let target = logging
            .fields
            .get("target")
            .and_then(|value| value.as_str())
            .unwrap_or_default()
            .to_string();
        if !should_forward_to_hub(&target) {
            return;
        }
        hub.send(WsEvent::new_log(TraceLog {
            timestamp: logging.timestamp,
            level: logging.level,
            message: logging
                .fields
                .get("message")
                .and_then(|value| value.as_str().map(str::to_string))
                .unwrap_or_default(),
            target,
            fields: logging.fields,
        }));
    }));

    let app = create_router(state);
    tracing::info!("Starting server...");
    create_server(
        SERVICE_PLACEHOLDER,
        app,
        Some(async move {
            token.cancelled().await;
        }),
        sids,
    )
    .await?;
    Ok(())
}
