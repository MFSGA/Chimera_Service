#[cfg(windows)]
mod win_service;

use chimera_service::{
    consts::ExitCode,
    handler,
    utils::{os::register_ctrlc_handler, register_panic_hook},
};
use chimera_utils::runtime::block_on;

fn main() -> ExitCode {
    #[cfg(feature = "dev")]
    chimera_service::init_dev_logging().unwrap();

    let mut rx = register_ctrlc_handler();
    register_panic_hook();
    #[cfg(windows)]
    {
        let service_mode = std::env::args_os().any(|arg| &arg == "--service");
        if service_mode {
            crate::win_service::run().unwrap();
            return ExitCode::Normal;
        }
    }

    block_on(async {
        tokio::select! {
            biased;
            Some(_) = rx.recv() => ExitCode::Normal,
            exit_code = handler() => exit_code,
        }
    })
}
