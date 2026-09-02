//! Clash core lifecycle management primitives.

mod capability;
mod error;
mod health;
pub mod kind;
pub mod state;

pub use capability::{Feature, RuntimeFeature};
pub use error::Error;
pub use health::HealthPolicy;
pub use state::{
    ConfigRevision, CoreState, CoreStatus, HealthState, HealthStatus, InstanceState,
    InstanceStatus, RevisionId, SpecSummary, StopReason,
};
