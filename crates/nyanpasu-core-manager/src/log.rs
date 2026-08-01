use crate::CoreKind;

pub const LOG_CHANNEL_CAPACITY: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogStream {
    Stdout,
    Stderr,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogLevel {
    Info,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogFrame {
    pub kind: CoreKind,
    pub epoch: u64,
    pub stream: LogStream,
    pub level: LogLevel,
    pub raw: String,
    pub timestamp: i64,
}

impl LogFrame {
    pub(crate) fn stdout(kind: CoreKind, epoch: u64, raw: String) -> Self {
        Self {
            kind,
            epoch,
            stream: LogStream::Stdout,
            level: LogLevel::Info,
            raw,
            timestamp: crate::state::now_ms(),
        }
    }

    pub(crate) fn stderr(kind: CoreKind, epoch: u64, raw: String) -> Self {
        Self {
            kind,
            epoch,
            stream: LogStream::Stderr,
            level: LogLevel::Error,
            raw,
            timestamp: crate::state::now_ms(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constructors_preserve_epoch_stream_and_text() {
        let stdout = LogFrame::stdout(CoreKind::Mihomo, 7, "ready".into());
        assert_eq!(stdout.epoch, 7);
        assert_eq!(stdout.stream, LogStream::Stdout);
        assert_eq!(stdout.level, LogLevel::Info);
        assert_eq!(stdout.raw, "ready");
        assert!(stdout.timestamp > 0);

        let stderr = LogFrame::stderr(CoreKind::Mihomo, 8, "failed".into());
        assert_eq!(stderr.stream, LogStream::Stderr);
        assert_eq!(stderr.level, LogLevel::Error);
    }
}
