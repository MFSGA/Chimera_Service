use std::{ffi::OsString, io::Result};

use windows_service::{define_windows_service, service::ServiceType, service_dispatcher};

use crate::consts::SERVICE_LABEL;

const SERVICE_TYPE: ServiceType = ServiceType::OWN_PROCESS;

pub fn run() -> Result<()> {
    service_dispatcher::start(SERVICE_LABEL, ffi_service_main).map_err(std::io::Error::other)
}

define_windows_service!(ffi_service_main, service_main);

pub fn service_main(args: Vec<OsString>) {
    if let Err(e) = run_service(args) {
        panic!("Error starting service: {e:?}");
    }
}

pub fn run_service(_arguments: Vec<OsString>) -> windows_service::Result<()> {
    todo!()
}
