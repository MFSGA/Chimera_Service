use std::time::Duration;

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ProcessError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("failed to spawn `{program}`: {message}")]
    Spawn { program: String, message: String },
    #[error("process timed out after {after:?}")]
    Timeout { after: Duration },
    #[error("process already exited")]
    AlreadyExited,
    #[error("stdin is not piped (enable Command::pipe_stdin) or already closed")]
    StdinUnavailable,
    #[error("process engine error: {0}")]
    Engine(String),
}

#[derive(Debug, Clone)]
pub struct ProcessOutput {
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

impl ProcessOutput {
    pub fn success(&self) -> bool {
        self.code == Some(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn process_output_success_only_on_zero() {
        let output = |code| ProcessOutput {
            code,
            stdout: String::new(),
            stderr: String::new(),
        };
        assert!(output(Some(0)).success());
        assert!(!output(Some(1)).success());
        assert!(!output(None).success());
    }

    #[test]
    fn error_display_is_stable() {
        let error = ProcessError::Spawn {
            program: "mihomo".into(),
            message: "not found".into(),
        };
        assert_eq!(error.to_string(), "failed to spawn `mihomo`: not found");
    }
}
