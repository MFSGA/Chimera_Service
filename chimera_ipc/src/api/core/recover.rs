use crate::api::R;

pub const CORE_RECOVER_ENDPOINT: &str = "/core/recover";

/// Clear a quarantined lifecycle state. The operation is idempotent.
pub type CoreRecoverRes<'a> = R<'a, ()>;
