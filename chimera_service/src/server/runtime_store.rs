//! Durable, service-owned runtime configuration artifacts.
//!
//! This is the Chimera-side equivalent of the ref core-manager runtime store:
//! staged files are fsynced before publication, epoch paths are stable, same-epoch
//! replacements are backed up, and parent-directory sync uncertainty is preserved
//! as a warning after an atomic replacement has already succeeded.

use std::{
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};

#[cfg(test)]
use std::sync::Arc;

use camino::{Utf8Path, Utf8PathBuf};
use nyanpasu_utils::io::atomic_fs;
use tokio::io::AsyncWriteExt;

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone)]
pub(super) struct RuntimeConfigStore {
    dir: Utf8PathBuf,
    #[cfg(test)]
    replace_parent_sync_failures: Arc<AtomicU64>,
}

#[derive(Debug)]
pub(super) struct StagedRuntimeConfig {
    path: Utf8PathBuf,
    consumed: bool,
}

impl StagedRuntimeConfig {
    pub(super) fn path(&self) -> &Utf8Path {
        &self.path
    }
}

impl Drop for StagedRuntimeConfig {
    fn drop(&mut self) {
        if !self.consumed {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

#[derive(Debug, Clone)]
pub(super) struct RuntimeConfigBackup {
    path: Utf8PathBuf,
    epoch: u64,
}

#[derive(Debug, Clone)]
pub(super) struct RuntimeConfigCommit {
    path: Utf8PathBuf,
    warning: Option<String>,
}

impl RuntimeConfigCommit {
    pub(super) fn into_parts(self) -> (Utf8PathBuf, Option<String>) {
        (self.path, self.warning)
    }
}

impl RuntimeConfigStore {
    pub(super) async fn open(dir: PathBuf) -> anyhow::Result<Self> {
        match tokio::fs::symlink_metadata(&dir).await {
            Ok(metadata)
                if metadata.file_type().is_symlink()
                    || !metadata.is_dir()
                    || atomic_fs::is_reparse_point(&metadata) =>
            {
                anyhow::bail!("unsafe runtime config directory: {}", dir.display());
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                tokio::fs::create_dir_all(&dir).await?;
            }
            Err(error) => return Err(error.into()),
        }

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            tokio::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).await?;
        }
        #[cfg(windows)]
        {
            atomic_fs::harden_windows_directory_acl(&dir)?;
            atomic_fs::verify_windows_directory_acl(&dir)?;
        }

        let canonical = tokio::fs::canonicalize(&dir).await?;
        let dir = Utf8PathBuf::from_path_buf(canonical)
            .map_err(|_| anyhow::anyhow!("runtime config directory is not valid UTF-8"))?;
        Ok(Self {
            dir,
            #[cfg(test)]
            replace_parent_sync_failures: Arc::new(AtomicU64::new(0)),
        })
    }

    pub(super) fn dir(&self) -> &Utf8Path {
        &self.dir
    }

    pub(super) fn runtime_path(&self, epoch: u64) -> Utf8PathBuf {
        self.dir.join(format!("config-{epoch}.yaml"))
    }

    pub(super) async fn artifact_epochs(&self) -> anyhow::Result<Vec<u64>> {
        let mut epochs = Vec::new();
        let mut entries = tokio::fs::read_dir(self.dir.as_std_path()).await?;
        while let Some(entry) = entries.next_entry().await? {
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            let epoch = name
                .strip_prefix("core-")
                .and_then(|value| value.strip_suffix(".pid"))
                .or_else(|| {
                    name.strip_prefix("config-").and_then(|value| {
                        value
                            .strip_suffix(".yaml")
                            .or_else(|| value.split_once(".yaml.backup-").map(|(epoch, _)| epoch))
                    })
                })
                .or_else(|| {
                    name.strip_prefix(".config-")
                        .and_then(|value| value.split_once(".yaml.tmp-").map(|(epoch, _)| epoch))
                })
                .and_then(|value| value.parse::<u64>().ok());
            if let Some(epoch) = epoch {
                epochs.push(epoch);
            }
        }
        epochs.sort_unstable();
        epochs.dedup();
        Ok(epochs)
    }

    pub(super) async fn stage(
        &self,
        epoch: u64,
        contents: &[u8],
    ) -> anyhow::Result<StagedRuntimeConfig> {
        let counter = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = self.dir.join(format!(
            ".config-{epoch}.yaml.tmp-{}-{counter}",
            std::process::id()
        ));
        atomic_fs::validate_absent_regular_target(path.as_std_path()).await?;

        let mut options = tokio::fs::OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(path.as_std_path()).await?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(std::fs::Permissions::from_mode(0o600))
                .await?;
        }

        if let Err(error) = async {
            file.write_all(contents).await?;
            file.flush().await?;
            file.sync_all().await
        }
        .await
        {
            drop(file);
            let _ = tokio::fs::remove_file(path.as_std_path()).await;
            return Err(error.into());
        }
        drop(file);
        Ok(StagedRuntimeConfig {
            path,
            consumed: false,
        })
    }

