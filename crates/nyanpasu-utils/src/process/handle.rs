use tokio::sync::{mpsc, oneshot, watch};

use super::{ProcessError, TerminatedPayload};

#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Containment {
    JobObject,
    CgroupV2,
    ProcessGroup,
    /// Compatibility fallback: descendants are discovered and killed as a tree.
    ProcessTreeScan,
}

pub(crate) enum Ctrl {
    GracefulKill(oneshot::Sender<Result<(), ProcessError>>),
    Kill(oneshot::Sender<Result<(), ProcessError>>),
    WriteStdin(Vec<u8>, oneshot::Sender<Result<(), ProcessError>>),
}

/// Cloneable handle to a spawned child.
#[derive(Clone)]
pub struct ProcessHandle {
    pub(crate) pid: u32,
    pub(crate) containment: Containment,
    pub(crate) ctrl: mpsc::Sender<Ctrl>,
    pub(crate) terminated: watch::Receiver<Option<Result<TerminatedPayload, String>>>,
}

impl ProcessHandle {
    pub fn pid(&self) -> u32 {
        self.pid
    }

    pub fn containment(&self) -> Containment {
        self.containment
    }

    pub async fn wait(&self) -> Result<TerminatedPayload, ProcessError> {
        let mut receiver = self.terminated.clone();
        loop {
            if let Some(result) = receiver.borrow().clone() {
                return result.map_err(ProcessError::Engine);
            }
            receiver
                .changed()
                .await
                .map_err(|_| ProcessError::Engine("process pump task dropped".into()))?;
        }
    }

    pub async fn graceful_kill(&self) -> Result<(), ProcessError> {
        self.send_ctrl(Ctrl::GracefulKill).await?;
        self.wait().await?;
        Ok(())
    }

    pub async fn kill(&self) -> Result<(), ProcessError> {
        self.send_ctrl(Ctrl::Kill).await?;
        self.wait().await?;
        Ok(())
    }

    pub async fn write_stdin(&self, data: &[u8]) -> Result<(), ProcessError> {
        let data = data.to_vec();
        self.send_ctrl(move |reply| Ctrl::WriteStdin(data, reply))
            .await
    }

    async fn send_ctrl(
        &self,
        make: impl FnOnce(oneshot::Sender<Result<(), ProcessError>>) -> Ctrl,
    ) -> Result<(), ProcessError> {
        let (sender, receiver) = oneshot::channel();
        let ctrl = make(sender);
        let idempotent_kill = matches!(&ctrl, Ctrl::GracefulKill(_) | Ctrl::Kill(_));
        if self.ctrl.send(ctrl).await.is_err() {
            return if idempotent_kill && self.terminated.borrow().is_some() {
                Ok(())
            } else if idempotent_kill {
                Err(ProcessError::AlreadyExited)
            } else {
                Err(ProcessError::StdinUnavailable)
            };
        }
        match receiver.await {
            Ok(result) => result,
            Err(_) if idempotent_kill && self.terminated.borrow().is_some() => Ok(()),
            Err(_) if idempotent_kill => Err(ProcessError::AlreadyExited),
            Err(_) => Err(ProcessError::StdinUnavailable),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn wait_observes_an_already_published_exit() {
        let (ctrl, _) = mpsc::channel(1);
        let (_, terminated) = watch::channel(Some(Ok(TerminatedPayload {
            code: Some(0),
            signal: None,
        })));
        let handle = ProcessHandle {
            pid: 9,
            containment: Containment::ProcessGroup,
            ctrl,
            terminated,
        };
        assert_eq!(handle.pid(), 9);
        assert_eq!(handle.wait().await.unwrap().code, Some(0));
    }
}
