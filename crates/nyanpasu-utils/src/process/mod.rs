//! Generic child-process management primitives.

mod command;
mod engine;
mod error;
mod event;
mod handle;
mod supervisor;

pub use command::Command;
pub use error::{ProcessError, ProcessOutput};
pub use event::{ProcessEvent, TerminatedPayload};
pub use handle::{Containment, ProcessHandle};
pub use supervisor::{
    Backoff, ReadinessProbe, RestartPolicy, RestartStormPolicy, Supervisor, SupervisorBuilder,
    SupervisorEvent,
};
