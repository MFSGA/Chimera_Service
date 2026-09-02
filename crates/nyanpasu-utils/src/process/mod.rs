//! Generic child-process management primitives.

mod command;
mod engine;
mod error;
mod event;
mod handle;
mod pid_file;
mod supervisor;

pub use command::Command;
pub use error::{ProcessError, ProcessOutput};
pub use event::{ProcessEvent, TerminatedPayload};
pub use handle::{Containment, ProcessHandle};
pub use pid_file::{
    EpochPidFile, EpochPidRecord, OrphanReapOutcome, ProcessIdentity,
    inspect_process_identity, publish_epoch_pid_file, read_epoch_pid_file,
    reap_epoch_pid_file, record_matches_identity, remove_epoch_pid_file_if_matches,
};
pub use supervisor::{
    Backoff, ReadinessProbe, RestartPolicy, RestartStormPolicy, Supervisor, SupervisorBuilder,
    SupervisorEvent,
};