    pub(super) async fn commit_new(
        &self,
        mut staged: StagedRuntimeConfig,
        epoch: u64,
    ) -> anyhow::Result<RuntimeConfigCommit> {
        let target = self.runtime_path(epoch);
        atomic_fs::validate_absent_regular_target(target.as_std_path()).await?;
        atomic_fs::atomic_move_new(staged.path.as_std_path(), target.as_std_path()).await?;
        staged.consumed = true;
        atomic_fs::sync_dir(self.dir.as_std_path()).await?;
        Ok(RuntimeConfigCommit {
            path: target,
            warning: None,
        })
    }

    pub(super) async fn commit_replace(
        &self,
        mut staged: StagedRuntimeConfig,
        epoch: u64,
    ) -> anyhow::Result<RuntimeConfigCommit> {
        let target = self.runtime_path(epoch);
        atomic_fs::validate_existing_regular_target(target.as_std_path()).await?;
        atomic_fs::atomic_replace(atomic_fs::AtomicReplacement {
            replacement: staged.path.as_std_path(),
            destination: target.as_std_path(),
        })
        .await?;
        staged.consumed = true;

        #[cfg(test)]
        let injected_failure = self
            .replace_parent_sync_failures
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |remaining| {
                remaining.checked_sub(1)
            })
            .is_ok();
        #[cfg(test)]
        let parent_sync = if injected_failure {
            Err(std::io::Error::other(
                "injected parent-directory synchronization failure",
            ))
        } else {
            atomic_fs::sync_dir(self.dir.as_std_path()).await
        };
        #[cfg(not(test))]
        let parent_sync = atomic_fs::sync_dir(self.dir.as_std_path()).await;

