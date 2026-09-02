use camino::Utf8PathBuf;
use nyanpasu_utils::io::atomic_fs;

use crate::Error;

use super::RuntimeConfigStore;

impl RuntimeConfigStore {
    pub async fn cleanup_epoch(&self, epoch: u64) -> Result<(), Error> {
        for path in [self.runtime_path(epoch), self.pid_path(epoch)] {
            atomic_fs::remove_regular_file(&path).await?;
        }
        remove_socket_artifact(&self.socket_path(epoch)).await?;

        let backup_prefix = format!("config-{epoch}.yaml.backup-");
        let temp_prefix = format!(".config-{epoch}.yaml.tmp-");
        let pid_temp_prefix = format!("core-{epoch}.pid.tmp-");
        let mut entries = tokio::fs::read_dir(&self.dir).await?;
        while let Some(entry) = entries.next_entry().await? {
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            if name.starts_with(&backup_prefix)
                || name.starts_with(&temp_prefix)
                || name.starts_with(&pid_temp_prefix)
            {
                let path = Utf8PathBuf::from_path_buf(entry.path())
                    .map_err(|_| Error::UnsafeRuntimeArtifact(self.dir.clone()))?;
                atomic_fs::remove_regular_file(&path).await?;
            }
        }
        atomic_fs::sync_dir(&self.dir).await?;
        Ok(())
    }

    pub async fn artifact_epochs(&self) -> Result<Vec<u64>, Error> {
        let mut epochs = Vec::new();
        let mut entries = tokio::fs::read_dir(&self.dir).await?;
        while let Some(entry) = entries.next_entry().await? {
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            if let Some(epoch) = artifact_epoch(&name) {
                epochs.push(epoch);
            }
        }
        epochs.sort_unstable();
        epochs.dedup();
        Ok(epochs)
    }
}

fn artifact_epoch(name: &str) -> Option<u64> {
    let value = name
        .strip_prefix("core-")
        .and_then(|value| value.strip_suffix(".pid"))
        .or_else(|| {
            name.strip_prefix("core-")
                .and_then(|value| value.strip_suffix(".sock"))
        })
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
        .or_else(|| {
            name.strip_prefix("core-")
                .and_then(|value| value.split_once(".pid.tmp-").map(|(epoch, _)| epoch))
        })?;
    value.parse().ok()
}

async fn remove_socket_artifact(path: &camino::Utf8Path) -> Result<(), Error> {
    match tokio::fs::symlink_metadata(path).await {
        Ok(metadata)
            if metadata.file_type().is_symlink() || atomic_fs::is_reparse_point(&metadata) =>
        {
            Err(Error::UnsafeRuntimeArtifact(path.to_owned()))
        }
        #[cfg(unix)]
        Ok(metadata) if !std::os::unix::fs::FileTypeExt::is_socket(&metadata.file_type()) => {
            Err(Error::UnsafeRuntimeArtifact(path.to_owned()))
        }
        #[cfg(windows)]
        Ok(metadata) if !metadata.is_file() => Err(Error::UnsafeRuntimeArtifact(path.to_owned())),
        Ok(_) => {
            tokio::fs::remove_file(path).await?;
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}
