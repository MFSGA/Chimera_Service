//! Clash core lifecycle management primitives.

mod capability;
mod error;
mod health;
pub mod kind;
pub mod spec;
pub mod state;

pub use capability::{Feature, RuntimeFeature};
pub use error::Error;
pub use health::{
    HealthPolicy,
    probe::{HealthProbe, ProbeContext, ProbeFuture, ProbeHandle, ProbePhase, ProbeResult},
};
pub use clash_api::Host;
pub use spec::{
    CoreSpec, InstanceOptions, InstanceSpec, LocalIpcPolicy, ManagerOptions, ResolvedController,
};
pub use state::{
    ConfigRevision, CoreState, CoreStatus, HealthState, HealthStatus, InstanceState,
    InstanceStatus, RevisionId, SpecSummary, StopReason,
};
