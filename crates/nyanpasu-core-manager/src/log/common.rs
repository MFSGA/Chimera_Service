use chrono::{DateTime, FixedOffset, NaiveDate, NaiveTime, TimeZone};

use crate::LogLevel;

pub(super) fn parse_level(level: &str) -> Option<LogLevel> {
    match level {
        "trace" | "TRC" | "TRACE" => Some(LogLevel::Trace),
        "debug" | "DBG" | "DEBUG" => Some(LogLevel::Debug),
        "info" | "INF" | "INFO" | "???" => Some(LogLevel::Info),
        "warn" | "warning" | "WRN" | "WARN" => Some(LogLevel::Warning),
        "error" | "ERR" | "ERROR" => Some(LogLevel::Error),
        "fatal" | "panic" | "FTL" | "PNC" => Some(LogLevel::Fatal),
        _ => None,
    }
}

pub(super) fn parse_rfc3339_ms(raw: &str) -> Option<i64> {
    DateTime::parse_from_rfc3339(raw)
        .ok()
        .map(|timestamp| timestamp.timestamp_millis())
}

pub(super) fn local_unix_ms(
    date: NaiveDate,
    time: NaiveTime,
    offset: &FixedOffset,
) -> Option<i64> {
    offset
        .from_local_datetime(&date.and_time(time))
        .single()
        .map(|timestamp| timestamp.timestamp_millis())
}

pub(super) fn take_token(input: &str) -> Option<(&str, &str)> {
    let input = input.trim_start();
    let end = input.find(char::is_whitespace)?;
    Some((&input[..end], input[end..].trim_start()))
}
