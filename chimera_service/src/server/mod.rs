pub mod consts;
mod events;
mod logger;
mod manager_bridge;
mod manager_projection;
mod routing;

use chimera_ipc::{
    SERVICE_PLACEHOLDER,
    api::ws::events::Event as WsEvent,
    server::create_server,
};
pub use manager_bridge::CoreManagerService as CoreManager;
pub use logger::Logger;
use events::EventHub;
use routing::{AppState, create_router};
use tokio_util::sync::CancellationToken;
use tracing_attributes::instrument;

#[instrument]
pub async fn run(
    runtime: std::sync::Arc<consts::RuntimeInfos>,
    local_ipc_policy: nyanpasu_core_manager::LocalIpcPolicy,
    token: CancellationToken,
    #[cfg(windows)] sids: &[&str],
    #[cfg(not(windows))] sids: (),
) -> Result<(), anyhow::Error> {
    let core_manager = CoreManager::new(runtime.clone(), local_ipc_policy, token.clone()).await?;
    let control_watch = core_manager.clone();
    let control_failure_token = token.clone();
    tokio::spawn(async move {
        if control_watch.until_control_closed().await
            == nyanpasu_core_manager::ExecutorExit::Died
        {
            tracing::error!("core control executor died; stopping the service");
            control_failure_token.cancel();
        }
    });
    let bridge_manager = core_manager.clone();
    let mut manager_states = core_manager.subscribe();
    let mut requested_core = core_manager.subscribe_requested_core();
    let mut core_logs = core_manager.subscribe_logs();
    let hub = EventHub::new();
    let state = AppState {
        core_manager: core_manager.clone(),
        hub: hub.clone(),
        runtime,
        logger: Logger::global().clone(),
    };
    let log_hub = hub.clone();
    tokio::spawn(async move {
        loop {
            match core_logs.recv().await {
                Ok(frame) => {
                    forward_core_log(&frame);
                    log_hub.send_log(frame);
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                    tracing::warn!("core log subscriber dropped {skipped} frames");
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    });
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
                    let legacy = snapshot.state.clone();
                    state_hub.send(WsEvent::new_core_status_changed(snapshot));
                    if !same_legacy_state(&last, &legacy) {
                        tracing::info!("State changed: {:?}", legacy);
                        state_hub.send(WsEvent::new_core_state_changed(legacy.clone()));
                        last = legacy;
                    }
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

fn forward_core_log(frame: &nyanpasu_core_manager::LogFrame) {
    let kind = frame.kind;
    let epoch = frame.epoch;
    let stream = frame.stream;
    let core_level = frame.level;
    let raw = &frame.raw;
    match core_level {
        nyanpasu_core_manager::LogLevel::Trace => {
            tracing::trace!(target: "chimera_service::core", ?core_level, ?stream, %kind, epoch, "{raw}")
        }
        _ => {
            tracing::debug!(target: "chimera_service::core", ?core_level, ?stream, %kind, epoch, "{raw}")
        }
    }
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