        Ok(RuntimeConfigCommit {
            path: target,
            warning: parent_sync.err().map(|error| {
                format!(
                    "runtime config was atomically installed, but parent-directory synchronization failed: {error}"
                )
            }),
        })
    }

    /// Ensure an already-running legacy/digest-keyed runtime has a stable epoch path
    /// before same-epoch replacement begins.
    pub(super) async fn seed_current(
        &self,
        source: &Utf8Path,
        epoch: u64,
    ) -> anyhow::Result<RuntimeConfigCommit> {
        let target = self.runtime_path(epoch);
        if source == target {
            atomic_fs::validate_existing_regular_target(target.as_std_path()).await?;
            return Ok(RuntimeConfigCommit {
                path: target,
                warning: None,
            });
        }

        atomic_fs::validate_existing_regular_target(source.as_std_path()).await?;
        let contents = tokio::fs::read(source.as_std_path()).await?;
        let staged = self.stage(epoch, &contents).await?;
        match tokio::fs::symlink_metadata(target.as_std_path()).await {
            Ok(_) => self.commit_replace(staged, epoch).await,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                self.commit_new(staged, epoch).await
            }
            Err(error) => Err(error.into()),
        }
    }

    pub(super) async fn backup(
        &self,
        source: &Utf8Path,
        epoch: u64,
        generation: u64,
    ) -> anyhow::Result<RuntimeConfigBackup> {
        atomic_fs::validate_existing_regular_target(source.as_std_path()).await?;
        let contents = tokio::fs::read(source.as_std_path()).await?;
        let mut staged = self.stage(epoch, &contents).await?;
        let backup_path = self
            .dir
            .join(format!("config-{epoch}.yaml.backup-{generation}"));
        atomic_fs::validate_absent_regular_target(backup_path.as_std_path()).await?;
        atomic_fs::atomic_move_new(staged.path.as_std_path(), backup_path.as_std_path()).await?;
        staged.consumed = true;
        atomic_fs::sync_dir(self.dir.as_std_path()).await?;
        Ok(RuntimeConfigBackup {
            path: backup_path,
            epoch,
        })
    }

    pub(super) async fn restore(
        &self,
        backup: &RuntimeConfigBackup,
    ) -> anyhow::Result<RuntimeConfigCommit> {
        atomic_fs::validate_existing_regular_target(backup.path.as_std_path()).await?;
        let contents = tokio::fs::read(backup.path.as_std_path()).await?;
        let staged = self.stage(backup.epoch, &contents).await?;
        self.commit_replace(staged, backup.epoch).await
    }

    pub(super) async fn remove_backup(&self, backup: RuntimeConfigBackup) -> anyhow::Result<()> {
        atomic_fs::remove_regular_file(backup.path.as_std_path()).await?;
        Ok(())
    }

    pub(super) async fn cleanup_epoch(&self, epoch: u64) -> anyhow::Result<()> {
        atomic_fs::remove_regular_file(self.runtime_path(epoch).as_std_path()).await?;

        let backup_prefix = format!("config-{epoch}.yaml.backup-");
        let temp_prefix = format!(".config-{epoch}.yaml.tmp-");
        let mut entries = tokio::fs::read_dir(self.dir.as_std_path()).await?;
        while let Some(entry) = entries.next_entry().await? {
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            if name.starts_with(&backup_prefix) || name.starts_with(&temp_prefix) {
                atomic_fs::remove_regular_file(entry.path()).await?;
            }
        }
        atomic_fs::sync_dir(self.dir.as_std_path()).await?;
        Ok(())
    }

    #[cfg(test)]
    pub(super) fn inject_replace_parent_sync_failure_once(&self) {
        self.replace_parent_sync_failures
            .fetch_add(1, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir() -> PathBuf {
        std::env::temp_dir().join(format!(
            "chimera-runtime-store-{}-{}",
            std::process::id(),
            TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
        ))
    }

    #[tokio::test]
    async fn replace_warns_after_atomic_install_when_parent_sync_is_uncertain() {
        let dir = temp_dir();
        let store = RuntimeConfigStore::open(dir.clone()).await.unwrap();
        let initial = store.stage(7, b"marker: old\n").await.unwrap();
        store.commit_new(initial, 7).await.unwrap();
        let desired = store.stage(7, b"marker: new\n").await.unwrap();

        store.inject_replace_parent_sync_failure_once();
        let commit = store.commit_replace(desired, 7).await.unwrap();

        let (commit_path, warning) = commit.into_parts();
        assert_eq!(
            tokio::fs::read_to_string(&commit_path).await.unwrap(),
            "marker: new\n"
        );
        assert!(warning.as_deref().is_some_and(|warning| {
            warning.contains("atomically installed")
                && warning.contains("parent-directory synchronization failed")
        }));

        let _ = tokio::fs::remove_dir_all(dir).await;
    }

    #[tokio::test]
    async fn seed_current_migrates_legacy_digest_path_to_stable_epoch_path() {
        let dir = temp_dir();
        let store = RuntimeConfigStore::open(dir.clone()).await.unwrap();
        let legacy = store.dir.join("runtime-deadbeef.yaml");
        tokio::fs::write(&legacy, b"marker: legacy\n")
            .await
            .unwrap();

        let commit = store.seed_current(&legacy, 9).await.unwrap();

        let (commit_path, warning) = commit.into_parts();
        assert_eq!(commit_path, store.runtime_path(9));
        assert!(warning.is_none());
        assert_eq!(
            tokio::fs::read_to_string(&commit_path).await.unwrap(),
            "marker: legacy\n"
        );
        assert_eq!(
            tokio::fs::read_to_string(&legacy).await.unwrap(),
            "marker: legacy\n"
        );

        let _ = tokio::fs::remove_dir_all(dir).await;
    }

    #[tokio::test]
    async fn backup_restore_preserves_stable_epoch_path() {
        let dir = temp_dir();
        let store = RuntimeConfigStore::open(dir.clone()).await.unwrap();
        let initial = store.stage(4, b"marker: old\n").await.unwrap();
        let original = store.commit_new(initial, 4).await.unwrap();
        let (original_path, original_warning) = original.into_parts();
        assert!(original_warning.is_none());
        let backup = store.backup(&original_path, 4, 2).await.unwrap();
        let desired = store.stage(4, b"marker: new\n").await.unwrap();
        store.commit_replace(desired, 4).await.unwrap();

        let restored = store.restore(&backup).await.unwrap();
        let (restored_path, _restore_warning) = restored.into_parts();
        assert_eq!(restored_path, original_path);
        assert_eq!(
            tokio::fs::read_to_string(&restored_path).await.unwrap(),
            "marker: old\n"
        );
        store.remove_backup(backup).await.unwrap();

        let _ = tokio::fs::remove_dir_all(dir).await;
    }
}
