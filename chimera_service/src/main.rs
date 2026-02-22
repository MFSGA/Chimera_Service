// #![feature(error_generic_member_access)]
// #![feature(exit_status_error)]

mod cmds;
mod utils;

pub mod consts;

mod logging;

mod server;

use chimera_utils::runtime::block_on;
use consts::ExitCode;
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

        Err(e) => {
            error!("Error: {:#?}", e);
            ExitCode::Other
        }
    }
}

fn main() -> ExitCode {
    let mut rx = register_ctrlc_handler();
    // register_panic_hook();

    // todo

    block_on(async {
        tokio::select! {
            biased;
            Some(_) = rx.recv() => {
                ExitCode::Normal
            }
            exit_code = handler() => {
                exit_code
            }
        }
    })
}
