/// Exit information of a terminated child.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminatedPayload {
    pub code: Option<i32>,
    pub signal: Option<i32>,
}

/// Events delivered on the channel returned by `Command::spawn`.
#[non_exhaustive]
#[derive(Debug, Clone)]
pub enum ProcessEvent {
    Stdout(String),
    Stderr(String),
    Error(String),
    Terminated(TerminatedPayload),
}
