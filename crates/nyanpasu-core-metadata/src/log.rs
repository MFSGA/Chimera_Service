//! Canonical shape of one normalized core console record.

use serde::{Deserialize, Serialize};
use specta::Type;

use crate::ClashCoreKind;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Type, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogLevel {
    Trace,
    Debug,
    Info,
    Warning,
    Error,
    Fatal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Type, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogStream {
    Stdout,
    Stderr,
}

#[derive(Debug, Clone, PartialEq, Eq, Type, Serialize, Deserialize)]
pub struct LogTimestamp {
    pub raw: String,
    pub unix_ms: Option<i64>,
    pub inferred: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Type, Serialize, Deserialize)]
pub struct LogField {
    pub key: String,
    pub value: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Type, Serialize, Deserialize)]
pub struct LogFrame {
    pub at: i64,
    pub epoch: u64,
    pub kind: ClashCoreKind,
    pub stream: LogStream,
    pub level: LogLevel,
    pub timestamp: Option<LogTimestamp>,
    pub target: Option<String>,
    pub message: String,
    pub fields: Vec<LogField>,
    pub raw: String,
    pub truncated: bool,
}
