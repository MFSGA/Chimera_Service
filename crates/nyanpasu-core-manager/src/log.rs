//! Normalize core console output into the shared structured [`LogFrame`] shape.

mod common;
mod frame;
mod mihomo;
mod other;

use std::borrow::Borrow;

use chrono::{DateTime, FixedOffset, NaiveDate, NaiveTime};
pub use nyanpasu_core_metadata::{LogField, LogFrame, LogLevel, LogStream, LogTimestamp};

use crate::kind::CoreKind;
use frame::{Pending, bound_frame, stream_index, strip_ansi};

pub(crate) const LOG_CHANNEL_CAPACITY: usize = 256;
pub(crate) type ParsedFrames = [Option<LogFrame>; 2];

pub(super) struct ParsedLine {
    pub(super) timestamp: Option<LogTimestamp>,
    pub(super) level: LogLevel,
    pub(super) target: Option<String>,
    pub(super) message: String,
    pub(super) fields: Vec<LogField>,
}

pub(crate) struct LogParser {
    kind: CoreKind,
    epoch: u64,
    premium_clock: Option<(NaiveDate, NaiveTime)>,
    pending: [Option<Pending>; 2],
    newest_pending: Option<LogStream>,
}

impl LogParser {
    pub(crate) fn new(kind: CoreKind, epoch: u64) -> Self {
        Self {
            kind,
            epoch,
            premium_clock: None,
            pending: [None, None],
            newest_pending: None,
        }
    }

    pub(crate) fn push(&mut self, stream: LogStream, line: String) -> ParsedFrames {
        self.push_at(stream, line, chrono::Local::now().fixed_offset())
    }

    fn push_at(
        &mut self,
        stream: LogStream,
        line: String,
        observed_at: DateTime<FixedOffset>,
    ) -> ParsedFrames {
        let line = strip_ansi(line);
        if let Some(parsed) = self.parse_header(&line, observed_at) {
            let hold = parsed.level >= LogLevel::Error;
            let mut frame = LogFrame {
                at: observed_at.timestamp_millis(),
                epoch: self.epoch,
                kind: self.kind,
                stream,
                level: parsed.level,
                timestamp: parsed.timestamp,
                target: parsed.target,
                message: parsed.message,
                fields: parsed.fields,
                raw: line,
                truncated: false,
            };
            bound_frame(&mut frame);
            return self.emit(stream, frame, hold);
        }

        let error_root = line.starts_with("Error:");
        let warning_root = line.starts_with("warning:");
        if !error_root
            && !warning_root
            && let Some(pending) = self.pending[stream_index(stream)].as_mut()
        {
            pending.append(&line);
            return [None, None];
        }
        let level = match (error_root, warning_root, stream) {
            (true, _, _) => LogLevel::Error,
            (_, true, _) => LogLevel::Warning,
            (.., LogStream::Stdout) => LogLevel::Info,
            (.., LogStream::Stderr) => LogLevel::Warning,
        };
        let mut frame = raw_frame_at(self.kind, self.epoch, stream, line, level, observed_at);
        bound_frame(&mut frame);
        self.emit(stream, frame, error_root && stream == LogStream::Stderr)
    }

    pub(crate) fn finish(&mut self) -> ParsedFrames {
        let stdout = self.pending[0].take().map(|pending| pending.frame);
        let stderr = self.pending[1].take().map(|pending| pending.frame);
        match self.newest_pending.take() {
            Some(LogStream::Stdout) => [stderr, stdout],
            _ => [stdout, stderr],
        }
    }

    fn parse_header(
        &mut self,
        line: &str,
        observed_at: DateTime<FixedOffset>,
    ) -> Option<ParsedLine> {
        match self.kind {
            CoreKind::Mihomo => mihomo::parse(line),
            CoreKind::Meow => other::parse_meow(line),
            CoreKind::ClashRust => other::parse_clash_rs(line, observed_at),
            CoreKind::ClashPremium => {
                other::parse_premium(line, observed_at, &mut self.premium_clock)
            }
        }
    }

    fn emit(&mut self, stream: LogStream, frame: LogFrame, hold: bool) -> ParsedFrames {
        let index = stream_index(stream);
        let flushed = self.pending[index].take().map(|pending| pending.frame);
        if hold {
            self.newest_pending = Some(stream);
            self.pending[index] = Some(Pending::new(frame));
            [flushed, None]
        } else {
            [flushed, Some(frame)]
        }
    }
}

/// The most severe recent frame, latest first within that severity. `None`
/// when nothing above `Info` was logged.
pub(crate) fn error_summary<T: Borrow<LogFrame>>(frames: &[T]) -> Option<String> {
    let level = frames
        .iter()
        .map(|frame| Borrow::<LogFrame>::borrow(frame).level)
        .filter(|level| *level >= LogLevel::Warning)
        .max()?;
    frames
        .iter()
        .rev()
        .map(Borrow::<LogFrame>::borrow)
        .find(|frame| frame.level == level)
        .map(|frame| frame.message.clone())
}

