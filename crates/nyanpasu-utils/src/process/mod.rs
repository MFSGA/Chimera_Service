//! Generic child-process management primitives.

mod command;
mod error;
mod event;

pub use command::Command;
pub use error::{ProcessError, ProcessOutput};
pub use event::{ProcessEvent, TerminatedPayload};
