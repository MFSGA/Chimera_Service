#[macro_use]
extern crate derive_builder;

pub mod api;
#[cfg(feature = "client")]
pub mod client;
pub mod types;
pub mod utils;

pub const SERVICE_PLACEHOLDER: &str = "chimera_ipc";
