//! Stable machine-readable classifications for core-manager failures.

use serde::{Deserialize, Serialize};
use specta::Type;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Type, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CoreErrorKind {
    NotStarted,
    AlreadyRunning,
    RevisionConflict,
    Quarantined,
    ConfigCheckFailed,
    ConfigNotFound,
    BinaryNotFound,
    InvalidConfig,
    ControllerMissing,
    ApplyFailed,
    ApplyRollbackFailed,
    StopUnconfirmed,
}

impl CoreErrorKind {
    pub const ALL: &'static [Self] = &[
        Self::NotStarted,
        Self::AlreadyRunning,
        Self::RevisionConflict,
        Self::Quarantined,
        Self::ConfigCheckFailed,
        Self::ConfigNotFound,
        Self::BinaryNotFound,
        Self::InvalidConfig,
        Self::ControllerMissing,
        Self::ApplyFailed,
        Self::ApplyRollbackFailed,
        Self::StopUnconfirmed,
    ];

    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::NotStarted => "not_started",
            Self::AlreadyRunning => "already_running",
            Self::RevisionConflict => "revision_conflict",
            Self::Quarantined => "quarantined",
            Self::ConfigCheckFailed => "config_check_failed",
            Self::ConfigNotFound => "config_not_found",
            Self::BinaryNotFound => "binary_not_found",
            Self::InvalidConfig => "invalid_config",
            Self::ControllerMissing => "controller_missing",
            Self::ApplyFailed => "apply_failed",
            Self::ApplyRollbackFailed => "apply_rollback_failed",
            Self::StopUnconfirmed => "stop_unconfirmed",
        }
    }

    pub fn from_wire(value: &str) -> Option<Self> {
        Self::ALL
            .iter()
            .copied()
            .find(|kind| kind.as_str() == value)
    }
}

impl std::fmt::Display for CoreErrorKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_kind_roundtrips_its_wire_string() {
        for kind in CoreErrorKind::ALL {
            assert_eq!(
                serde_json::to_string(kind).unwrap(),
                format!("\"{}\"", kind.as_str())
            );
            assert_eq!(CoreErrorKind::from_wire(kind.as_str()), Some(*kind));
        }
    }

    #[test]
    fn unknown_wire_string_is_forward_compatible() {
        assert_eq!(CoreErrorKind::from_wire("a_future_kind"), None);
    }
}
