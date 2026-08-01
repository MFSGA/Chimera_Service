use constcat::concat;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use specta::Type;

#[derive(Debug, Clone, Copy, Eq, PartialEq, Hash, Type, JsonSchema, Serialize, Deserialize)]
#[serde(untagged)]
pub enum CoreKind {
    Clash(ClashCoreKind),
    SingBox,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq, Hash, Type, JsonSchema, Serialize, Deserialize)]
#[repr(u8)]
pub enum ClashCoreKind {
    #[serde(rename = "mihomo")]
    Mihomo,
    #[serde(rename = "clash-rs")]
    ClashRust,
    #[serde(rename = "clash")]
    ClashPremium,
    #[serde(rename = "meow")]
    Meow,
}

impl AsRef<str> for ClashCoreKind {
    fn as_ref(&self) -> &str {
        match self {
            Self::Mihomo => "mihomo",
            Self::ClashRust => "clash-rs",
            Self::ClashPremium => "clash",
            Self::Meow => "meow",
        }
    }
}

impl std::fmt::Display for ClashCoreKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_ref())
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq, Hash, Type, Serialize, Deserialize)]
#[repr(u8)]
pub enum ClashCoreResourceVariant {
    #[serde(rename = "mihomo")]
    Mihomo,
    #[serde(rename = "mihomo-alpha")]
    MihomoAlpha,
    #[serde(rename = "clash-rs")]
    ClashRust,
    #[serde(rename = "clash-rs-alpha")]
    ClashRustAlpha,
    #[serde(rename = "clash")]
    ClashPremium,
    #[serde(rename = "meow")]
    Meow,
}

impl ClashCoreResourceVariant {
    #[inline]
    pub fn binary_name(&self) -> &'static str {
        use std::env::consts::*;
        match self {
            Self::Mihomo => concat!("mihomo", EXE_SUFFIX),
            Self::MihomoAlpha => concat!("mihomo-alpha", EXE_SUFFIX),
            Self::ClashRust => concat!("clash-rs", EXE_SUFFIX),
            Self::ClashRustAlpha => concat!("clash-rs-alpha", EXE_SUFFIX),
            Self::ClashPremium => concat!("clash", EXE_SUFFIX),
            Self::Meow => concat!("meow", EXE_SUFFIX),
        }
    }
}
