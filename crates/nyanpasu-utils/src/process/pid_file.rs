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
fn process_identity(pid: u32) -> std::io::Result<Option<ProcessIdentity>> {
    use windows::Win32::System::Threading::{
        PROCESS_ACCESS_RIGHTS, PROCESS_QUERY_LIMITED_INFORMATION,
    };

    const PROCESS_SYNCHRONIZE: PROCESS_ACCESS_RIGHTS = PROCESS_ACCESS_RIGHTS(0x0010_0000);
    let handle = match open_process(pid, PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE) {
        Ok(handle) => handle,
        Err(error) if error.raw_os_error() == Some(87) => return Ok(None),
        Err(error) => return Err(error),
    };
    match process_identity_from_handle(&handle, pid) {
        Ok(identity) => Ok(Some(identity)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

#[cfg(windows)]
struct OwnedProcessHandle(windows::Win32::Foundation::HANDLE);

#[cfg(windows)]
impl Drop for OwnedProcessHandle {
    fn drop(&mut self) {
        let _ = unsafe { windows::Win32::Foundation::CloseHandle(self.0) };
    }
}

#[cfg(windows)]
fn open_process(
    pid: u32,
    access: windows::Win32::System::Threading::PROCESS_ACCESS_RIGHTS,
) -> std::io::Result<OwnedProcessHandle> {
    unsafe { windows::Win32::System::Threading::OpenProcess(access, false, pid) }
        .map(OwnedProcessHandle)
        .map_err(windows_io_error)
}

#[cfg(windows)]
fn process_identity_from_handle(
    handle: &OwnedProcessHandle,
    pid: u32,
) -> std::io::Result<ProcessIdentity> {
    use std::os::windows::ffi::OsStringExt;
    use windows::{
        Win32::{
            Foundation::{FILETIME, WAIT_OBJECT_0, WAIT_TIMEOUT},
            System::Threading::{
                GetProcessTimes, PROCESS_NAME_WIN32, QueryFullProcessImageNameW,
                WaitForSingleObject,
            },
        },
        core::PWSTR,
    };

    match unsafe { WaitForSingleObject(handle.0, 0) } {
        WAIT_OBJECT_0 => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("pid {pid} has terminated"),
            ));
        }
        WAIT_TIMEOUT => {}
        _ => return Err(std::io::Error::last_os_error()),
    }

    let mut creation = FILETIME::default();
    let mut exit = FILETIME::default();
    let mut kernel = FILETIME::default();
    let mut user = FILETIME::default();
    unsafe { GetProcessTimes(handle.0, &mut creation, &mut exit, &mut kernel, &mut user) }
        .map_err(windows_io_error)?;

    let mut executable = vec![0_u16; 32_768];
    let mut len = executable.len() as u32;
    unsafe {
        QueryFullProcessImageNameW(
            handle.0,
            PROCESS_NAME_WIN32,
            PWSTR(executable.as_mut_ptr()),
            &mut len,
        )
    }
    .map_err(windows_io_error)?;
    let path = PathBuf::from(std::ffi::OsString::from_wide(&executable[..len as usize]));
    let executable = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| identity_error(format!("cannot resolve executable for live pid {pid}")))?;
    Ok(ProcessIdentity {
        executable: executable.to_owned(),
        start_token: (u64::from(creation.dwHighDateTime) << 32)
            | u64::from(creation.dwLowDateTime),
    })
}

#[cfg(windows)]
fn windows_io_error(error: windows::core::Error) -> std::io::Error {
    let code = error.code().0 as u32;
    if code & 0xffff_0000 == 0x8007_0000 {
        std::io::Error::from_raw_os_error((code & 0xffff) as i32)
    } else {
        std::io::Error::other(error)
    }
}

#[cfg(target_os = "linux")]
fn process_identity(pid: u32) -> std::io::Result<Option<ProcessIdentity>> {
    let Some(first_ticks) = linux_start_ticks(pid)? else {
        return Ok(None);
    };
    let executable_path = match std::fs::read_link(format!("/proc/{pid}/exe")) {
        Ok(path) => path,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let Some(second_ticks) = linux_start_ticks(pid)? else {
        return Ok(None);
    };
    if first_ticks != second_ticks {
        return Ok(None);
    }
    let executable = executable_path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| identity_error(format!("cannot resolve executable for live pid {pid}")))?;
    let boot_id = std::fs::read_to_string("/proc/sys/kernel/random/boot_id")?;
    Ok(Some(ProcessIdentity {
        executable: executable.to_owned(),
        start_token: boot_bound_start_token(boot_id.trim(), first_ticks),
    }))
}

#[cfg(target_os = "linux")]
fn linux_start_ticks(pid: u32) -> std::io::Result<Option<u64>> {
    let stat = match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(stat) => stat,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let command_end = stat
        .rfind(')')
        .ok_or_else(|| invalid_data(format!("malformed /proc/{pid}/stat")))?;
    let ticks = stat[command_end + 1..]
        .split_whitespace()
        .nth(19)
        .ok_or_else(|| invalid_data(format!("missing start time in /proc/{pid}/stat")))?
        .parse()
        .map_err(|_| invalid_data(format!("invalid start time in /proc/{pid}/stat")))?;
    Ok(Some(ticks))
}

#[cfg(target_os = "linux")]
fn boot_bound_start_token(boot_id: &str, ticks: u64) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in boot_id.bytes().chain(ticks.to_le_bytes()) {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

#[cfg(all(not(windows), not(target_os = "linux")))]
fn process_identity(pid: u32) -> std::io::Result<Option<ProcessIdentity>> {
    use sysinfo::{Pid, ProcessRefreshKind, RefreshKind, System, UpdateKind};

    let kind = RefreshKind::nothing()
        .with_processes(ProcessRefreshKind::nothing().with_exe(UpdateKind::Always));
    let mut system = System::new_with_specifics(kind);
    system.refresh_specifics(kind);
    let Some(process) = system.process(Pid::from_u32(pid)) else {
        return Ok(None);
    };
    let executable = process
        .exe()
        .and_then(Path::file_name)
        .and_then(|name| name.to_str())
        .ok_or_else(|| identity_error(format!("cannot resolve executable for live pid {pid}")))?;
    Ok(Some(ProcessIdentity {
        executable: executable.to_owned(),
        start_token: process.start_time(),
    }))
}

/// Read a structured epoch pid record. Numeric legacy files are rejected.
