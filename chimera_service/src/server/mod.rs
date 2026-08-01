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
pub use manager_bridge::CoreManagerService as CoreManager;
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
    let core_manager = CoreManager::new(token.clone()).await?;
    let bridge_manager = core_manager.clone();
    let mut manager_states = core_manager.subscribe();
    let mut requested_core = core_manager.subscribe_requested_core();
    let hub = EventHub::new();
    let state = AppState {
        core_manager: core_manager.clone(),
        hub: hub.clone(),
    };
    let state_hub = hub.clone();
    tokio::spawn(async move {
        let mut last = bridge_manager.status().await.state;
        loop {
            tokio::select! {
                changed = manager_states.changed() => {
                    if changed.is_err() {
                        break;
                    }
                    let snapshot = bridge_manager.status().await;
                    if !same_legacy_state(&last, &snapshot.state) {
                        tracing::info!("State changed: {:?}", snapshot.state);
                        state_hub.send(WsEvent::new_core_state_changed(snapshot.state.clone()));
                        last = snapshot.state.clone();
                    }
                    state_hub.send(WsEvent::new_core_status_changed(snapshot));
                }
                changed = requested_core.changed() => {
                    if changed.is_err() {
                        break;
                    }
                    state_hub.send(WsEvent::new_core_status_changed(
                        bridge_manager.status().await,
                    ));
                }
            }
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
    let shutdown_token = token.clone();
    let result = create_server(
        SERVICE_PLACEHOLDER,
        app,
        Some(async move {
            shutdown_token.cancelled().await;
        }),
        sids,
    )
    .await;
    core_manager.shutdown().await;
    result?;
    Ok(())
}

fn same_legacy_state(
    previous: &chimera_ipc::api::status::CoreState,
    next: &chimera_ipc::api::status::CoreState,
) -> bool {
    use chimera_ipc::api::status::CoreState;
    match (previous, next) {
        (CoreState::Running, CoreState::Running) => true,
        (CoreState::Stopped(previous), CoreState::Stopped(next)) => previous == next,
        _ => false,
    }
}
