//! Shared utilities for Nyanpasu applications and services.

#[cfg(feature = "atomic_fs")]
pub mod io;

#[cfg(feature = "process")]
pub mod process;
