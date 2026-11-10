pub mod consts;
mod controller_access;
mod events;
mod logger;
mod manager_bridge;
mod routing;
mod transparent_proxy;

use std::sync::Arc;

use chimera_core_manager::{ExecutorExit, LocalIpcPolicy};
use chimera_ipc::{
    SERVICE_PLACEHOLDER,
    api::ws::events::{Event as WsEvent, TraceLog},
    server::create_server,
};
use consts::RuntimeInfos;
pub use events::EventHub;
pub use logger::Logger;
pub use manager_bridge::{CoreManagerService as CoreManager, ServiceDirs};
use routing::{AppState, create_router};
use tokio_util::sync::CancellationToken;
use tracing_attributes::instrument;

const SERVER_DRAIN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

#[instrument(skip(runtime))]
pub async fn run(
    runtime: RuntimeInfos,
    local_ipc_policy: LocalIpcPolicy,
    token: CancellationToken,
    #[cfg(windows)] sids: &[&str],
    #[cfg(not(windows))] sids: (),
) -> Result<(), anyhow::Error> {
    let runtime_dir =
        camino::Utf8PathBuf::from_path_buf(crate::utils::dirs::service_core_runtime_dir())
            .map_err(|path| anyhow::anyhow!("core runtime dir is not UTF-8: {}", path.display()))?;
    let data_dir = camino::Utf8PathBuf::from_path_buf(runtime.nyanpasu_data_dir.clone())
        .map_err(|path| anyhow::anyhow!("nyanpasu data dir is not UTF-8: {}", path.display()))?;
    let (controller_dir, access): (_, Arc<dyn chimera_core_manager::ControllerAccess>) =
        controller_access_for_host(sids)?;
    let core_manager = CoreManager::with_controller_access(
        ServiceDirs {
            runtime: runtime_dir,
            data: data_dir,
        },
        local_ipc_policy,
        controller_dir,
        access,
    )
    .await?;
    let hub = EventHub::new();
    core_manager.spawn_bridges(hub.clone());

    let transparent_proxy = transparent_proxy::TransparentProxy::new(core_manager.clone());
    if let Err(error) = transparent_proxy.recover().await {
        tracing::error!("failed to recover transparent proxy state: {error}");
    }
    let mut core_status = core_manager.subscribe_runtime_status();
    let proxy_lifecycle = transparent_proxy.clone();
    let proxy_shutdown = transparent_proxy.clone();
    let lifecycle_token = token.clone();
    let lifecycle_shutdown = token.clone();
    let lifecycle_task = tokio::spawn(async move {
        let mut retry = tokio::time::interval(std::time::Duration::from_secs(5));
        retry.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = lifecycle_token.cancelled() => break,
                _ = retry.tick() => {
                    if let Err(error) = proxy_lifecycle.cleanup_if_stale().await {
                        tracing::error!("failed to retry transparent proxy cleanup: {error}");
                    }
                }
                changed = core_status.changed() => {
                    if changed.is_err() { break; }
                    core_status.borrow_and_update();
                    if let Err(error) = proxy_lifecycle.cleanup_if_stale().await {
                        tracing::error!("failed to remove stale transparent proxy rules: {error}");
                    }
                }
            }
        }
        proxy_lifecycle.begin_shutdown();
        if let Err(error) = proxy_lifecycle.cleanup().await {
            tracing::error!("failed to clean transparent proxy rules on shutdown: {error}");
        }
    });

    // Preserve Chimera's service tracing event while the manager owns status
    // and core-log events through the reference EventHub.
    let log_hub = hub.clone();
    Logger::global().set_subscriber(Box::new(move |logging| {
        log_hub.send(WsEvent::new_log(TraceLog {
            timestamp: logging.timestamp,
            level: logging.level,
            message: logging
                .fields
                .get("message")
                .and_then(|value| value.as_str().map(str::to_owned))
                .unwrap_or_default(),
            target: logging
                .fields
                .get("target")
                .and_then(|value| value.as_str().map(str::to_owned))
                .unwrap_or_default(),
            fields: logging.fields,
        }));
    }));

    let state = AppState {
        core_manager: core_manager.clone(),
        hub,
        runtime: Arc::new(runtime),
        transparent_proxy,
    };
    let app = create_router(state);
    tracing::info!("Starting server...");
    let shutdown_token = token.clone();
    let server = create_server(
        SERVICE_PLACEHOLDER,
        app,
        Some(async move {
            shutdown_token.cancelled().await;
        }),
        sids,
    );
    tokio::pin!(server);
    enum ServerExit<E> {
        Server(Result<(), E>),
        Cancelled,
        Control(ExecutorExit),
    }
    let exit = tokio::select! {
        result = &mut server => ServerExit::Server(result),
        _ = token.cancelled() => ServerExit::Cancelled,
        // The control plane owns every core transaction. If its executor is
        // gone the daemon cannot serve `/v2/core/*` truthfully, so it stops
        // rather than answering with a control plane that is not there.
        exit = core_manager.until_control_closed() => ServerExit::Control(exit),
    };
    proxy_shutdown.begin_shutdown();
    lifecycle_shutdown.cancel();
    if let Err(error) = lifecycle_task.await {
        tracing::error!("transparent proxy lifecycle task failed: {error}");
    }
    match exit {
        ServerExit::Server(result) => {
            core_manager.shutdown().await;
            result?;
        }
        ServerExit::Cancelled => {
            core_manager.shutdown().await;
            drain(&mut server).await?;
        }
        ServerExit::Control(exit) => {
            if exit == ExecutorExit::Died {
                tracing::error!("the core control executor died; shutting the service down");
                core_manager.shutdown().await;
                drain(&mut server).await?;
                anyhow::bail!("the core control executor died");
            }
            // Clean: a local shutdown already ran. Nothing maps `Shutdown` onto
            // the wire, so this is the service's own teardown finishing.
            core_manager.shutdown().await;
            drain(&mut server).await?;
        }
    }
    Ok(())
}

