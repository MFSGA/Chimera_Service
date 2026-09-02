use enumset::EnumSetType;

pub use nyanpasu_core_metadata::Feature;

/// Functionality the manager actually enabled for an epoch.
#[derive(Debug, EnumSetType)]
pub enum RuntimeFeature {
    /// The epoch's control channel is a manager-owned local IPC endpoint.
    LocalIpc,
}
