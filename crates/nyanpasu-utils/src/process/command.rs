use std::{
    ffi::{OsStr, OsString},
    path::PathBuf,
    time::Duration,
};

/// Builder for spawning a managed child process.
pub struct Command {
    pub(crate) program: OsString,
    pub(crate) args: Vec<OsString>,
    pub(crate) envs: Vec<(OsString, OsString)>,
    pub(crate) current_dir: Option<PathBuf>,
    pub(crate) encoding: Option<&'static encoding_rs::Encoding>,
    pub(crate) hide_window: bool,
    pub(crate) kill_grace: Duration,
    pub(crate) event_channel_capacity: usize,
    pub(crate) timeout: Option<Duration>,
    pub(crate) pipe_stdin: bool,
}

impl Command {
    pub fn new(program: impl AsRef<OsStr>) -> Self {
        Self {
            program: program.as_ref().to_os_string(),
            args: Vec::new(),
            envs: Vec::new(),
            current_dir: None,
            encoding: None,
            hide_window: true,
            kill_grace: Duration::from_secs(5),
            event_channel_capacity: 64,
            timeout: None,
            pipe_stdin: false,
        }
    }

    pub fn arg(mut self, arg: impl AsRef<OsStr>) -> Self {
        self.args.push(arg.as_ref().to_os_string());
        self
    }

    pub fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.args
            .extend(args.into_iter().map(|arg| arg.as_ref().to_os_string()));
        self
    }

    pub fn env(mut self, key: impl AsRef<OsStr>, value: impl AsRef<OsStr>) -> Self {
        self.envs.push((
            key.as_ref().to_os_string(),
            value.as_ref().to_os_string(),
        ));
        self
    }

    pub fn current_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.current_dir = Some(dir.into());
        self
    }

    pub fn encoding(mut self, encoding: Option<&'static encoding_rs::Encoding>) -> Self {
        self.encoding = encoding;
        self
    }

    pub fn hide_window(mut self, hide: bool) -> Self {
        self.hide_window = hide;
        self
    }

    pub fn kill_grace(mut self, grace: Duration) -> Self {
        self.kill_grace = grace;
        self
    }

    pub fn event_channel_capacity(mut self, capacity: usize) -> Self {
        self.event_channel_capacity = capacity.max(1);
        self
    }

    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }

    pub fn pipe_stdin(mut self, pipe: bool) -> Self {
        self.pipe_stdin = pipe;
        self
    }

}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_design() {
        let command = Command::new("prog");
        assert_eq!(command.event_channel_capacity, 64);
        assert_eq!(command.kill_grace, Duration::from_secs(5));
        assert!(command.hide_window);
        assert!(!command.pipe_stdin);
        assert!(command.encoding.is_none());
        assert!(command.timeout.is_none());
    }

    #[test]
    fn builder_chain_sets_fields() {
        let command = Command::new("prog")
            .arg("-v")
            .args(["a", "b"])
            .env("K", "V")
            .current_dir("C:/tmp")
            .kill_grace(Duration::from_secs(1))
            .event_channel_capacity(8)
            .timeout(Duration::from_secs(3))
            .pipe_stdin(true)
            .hide_window(false);
        assert_eq!(command.args.len(), 3);
        assert_eq!(command.envs.len(), 1);
        assert_eq!(command.event_channel_capacity, 8);
        assert_eq!(command.timeout, Some(Duration::from_secs(3)));
        assert!(command.pipe_stdin);
        assert!(!command.hide_window);
    }
}
