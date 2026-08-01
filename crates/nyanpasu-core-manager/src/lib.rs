//! Clash core lifecycle management primitives.

mod capability;
mod config;
mod error;
mod health;
pub mod instance;
pub mod kind;
pub mod manager;
pub mod spec;
pub mod state;

pub use capability::{Feature, RuntimeFeature};
pub use clash_api::Host;
pub use error::Error;
pub use health::{
    HealthPolicy,
    probe::{
        ControllerVersionProbe, HealthProbe, ProbeContext, ProbeFuture, ProbeHandle, ProbePhase,
        ProbeResult,
    },
};
pub use instance::{Instance, InstanceBuilder};
pub use kind::CoreKind;
pub use manager::{ApplyOutcome, CoreManager, CoreManagerBuilder, DegradeReason, SwitchOutcome};
pub use spec::{
    CoreSpec, InstanceOptions, InstanceSpec, LocalIpcPolicy, ManagerOptions, ResolvedController,
};
pub use state::{
    ConfigRevision, CoreState, CoreStatus, HealthState, HealthStatus, InstanceState,
    InstanceStatus, RevisionId, SpecSummary, StopReason,
};
