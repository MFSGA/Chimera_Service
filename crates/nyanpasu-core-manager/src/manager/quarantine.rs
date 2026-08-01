use nyanpasu_utils::process::{OrphanReapOutcome, reap_epoch_pid_file};

use crate::{Error, state::CoreState};

use super::{CoreManager, Ctrl, QuarantinedEpoch};

impl CoreManager {
    pub(super) fn latch_quarantine(
        &self,
        ctrl: &mut Ctrl,
        epoch: u64,
        error: &Error,
    ) {
        record_quarantine(ctrl, epoch, error.to_string());
        let quarantine = quarantine_error(ctrl).expect("quarantine was just inserted");
        self.inner.publish(
            CoreState::Stopped {
                reason: Some(crate::StopReason::Error(quarantine.to_string())),
            },
            None,
            None,
            None,
            None,
        );
    }

    /// Prove every uncertain epoch dead, clean its artifacts, then unlock the
    /// manager. Missing authoritative PID records keep the latch closed.
    pub async fn recover_quarantine(&self) -> Result<(), Error> {
        let _operation = self.inner.operation.lock().await;
        let mut ctrl = self.inner.ctrl.lock().await;
        if ctrl.quarantine.is_empty() {
            return Ok(());
        }
        let runtime_dir = self
            .inner
            .options
            .runtime_dir
            .as_ref()
            .expect("validated runtime directory")
            .clone();
        let quarantined = ctrl.quarantine.clone();
        let mut failures = Vec::new();

        for entry in quarantined {
            if !entry.death_proven {
                let pid_path = runtime_dir.join(format!("core-{}.pid", entry.epoch));
                match reap_epoch_pid_file(pid_path.as_std_path(), runtime_dir.as_std_path()).await {
                    Ok(OrphanReapOutcome::AlreadyExited | OrphanReapOutcome::Killed) => {
                        if let Some(current) = ctrl
                            .quarantine
                            .iter_mut()
                            .find(|current| current.epoch == entry.epoch)
                        {
                            current.death_proven = true;
                        }
                    }
                    Ok(OrphanReapOutcome::NotFound) => {
                        failures.push(format!(
                            "epoch {}: authoritative pid record is unavailable",
                            entry.epoch
                        ));
                        continue;
                    }
                    Err(error) => {
                        failures.push(format!("epoch {}: recovery failed: {error}", entry.epoch));
                        continue;
                    }
                }
            }

            if let Err(error) = cleanup_quarantined_epoch(&runtime_dir, entry.epoch).await {
                failures.push(format!(
                    "epoch {}: artifact cleanup failed: {error}",
                    entry.epoch
                ));
            } else {
                ctrl.quarantine
                    .retain(|current| current.epoch != entry.epoch);
            }
        }

        if !failures.is_empty() || !ctrl.quarantine.is_empty() {
            let epoch = ctrl.quarantine.first().map_or(0, |entry| entry.epoch);
            return Err(Error::ManagerQuarantined {
                epoch,
                reason: failures.join(" | "),
            });
        }
        self.inner
            .publish(CoreState::Stopped { reason: None }, None, None, None, None);
        Ok(())
    }
}

