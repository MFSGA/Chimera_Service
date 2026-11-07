// #![feature(error_generic_member_access)]
// #![feature(exit_status_error)]

mod cmds;
mod utils;

pub mod consts;

mod logging;

mod server;

#[cfg(windows)]
mod win_service;

use std::future::Future;

use chimera_utils::runtime::block_on;
use consts::ExitCode;
use tokio::sync::mpsc::Receiver;
use tracing::error;

use utils::{os::register_ctrlc_handler, register_panic_hook};

pub async fn handler() -> ExitCode {
    crate::utils::deadlock_detection();
    let result = cmds::process().await;
    match result {
        Ok(_) => ExitCode::Normal,
        Err(cmds::CommandError::PermissionDenied) => {
            eprintln!("Permission denied, please run as administrator or root");
            ExitCode::PermissionDenied
        }
        Err(cmds::CommandError::ServiceNotInstalled) => {
            eprintln!("Service not installed");
            ExitCode::ServiceNotInstalled
        }
        Err(cmds::CommandError::ServiceAlreadyInstalled) => {
            eprintln!("Service already installed");
            ExitCode::ServiceAlreadyInstalled
        }
        Err(cmds::CommandError::ServiceAlreadyStopped) => {
            eprintln!("Service already stopped");
            ExitCode::ServiceAlreadyStopped
        }
        Err(cmds::CommandError::ServiceAlreadyRunning) => {
            eprintln!("Service already running");
            ExitCode::ServiceAlreadyRunning
        }
        Err(e) => {
            error!("Error: {:#?}", e);
            ExitCode::Other
        }
    }
}

fn main() -> ExitCode {
    #[cfg(windows)]
    if let Some(result) = chimera_process_utils::process::windows_shutdown_helper_entry() {
        return match result {
            Ok(()) => ExitCode::Normal,
            Err(error) => {
                eprintln!("Windows core shutdown helper failed: {error}");
                ExitCode::Other
            }
        };
    }

    #[cfg(feature = "dev")]
    logging::init(true, false).unwrap();
    let mut rx = register_ctrlc_handler();
    register_panic_hook();
    #[cfg(windows)]
    {
        let args = std::env::args_os().any(|arg| &arg == "--service");
        if args {
            crate::win_service::run().unwrap();
            return ExitCode::Normal;
        }
    }

    block_on(run_until_signal(&mut rx, handler(), || {
        crate::cmds::SERVER_SHUTDOWN_TOKEN
            .get()
            .map(|token| token.cancel())
            .is_some()
    }))
}

/// Runs `command` until it ends or a signal arrives. On a signal a running
/// server is asked to stop and still awaited, so it stops its core and drains
/// its clients before the process exits; a second signal ends that wait, and
/// any other command ends at once. `stop_server` cancels the server, and
/// reports whether there was one.
async fn run_until_signal(
    signals: &mut Receiver<()>,
    command: impl Future<Output = ExitCode>,
    stop_server: impl FnOnce() -> bool,
) -> ExitCode {
    tokio::pin!(command);
    tokio::select! {
        biased;
        Some(()) = signals.recv() => {}
        exit_code = &mut command => return exit_code,
    }
    if !stop_server() {
        return ExitCode::Normal;
    }
    tokio::select! {
        biased;
        exit_code = &mut command => exit_code,
        Some(()) = signals.recv() => {
            eprintln!("Stopping without waiting for the service to finish");
            ExitCode::Normal
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };

    use tokio::sync::{Notify, mpsc};

    use super::*;

    /// A server that, once its token is cancelled, stops its core after
    /// `release` is notified.
    async fn server(
        cancelled: Arc<Notify>,
        release: Arc<Notify>,
        core_stopped: Arc<AtomicBool>,
    ) -> ExitCode {
        cancelled.notified().await;
        release.notified().await;
        core_stopped.store(true, Ordering::SeqCst);
        ExitCode::Other
    }

    fn signals() -> (mpsc::Sender<()>, mpsc::Receiver<()>) {
        mpsc::channel(1)
    }

    #[tokio::test]
    async fn a_signal_waits_for_the_server_to_stop_its_core() {
        let (cancelled, release) = (Arc::new(Notify::new()), Arc::new(Notify::new()));
        let core_stopped = Arc::new(AtomicBool::new(false));
        let (signal, mut rx) = signals();
        let stop = cancelled.clone();
        let running = tokio::spawn({
            let (release, core_stopped) = (release.clone(), core_stopped.clone());
            async move {
                run_until_signal(
                    &mut rx,
                    server(cancelled, release, core_stopped),
                    move || {
                        stop.notify_one();
                        true
                    },
                )
                .await
            }
        });

        signal.send(()).await.unwrap();
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
        assert!(!running.is_finished(), "still waiting for the server");

        release.notify_one();
        let exit_code = running.await.unwrap();
        assert!(core_stopped.load(Ordering::SeqCst));
        assert!(
            matches!(exit_code, ExitCode::Other),
            "the server's own exit code"
        );
    }

    #[tokio::test]
    async fn a_second_signal_ends_the_wait_for_a_stuck_server() {
        let (signal, mut rx) = signals();
        let running = tokio::spawn(async move {
            run_until_signal(&mut rx, std::future::pending(), || true).await
        });

        signal.send(()).await.unwrap();
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
        assert!(!running.is_finished(), "the first signal waits");

        signal.send(()).await.unwrap();
        assert!(matches!(running.await.unwrap(), ExitCode::Normal));
    }

    #[tokio::test]
    async fn a_signal_ends_any_other_command_at_once() {
        let (signal, mut rx) = signals();
        signal.send(()).await.unwrap();

        let exit_code = run_until_signal(&mut rx, std::future::pending(), || false).await;

        assert!(matches!(exit_code, ExitCode::Normal));
    }

    #[tokio::test]
    async fn a_command_that_ends_first_keeps_its_exit_code() {
        let (_signal, mut rx) = signals();

        let exit_code = run_until_signal(&mut rx, async { ExitCode::PermissionDenied }, || {
            unreachable!("no signal arrived")
        })
        .await;

        assert!(matches!(exit_code, ExitCode::PermissionDenied));
    }
}