pub(crate) fn format_tail<T: Borrow<LogFrame>>(frames: &[T]) -> String {
    frames
        .iter()
        .map(|frame| Borrow::<LogFrame>::borrow(frame).raw.as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

fn raw_frame_at(
    kind: CoreKind,
    epoch: u64,
    stream: LogStream,
    line: String,
    level: LogLevel,
    observed_at: DateTime<FixedOffset>,
) -> LogFrame {
    LogFrame {
        at: observed_at.timestamp_millis(),
        epoch,
        kind,
        stream,
        level,
        timestamp: None,
        target: None,
        message: line.clone(),
        fields: Vec::new(),
        raw: line,
        truncated: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn observed(value: &str) -> DateTime<FixedOffset> {
        DateTime::parse_from_rfc3339(value).unwrap()
    }

    fn parse_one(kind: CoreKind, stream: LogStream, line: &str) -> LogFrame {
        let mut parser = LogParser::new(kind, 7);
        let mut frames = parser
            .push_at(
                stream,
                line.to_owned(),
                observed("2026-07-29T00:17:27+08:00"),
            )
            .into_iter()
            .flatten()
            .collect::<Vec<_>>();
        frames.extend(parser.finish().into_iter().flatten());
        assert_eq!(frames.len(), 1, "expected exactly one parsed frame");
        frames.remove(0)
    }

    #[test]
    fn parses_mihomo_logfmt_with_structured_fields() {
        let frame = parse_one(
            CoreKind::Mihomo,
            LogStream::Stdout,
            r#"time="2026-07-29T00:16:22+08:00" level=warning msg="say \"hello\"" request=7"#,
        );
        assert_eq!(frame.level, LogLevel::Warning);
        assert_eq!(frame.message, r#"say "hello""#);
        assert_eq!(
            frame.timestamp.as_ref().unwrap().raw,
            "2026-07-29T00:16:22+08:00"
        );
        assert_eq!(
            frame.fields,
            [LogField {
                key: "request".into(),
                value: "7".into()
            }]
        );
    }

    #[test]
    fn strips_ansi_before_parsing_meow() {
        let frame = parse_one(
            CoreKind::Meow,
            LogStream::Stdout,
            "\u{1b}[2m2026-07-28T16:16:26.616489Z\u{1b}[0m \u{1b}[32m INFO\u{1b}[0m \u{1b}[2mmeow\u{1b}[0m\u{1b}[2m:\u{1b}[0m started",
        );
        assert_eq!(frame.level, LogLevel::Info);
        assert_eq!(frame.target.as_deref(), Some("meow"));
        assert_eq!(frame.message, "started");
        assert!(!frame.raw.contains('\u{1b}'));
    }

    #[test]
    fn parses_clash_rs_source_and_message() {
        let frame = parse_one(
            CoreKind::ClashRust,
            LogStream::Stdout,
            r"26-07-29 00:16:35:093078 ERROR clash-lib\src\lib.rs:445: failed at config.yaml:3",
        );
        assert_eq!(frame.level, LogLevel::Error);
        assert_eq!(frame.target.as_deref(), Some(r"clash-lib\src\lib.rs:445"));
        assert_eq!(frame.message, "failed at config.yaml:3");
    }

    #[test]
    fn premium_fatal_holds_following_stack_lines_until_finish() {
        let mut parser = LogParser::new(CoreKind::ClashPremium, 1);
        let at = observed("2026-07-29T00:17:27+08:00");
        for line in [
            "00:17:26 FTL [Config] parse config failed error=bad",
            "goroutine 1 [running]:",
            "main.main()",
        ] {
            assert!(
                parser
                    .push_at(LogStream::Stdout, line.to_owned(), at)
                    .into_iter()
                    .flatten()
                    .next()
                    .is_none()
            );
        }
        let frame = parser.finish().into_iter().flatten().next().unwrap();
        assert_eq!(frame.level, LogLevel::Fatal);
        assert_eq!(frame.target.as_deref(), Some("Config"));
        assert!(
            frame
                .message
                .contains("goroutine 1 [running]:\nmain.main()")
        );
    }

    #[test]
    fn diagnostic_summary_prefers_the_latest_highest_severity() {
        let warning = parse_one(CoreKind::Meow, LogStream::Stderr, "warning: first");
        let error = parse_one(CoreKind::ClashRust, LogStream::Stderr, "Error: second");
        let latest_error = parse_one(CoreKind::ClashRust, LogStream::Stderr, "Error: third");
        assert_eq!(
            error_summary(&[warning, error, latest_error]).as_deref(),
            Some("Error: third")
        );
    }
}