pub(super) fn reject_quarantine(ctrl: &Ctrl) -> Result<(), Error> {
    match quarantine_error(ctrl) {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

pub(super) fn record_quarantine(ctrl: &mut Ctrl, epoch: u64, reason: String) {
    if let Some(existing) = ctrl
        .quarantine
        .iter_mut()
        .find(|entry| entry.epoch == epoch)
    {
        existing.reason = reason;
    } else {
        ctrl.quarantine.push(QuarantinedEpoch {
            epoch,
            reason,
            death_proven: false,
        });
    }
}

fn quarantine_error(ctrl: &Ctrl) -> Option<Error> {
    let first = ctrl.quarantine.first()?;
    let reason = if ctrl.quarantine.len() == 1 {
        first.reason.clone()
    } else {
        let epochs = ctrl
            .quarantine
            .iter()
            .map(|entry| entry.epoch.to_string())
            .collect::<Vec<_>>()
            .join(", ");
        format!("{}; additional uncertain epochs: {epochs}", first.reason)
    };
    Some(Error::ManagerQuarantined {
        epoch: first.epoch,
        reason,
    })
}

async fn cleanup_quarantined_epoch(
    runtime_dir: &camino::Utf8Path,
    epoch: u64,
) -> Result<(), Error> {
    for path in [
        runtime_dir.join(format!("config-{epoch}.yaml")),
        runtime_dir.join(format!("core-{epoch}.pid")),
        runtime_dir.join(format!("core-{epoch}.sock")),
    ] {
        match tokio::fs::remove_file(path).await {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ManagerOptions, manager::QuarantinedEpoch};

    async fn manager() -> (CoreManager, tempfile::TempDir, camino::Utf8PathBuf) {
        let temp = tempfile::tempdir().unwrap();
        let runtime = camino::Utf8PathBuf::from_path_buf(temp.path().join("runtime")).unwrap();
        let manager = CoreManager::new(ManagerOptions {
            runtime_dir: Some(runtime.clone()),
            ..ManagerOptions::default()
        })
        .await
        .unwrap();
        (manager, temp, runtime)
    }

    #[tokio::test]
    async fn proven_dead_quarantine_is_cleaned_and_unlatched() {
        let (manager, _temp, runtime) = manager().await;
        tokio::fs::write(runtime.join("config-4.yaml"), b"old")
            .await
            .unwrap();
        tokio::fs::write(runtime.join("core-4.sock"), b"old")
            .await
            .unwrap();
        manager.inner.ctrl.lock().await.quarantine.push(QuarantinedEpoch {
            epoch: 4,
            reason: "uncertain stop".into(),
            death_proven: true,
        });

        manager.recover_quarantine().await.unwrap();
        assert!(manager.inner.ctrl.lock().await.quarantine.is_empty());
        assert!(!runtime.join("config-4.yaml").exists());
        assert!(!runtime.join("core-4.sock").exists());
        assert!(matches!(manager.status().state, CoreState::Stopped { reason: None }));
    }

    #[tokio::test]
    async fn missing_authoritative_pid_record_keeps_the_latch_closed() {
        let (manager, _temp, _runtime) = manager().await;
        manager.inner.ctrl.lock().await.quarantine.push(QuarantinedEpoch {
            epoch: 5,
            reason: "uncertain stop".into(),
            death_proven: false,
        });

        assert!(matches!(
            manager.recover_quarantine().await,
            Err(Error::ManagerQuarantined { epoch: 5, .. })
        ));
        assert_eq!(manager.inner.ctrl.lock().await.quarantine.len(), 1);
    }

    #[tokio::test]
    async fn exited_epoch_with_identity_record_is_recovered() {
        let (manager, _temp, runtime) = manager().await;
        let config = runtime.join("config-6.yaml");
        let pid = runtime.join("core-6.pid");
        tokio::fs::write(&config, b"old").await.unwrap();
        nyanpasu_utils::process::publish_epoch_pid_file(
            pid.as_std_path(),
            &nyanpasu_utils::process::EpochPidRecord {
                pid: u32::MAX - 1,
                epoch: 6,
                executable: "exited-core".into(),
                start_token: 1,
                runtime_config: config.as_std_path().to_path_buf(),
            },
        )
        .await
        .unwrap();
        manager.inner.ctrl.lock().await.quarantine.push(QuarantinedEpoch {
            epoch: 6,
            reason: "uncertain stop".into(),
            death_proven: false,
        });

        manager.recover_quarantine().await.unwrap();
        assert!(!pid.exists());
        assert!(!config.exists());
        assert!(manager.inner.ctrl.lock().await.quarantine.is_empty());
    }
}
