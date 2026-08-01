use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use tokio::io::AsyncWriteExt;

const EPOCH_PID_VERSION: u32 = 2;

/// Describes one manager-owned, per-epoch pid record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EpochPidFile {
    path: PathBuf,
    epoch: u64,
    runtime_config: PathBuf,
}

impl EpochPidFile {
    pub fn new(path: impl Into<PathBuf>, epoch: u64, runtime_config: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            epoch,
            runtime_config: runtime_config.into(),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    pub fn runtime_config(&self) -> &Path {
        &self.runtime_config
    }
}

/// Versioned pid-file contents used for post-manager-kill orphan recovery.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EpochPidRecord {
    pub pid: u32,
    pub epoch: u64,
    pub executable: String,
    pub start_token: u64,
    pub runtime_config: PathBuf,
}

impl EpochPidRecord {
    /// Encode the versioned line protocol used by manager-owned pid files.
    pub fn encode(&self) -> std::io::Result<String> {
        let runtime_config = self.runtime_config.to_str().ok_or_else(|| {
            invalid_input("runtime config path must be UTF-8 for an epoch pid record")
        })?;
        Ok(format!(
            "version={EPOCH_PID_VERSION}\npid={}\nepoch={}\nexecutable={}\nstart-token={}\nruntime-config={}\n",
            self.pid,
            self.epoch,
            hex_encode(self.executable.as_bytes()),
            self.start_token,
            hex_encode(runtime_config.as_bytes()),
        ))
    }

    /// Decode and strictly validate the versioned line protocol.
    pub fn decode(raw: &str) -> std::io::Result<Self> {
        let mut fields = BTreeMap::new();
        for line in raw.lines() {
            let (key, value) = line
                .split_once('=')
                .ok_or_else(|| invalid_data("malformed epoch pid record line"))?;
            if fields.insert(key, value).is_some() {
                return Err(invalid_data("duplicate epoch pid record field"));
            }
        }
        let expected = [
            "epoch",
            "executable",
            "pid",
            "runtime-config",
            "start-token",
            "version",
        ];
        if fields.len() != expected.len() || !expected.iter().all(|key| fields.contains_key(key)) {
            return Err(invalid_data("epoch pid record fields are incomplete"));
        }
        let version = parse_field::<u32>(&fields, "version")?;
        if version != EPOCH_PID_VERSION {
            return Err(invalid_data(format!(
                "unsupported epoch pid record version {version}"
            )));
        }
        let executable = String::from_utf8(hex_decode(required(&fields, "executable")?)?)
            .map_err(|_| invalid_data("epoch pid executable is not UTF-8"))?;
        let runtime_config = String::from_utf8(hex_decode(required(&fields, "runtime-config")?)?)
            .map_err(|_| invalid_data("epoch pid runtime path is not UTF-8"))?;
        Ok(Self {
            pid: parse_field(&fields, "pid")?,
            epoch: parse_field(&fields, "epoch")?,
            executable,
            start_token: parse_field(&fields, "start-token")?,
            runtime_config: PathBuf::from(runtime_config),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrphanReapOutcome {
    NotFound,
    AlreadyExited,
    Killed,
}

/// Stable identity attributes used to distinguish PID reuse.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessIdentity {
    pub executable: String,
    pub start_token: u64,
}

/// Inspect a live process and bind its PID to executable and start time.
pub async fn inspect_process_identity(
    pid: u32,
) -> std::io::Result<Option<ProcessIdentity>> {
    const ATTEMPTS: usize = 20;
    const DELAY: std::time::Duration = std::time::Duration::from_millis(25);

    let mut provisional_error = None;
    for attempt in 0..ATTEMPTS {
        match process_identity(pid) {
            Ok(Some(identity)) => return Ok(Some(identity)),
            Ok(None) => {}
            Err(error) if identity_query_is_provisional(&error) => {
                provisional_error = Some(error);
            }
            Err(error) => return Err(error),
        }
        if attempt + 1 < ATTEMPTS {
            tokio::time::sleep(DELAY).await;
        }
    }
    match provisional_error {
        Some(error) => Err(error),
        None => Ok(None),
    }
}

pub fn record_matches_identity(record: &EpochPidRecord, identity: &ProcessIdentity) -> bool {
    record.start_token == identity.start_token
        && executable_names_equal(&record.executable, &identity.executable)
}

#[cfg(windows)]
fn executable_names_equal(left: &str, right: &str) -> bool {
    left.eq_ignore_ascii_case(right)
}

#[cfg(not(windows))]
fn executable_names_equal(left: &str, right: &str) -> bool {
    left == right
}

fn identity_query_is_provisional(error: &std::io::Error) -> bool {
    if error.kind() == std::io::ErrorKind::PermissionDenied {
        return true;
    }
    #[cfg(windows)]
    {
        matches!(error.raw_os_error(), Some(6 | 31 | 87 | 1168))
    }
    #[cfg(not(windows))]
    false
}

#[cfg(windows)]
