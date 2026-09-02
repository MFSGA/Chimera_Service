//! Durable JSONL archive for the manager's structured core-log stream.

use std::sync::Arc;

use camino::{Utf8Path, Utf8PathBuf};
use serde::Serialize;
use tokio::{
    io::AsyncWriteExt,
    sync::broadcast::{
        Receiver,
        error::{RecvError, TryRecvError},
    },
};
use tokio_util::sync::CancellationToken;

use crate::{
    Error,
    config::runtime_store::validate_directory_metadata,
    log::LogFrame,
};

const LOG_DIR_NAME: &str = "logs";
const FILE_PREFIX: &str = "core-";
const FILE_SUFFIX: &str = ".jsonl";
const MAX_BATCH_RECORDS: usize = 256;

#[derive(Debug, Clone, Copy)]
pub(crate) struct SinkOptions {
    pub max_bytes: u64,
    pub max_files: usize,
}

pub(crate) struct SinkHandle {
    cancel: CancellationToken,
    task: tokio::task::JoinHandle<()>,
}

impl SinkHandle {
    pub(crate) async fn shutdown(mut self) {
        self.cancel.cancel();
        match tokio::time::timeout(std::time::Duration::from_secs(5), &mut self.task).await {
            Ok(_) => {}
            Err(_) => {
                tracing::warn!("timed out waiting for the core log sink to shut down");
                self.task.abort();
                let _ = self.task.await;
            }
        }
    }
}

#[derive(Serialize)]
struct LogRecord<'a> {
    t: &'static str,
    #[serde(flatten)]
    frame: &'a LogFrame,
}

#[derive(Serialize)]
struct GapRecord {
    t: &'static str,
    at: i64,
    dropped: u64,
}

enum Entry {
    Log(Arc<LogFrame>),
    Gap(u64),
}

pub(crate) async fn prepare_dir(parent: &Utf8Path) -> Result<Utf8PathBuf, Error> {
    let dir = parent.join(LOG_DIR_NAME);
    match tokio::fs::symlink_metadata(&dir).await {
        Ok(metadata) => validate_directory_metadata(&dir, &metadata)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            tokio::fs::create_dir_all(&dir).await?;
            let metadata = tokio::fs::symlink_metadata(&dir).await?;
            validate_directory_metadata(&dir, &metadata)?;
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
        nyanpasu_utils::io::atomic_fs::harden_windows_directory_acl(&dir)?;
        nyanpasu_utils::io::atomic_fs::verify_windows_directory_acl(&dir)?;
    }

    let canonical = tokio::fs::canonicalize(&dir).await?;
    Utf8PathBuf::from_path_buf(canonical)
        .map_err(|_| Error::InvalidManagerOptions("core log directory is not UTF-8".into()))
}

pub(crate) async fn spawn(
    dir: Utf8PathBuf,
    options: SinkOptions,
    logs: Receiver<Arc<LogFrame>>,
    cancel: CancellationToken,
) -> Result<SinkHandle, Error> {
    let writer = Writer::open(dir, options).await?;
    let task_cancel = cancel.clone();
    let task = tokio::spawn(run(writer, logs, task_cancel));
    Ok(SinkHandle { cancel, task })
}

async fn run(mut writer: Writer, mut logs: Receiver<Arc<LogFrame>>, cancel: CancellationToken) {
    let mut batch = Vec::with_capacity(MAX_BATCH_RECORDS);
    loop {
        batch.clear();
        let first = tokio::select! {
            _ = cancel.cancelled() => {
                drain_shutdown(&mut writer, &mut logs, &mut batch).await;
                break;
            }
            received = logs.recv() => received,
        };
        let closed = push_received(first, &mut batch);
        let closed = closed || drain(&mut logs, &mut batch);
        if !batch.is_empty()
            && let Err(error) = writer.write(&batch).await
        {
            tracing::error!("core log archive stopped after write failure: {error}");
            break;
        }
        if closed {
            break;
        }
    }
}

async fn drain_shutdown(
    writer: &mut Writer,
    logs: &mut Receiver<Arc<LogFrame>>,
    batch: &mut Vec<Entry>,
) {
    loop {
        batch.clear();
        let closed = drain(logs, batch);
        if batch.is_empty() {
            break;
        }
        if let Err(error) = writer.write(batch).await {
            tracing::error!("core log archive shutdown drain failed: {error}");
            break;
        }
        if closed || batch.len() < MAX_BATCH_RECORDS {
            break;
        }
    }
}

fn push_received(received: Result<Arc<LogFrame>, RecvError>, batch: &mut Vec<Entry>) -> bool {
    match received {
        Ok(frame) => {
            batch.push(Entry::Log(frame));
            false
        }
        Err(RecvError::Lagged(dropped)) => {
            batch.push(Entry::Gap(dropped));
            false
        }
        Err(RecvError::Closed) => true,
    }
}

fn drain(logs: &mut Receiver<Arc<LogFrame>>, batch: &mut Vec<Entry>) -> bool {
    while batch.len() < MAX_BATCH_RECORDS {
        match logs.try_recv() {
            Ok(frame) => batch.push(Entry::Log(frame)),
            Err(TryRecvError::Lagged(dropped)) => batch.push(Entry::Gap(dropped)),
            Err(TryRecvError::Empty) => return false,
            Err(TryRecvError::Closed) => return true,
        }
    }
    false
}

struct Writer {
    dir: Utf8PathBuf,
    options: SinkOptions,
    index: u64,
    path: Utf8PathBuf,
    file: tokio::fs::File,
    bytes: u64,
}