async fn drain<E: std::error::Error + Send + Sync + 'static>(
    server: impl std::future::Future<Output = Result<(), E>> + Unpin,
) -> Result<(), anyhow::Error> {
    match tokio::time::timeout(SERVER_DRAIN_TIMEOUT, server).await {
        Ok(result) => result?,
        Err(_) => tracing::warn!(
            "pipe server did not drain within {SERVER_DRAIN_TIMEOUT:?}; abandoning open connections"
        ),
    }
    Ok(())
}

fn controller_access_for_host(
    #[cfg(windows)] sids: &[&str],
    #[cfg(not(windows))] _sids: (),
) -> anyhow::Result<(
    Option<camino::Utf8PathBuf>,
    Arc<dyn chimera_core_manager::ControllerAccess>,
)> {
    #[cfg(windows)]
    {
        Ok((
            None,
            Arc::new(controller_access::WindowsControllerAccess::new(sids)?),
        ))
    }
    #[cfg(unix)]
    {
        // The installation establishes this authorization group for GUI users.
        let mut group = std::mem::MaybeUninit::<libc::group>::uninit();
        let mut result = std::ptr::null_mut();
        let mut buffer = vec![0u8; 16 * 1024];
        let error = unsafe {
            libc::getgrnam_r(
                c"nyanpasu".as_ptr(),
                group.as_mut_ptr(),
                buffer.as_mut_ptr().cast(),
                buffer.len(),
                &mut result,
            )
        };
        if error == 0 && !result.is_null() {
            let gid = unsafe { group.assume_init().gr_gid };
            let root = std::path::Path::new("/var/run/nyanpasu-core");
            match controller_access::UnixControllerAccess::prepare(root, gid) {
                Ok(access) => {
                    let path = root
                        .canonicalize()
                        .ok()
                        .and_then(|path| camino::Utf8PathBuf::from_path_buf(path).ok());
                    if path.is_some() {
                        return Ok((path, Arc::new(access)));
                    }
                }
                Err(error) => tracing::warn!("service core IPC is unavailable: {error}"),
            }
        }
    }
    #[cfg(not(windows))]
    Ok((
        None,
        Arc::new(controller_access::UnavailableControllerAccess),
    ))
}
