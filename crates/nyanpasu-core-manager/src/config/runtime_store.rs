//! Durable, manager-owned runtime configuration artifacts.

mod maintenance;

use std::sync::atomic::{AtomicU64, Ordering};

#[cfg(feature = "test-hooks")]
use std::sync::{Arc, atomic::AtomicUsize};

use camino::{Utf8Path, Utf8PathBuf};
use nyanpasu_utils::io::atomic_fs;
use tokio::io::AsyncWriteExt;

use crate::Error;

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone)]
pub struct RuntimeConfigStore {
    dir: Utf8PathBuf,
    #[cfg(feature = "test-hooks")]
    replace_parent_sync_failures: Arc<AtomicUsize>,
}

#[derive(Debug)]
pub(crate) struct RuntimeDirectoryLock {
    _lock: atomic_fs::DirLock,
}

#[derive(Debug)]
pub struct StagedRuntimeConfig {
    path: Utf8PathBuf,
    consumed: bool,
}

impl StagedRuntimeConfig {
    pub fn path(&self) -> &Utf8Path {
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
pub struct RuntimeConfigBackup {
    path: Utf8PathBuf,
    epoch: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuntimeCommitDurability {
    Durable,
    Uncertain(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeConfigCommit {
    path: Utf8PathBuf,
    durability: RuntimeCommitDurability,
}

impl RuntimeConfigCommit {
    pub fn path(&self) -> &Utf8Path {
        &self.path
    }

    pub fn durability(&self) -> &RuntimeCommitDurability {
        &self.durability
    }

    pub fn durability_warning(&self) -> Option<&str> {
        match &self.durability {
            RuntimeCommitDurability::Durable => None,
            RuntimeCommitDurability::Uncertain(warning) => Some(warning),
        }
    }
}

impl RuntimeConfigBackup {
    pub fn path(&self) -> &Utf8Path {
        &self.path
    }
}

impl RuntimeConfigStore {
    pub async fn new(dir: Utf8PathBuf) -> Result<Self, Error> {
        match tokio::fs::symlink_metadata(&dir).await {
            Ok(metadata) => validate_directory_metadata(&dir, &metadata)?,
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
            .map_err(|_| Error::InvalidManagerOptions("runtime directory is not UTF-8".into()))?;
        let metadata = tokio::fs::symlink_metadata(&dir).await?;
        validate_directory_metadata(&dir, &metadata)?;
        Ok(Self {
            dir,
            #[cfg(feature = "test-hooks")]
            replace_parent_sync_failures: Arc::new(AtomicUsize::new(0)),
        })
    }

    pub fn dir(&self) -> &Utf8Path {
        &self.dir
    }

    pub fn runtime_path(&self, epoch: u64) -> Utf8PathBuf {
        self.dir.join(format!("config-{epoch}.yaml"))
    }

    pub fn pid_path(&self, epoch: u64) -> Utf8PathBuf {
        self.dir.join(format!("core-{epoch}.pid"))
    }

    pub fn socket_path(&self, epoch: u64) -> Utf8PathBuf {
        self.dir.join(format!("core-{epoch}.sock"))
    }

    #[cfg(feature = "test-hooks")]
    pub fn inject_replace_parent_sync_failure_once(&self) {
        self.replace_parent_sync_failures
            .fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) async fn acquire_ownership(&self) -> Result<RuntimeDirectoryLock, Error> {
        let path = self.dir.join(".manager.lock");
        tokio::task::spawn_blocking(move || {
            atomic_fs::acquire_dir_lock(path).map(|lock| RuntimeDirectoryLock { _lock: lock })
        })
        .await
        .map_err(std::io::Error::other)?
        .map_err(Error::from)
    }

    pub async fn stage(
        &self,
        epoch: u64,
        contents: &[u8],
    ) -> Result<StagedRuntimeConfig, Error> {
        let counter = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = self.dir.join(format!(
            ".config-{epoch}.yaml.tmp-{}-{counter}",
            std::process::id()
        ));
        atomic_fs::validate_absent_regular_target(&path).await?;
        let mut options = tokio::fs::OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&path).await?;
        if let Err(error) = async {
            file.write_all(contents).await?;
            file.flush().await?;
            file.sync_all().await
        }
        .await
        {
            drop(file);
            let _ = tokio::fs::remove_file(&path).await;
            return Err(error.into());
        }
        drop(file);
        Ok(StagedRuntimeConfig {
            path,
            consumed: false,
        })
    }

    pub async fn commit_new(
        &self,
        mut staged: StagedRuntimeConfig,
        epoch: u64,
    ) -> Result<Utf8PathBuf, Error> {
        self.validate_staged(&staged, epoch).await?;
        let target = self.runtime_path(epoch);
        atomic_fs::validate_absent_regular_target(&target).await?;
        atomic_fs::atomic_move_new(&staged.path, &target).await?;
        staged.consumed = true;
        atomic_fs::sync_dir(&self.dir).await?;
        Ok(target)
    }

    pub async fn replace(
        &self,
        epoch: u64,
        contents: &[u8],
    ) -> Result<RuntimeConfigCommit, Error> {
        let staged = self.stage(epoch, contents).await?;
        self.commit_replace(staged, epoch).await
    }

    pub async fn commit_replace(
        &self,
        mut staged: StagedRuntimeConfig,
        epoch: u64,
    ) -> Result<RuntimeConfigCommit, Error> {
        self.validate_staged(&staged, epoch).await?;
        let target = self.runtime_path(epoch);
        atomic_fs::validate_existing_regular_target(&target).await?;
        atomic_fs::atomic_replace(&staged.path, &target).await?;
        staged.consumed = true;
        #[cfg(feature = "test-hooks")]
        let injected_failure = self
            .replace_parent_sync_failures
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |remaining| {
                remaining.checked_sub(1)
            })
            .is_ok();
        #[cfg(feature = "test-hooks")]
        let parent_sync = if injected_failure {
            Err(std::io::Error::other(
                "injected parent-directory synchronization failure",
            ))
        } else {
            atomic_fs::sync_dir(&self.dir).await
        };
        #[cfg(not(feature = "test-hooks"))]
        let parent_sync = atomic_fs::sync_dir(&self.dir).await;
        Ok(installed_commit(target, parent_sync))
    }

    pub async fn backup(
        &self,
        epoch: u64,
        generation: u64,
    ) -> Result<RuntimeConfigBackup, Error> {
        let target = self.runtime_path(epoch);
        atomic_fs::validate_existing_regular_target(&target).await?;
        let contents = tokio::fs::read(&target).await?;
        let mut staged = self.stage(epoch, &contents).await?;
        let backup_path = self
            .dir
            .join(format!("config-{epoch}.yaml.backup-{generation}"));
        atomic_fs::validate_absent_regular_target(&backup_path).await?;
        atomic_fs::atomic_move_new(&staged.path, &backup_path).await?;
        staged.consumed = true;
        atomic_fs::sync_dir(&self.dir).await?;
        Ok(RuntimeConfigBackup {
            path: backup_path,
            epoch,
        })
    }

    pub async fn restore(
        &self,
        backup: &RuntimeConfigBackup,
    ) -> Result<RuntimeConfigCommit, Error> {
        atomic_fs::validate_existing_regular_target(&backup.path).await?;
        let contents = tokio::fs::read(&backup.path).await?;
        self.replace(backup.epoch, &contents).await
    }

    pub async fn remove_backup(&self, backup: RuntimeConfigBackup) -> Result<(), Error> {
        atomic_fs::remove_regular_file(&backup.path).await?;
        Ok(())
    }

    async fn validate_staged(&self, staged: &StagedRuntimeConfig, epoch: u64) -> Result<(), Error> {
        if staged.path.parent() != Some(self.dir.as_path())
            || !staged
                .path
                .file_name()
                .is_some_and(|name| name.starts_with(&format!(".config-{epoch}.yaml.tmp-")))
        {
            return Err(Error::UnsafeRuntimeArtifact(staged.path.clone()));
        }
        atomic_fs::validate_existing_regular_target(&staged.path).await?;
        Ok(())
    }
}

fn installed_commit(path: Utf8PathBuf, parent_sync: std::io::Result<()>) -> RuntimeConfigCommit {
    let durability = match parent_sync {
        Ok(()) => RuntimeCommitDurability::Durable,
        Err(error) => RuntimeCommitDurability::Uncertain(format!(
            "runtime config was atomically installed, but parent-directory synchronization failed: {error}"
        )),
    };
    RuntimeConfigCommit { path, durability }
}

pub(crate) fn validate_directory_metadata(
    path: &Utf8Path,
    metadata: &std::fs::Metadata,
) -> Result<(), Error> {
    if metadata.file_type().is_symlink()
        || !metadata.is_dir()
        || atomic_fs::is_reparse_point(metadata)
    {
        return Err(Error::UnsafeRuntimeArtifact(path.to_owned()));
    }
    Ok(())
}
