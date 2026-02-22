// #![feature(error_generic_member_access)]
// #![feature(exit_status_error)]

mod utils;

pub mod consts;

use consts::ExitCode;

use utils::os::register_ctrlc_handler;

fn main() -> ExitCode {
    let mut rx = register_ctrlc_handler();
    // register_panic_hook();
    todo!()
}
