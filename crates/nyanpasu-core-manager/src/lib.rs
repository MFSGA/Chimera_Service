//! Clash core lifecycle management primitives.

mod capability;
pub mod kind;
pub mod state;

pub use capability::{Feature, RuntimeFeature};
pub use state::{
    ConfigRevision, CoreState, CoreStatus, HealthState, HealthStatus, InstanceState,
    InstanceStatus, RevisionId, SpecSummary, StopReason,
};