impl Writer {
    async fn open(dir: Utf8PathBuf, options: SinkOptions) -> Result<Self, Error> {
        let index = next_index(&dir).await?;
        let (path, file) = open_file(&dir, index).await?;
        let writer = Self {
            dir,
            options,
            index,
            path,
            file,
            bytes: 0,
        };
        writer.prune().await?;
        Ok(writer)
    }

    async fn write(&mut self, entries: &[Entry]) -> std::io::Result<()> {
        let mut chunk = Vec::new();
        for entry in entries {
            let record = encode_entry(entry)?;
            let pending = chunk.len() as u64;
            if self.bytes + pending > 0
                && self.bytes + pending + record.len() as u64 > self.options.max_bytes
            {
                self.flush_chunk(&mut chunk).await?;
                self.rotate().await?;
            }
            chunk.extend_from_slice(&record);
        }
        self.flush_chunk(&mut chunk).await?;
        self.file.flush().await
    }

    async fn flush_chunk(&mut self, chunk: &mut Vec<u8>) -> std::io::Result<()> {
        if chunk.is_empty() {
            return Ok(());
        }
        self.file.write_all(chunk).await?;
        self.bytes += chunk.len() as u64;
        chunk.clear();
        Ok(())
    }

    async fn rotate(&mut self) -> std::io::Result<()> {
        self.file.flush().await?;
        self.index = self.index.saturating_add(1);
        let (path, file) = open_file(&self.dir, self.index).await?;
        self.path = path;
        self.file = file;
        self.bytes = 0;
        self.prune().await
    }

    async fn prune(&self) -> std::io::Result<()> {
        let mut files = archive_files(&self.dir).await?;
        while files.len() > self.options.max_files {
            let (_, path) = files.remove(0);
            if path == self.path {
                continue;
            }
            match tokio::fs::remove_file(&path).await {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }
}

fn encode_entry(entry: &Entry) -> std::io::Result<Vec<u8>> {
    let mut encoded = match entry {
        Entry::Log(frame) => serde_json::to_vec(&LogRecord {
            t: "log",
            frame: frame.as_ref(),
        }),
        Entry::Gap(dropped) => serde_json::to_vec(&GapRecord {
            t: "gap",
            at: chrono::Utc::now().timestamp_millis(),
            dropped: *dropped,
        }),
    }
    .map_err(std::io::Error::other)?;
    encoded.push(b'\n');
    Ok(encoded)
}

async fn next_index(dir: &Utf8Path) -> std::io::Result<u64> {
    Ok(archive_files(dir)
        .await?
        .last()
        .map_or(1, |(index, _)| index.saturating_add(1)))
}

async fn archive_files(dir: &Utf8Path) -> std::io::Result<Vec<(u64, Utf8PathBuf)>> {
    let mut entries = tokio::fs::read_dir(dir).await?;
    let mut files = Vec::new();
    while let Some(entry) = entries.next_entry().await? {
        let file_type = entry.file_type().await?;
        if !file_type.is_file() {
            continue;
        }
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let Some(index) = parse_index(&name) else {
            continue;
        };
        let path = Utf8PathBuf::from_path_buf(entry.path()).map_err(|path| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("core log path is not UTF-8: {}", path.display()),
            )
        })?;
        files.push((index, path));
    }
    files.sort_by_key(|(index, _)| *index);
    Ok(files)
}

fn parse_index(name: &str) -> Option<u64> {
    name.strip_prefix(FILE_PREFIX)?
        .strip_suffix(FILE_SUFFIX)?
        .parse()
        .ok()
}

async fn open_file(dir: &Utf8Path, index: u64) -> std::io::Result<(Utf8PathBuf, tokio::fs::File)> {
    let path = dir.join(format!("{FILE_PREFIX}{index:06}{FILE_SUFFIX}"));
    let mut options = tokio::fs::OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(&path).await?;
    Ok((path, file))
}

#[cfg(test)]
mod tests {
    use super::*;
    use nyanpasu_core_metadata::{ClashCoreKind, LogLevel, LogStream};

    fn frame(epoch: u64, message: &str) -> Arc<LogFrame> {
        Arc::new(LogFrame {
            at: 1_700_000_000_000,
            epoch,
            kind: ClashCoreKind::Mihomo,
            stream: LogStream::Stdout,
            level: LogLevel::Info,
            timestamp: None,
            target: None,
            message: message.into(),
            fields: Vec::new(),
            raw: message.into(),
            truncated: false,
        })
    }

    #[tokio::test]
    async fn rotation_retains_only_the_budgeted_files() {
        let temp = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();
        let dir = prepare_dir(&root).await.unwrap();
        let mut writer = Writer::open(
            dir.clone(),
            SinkOptions {
                max_bytes: 80,
                max_files: 2,
            },
        )
        .await
        .unwrap();
        for epoch in 1..=10 {
            writer.write(&[Entry::Log(frame(epoch, "a moderately long record"))]).await.unwrap();
        }
        assert!(archive_files(&dir).await.unwrap().len() <= 2);
    }

    #[test]
    fn gap_and_log_records_are_distinguishable() {
        let log = encode_entry(&Entry::Log(frame(1, "hello"))).unwrap();
        let gap = encode_entry(&Entry::Gap(4)).unwrap();
        assert!(String::from_utf8(log).unwrap().contains("\"t\":\"log\""));
        assert!(String::from_utf8(gap).unwrap().contains("\"t\":\"gap\""));
    }
}
