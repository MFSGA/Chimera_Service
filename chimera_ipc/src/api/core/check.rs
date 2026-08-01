use crate::api::R;
use serde::{Deserialize, Serialize};
use std::{borrow::Cow, path::PathBuf};

pub const CORE_CHECK_ENDPOINT: &str = "/core/check";

/// Dry-run a config against a core binary without changing the running core.
#[derive(Debug, Serialize, Deserialize, Clone)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct CoreCheckReq<'n> {
    pub core_type: Cow<'n, chimera_utils::core::CoreType>,
    pub config_file: Cow<'n, PathBuf>,
}

pub type CoreCheckRes<'a> = R<'a, ()>;
