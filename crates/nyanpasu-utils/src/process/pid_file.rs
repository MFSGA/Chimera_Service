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
pub async fn inspect_process_identity(pid: u32) -> std::io::Result<Option<ProcessIdentity>> {
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
        start_token: (u64::from(creation.dwHighDateTime) << 32) | u64::from(creation.dwLowDateTime),
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
pub async fn read_epoch_pid_file(
    path: impl AsRef<Path>,
) -> std::io::Result<Option<EpochPidRecord>> {
    let path = path.as_ref();
    match tokio::fs::symlink_metadata(path).await {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            return Err(invalid_input(format!(
                "pid file must not be a symlink: {}",
                path.display()
            )));
        }
        Ok(metadata) if !metadata.is_file() => {
            return Err(invalid_input(format!(
                "pid file must be a regular file: {}",
                path.display()
            )));
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    }
    let raw = tokio::fs::read_to_string(path).await?;
    EpochPidRecord::decode(&raw).map(Some)
}

/// Publish a new epoch record without replacing an existing owner.
pub async fn publish_epoch_pid_file(
    path: impl AsRef<Path>,
    record: &EpochPidRecord,
) -> std::io::Result<()> {
    static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

    let path = path.as_ref();
    validate_pid_target(path).await?;
    if tokio::fs::try_exists(path).await? {
        return Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            format!("pid file unexpectedly exists: {}", path.display()),
        ));
    }
    let parent = path
        .parent()
        .ok_or_else(|| invalid_input("pid file has no parent directory"))?;
    if !tokio::fs::metadata(parent).await?.is_dir() {
        return Err(invalid_input("pid file parent must be a directory"));
    }

    let counter = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| invalid_input("pid filename must be UTF-8"))?;
    let temp = parent.join(format!(".{file_name}.tmp-{}-{counter}", std::process::id()));
    let result = async {
        let mut file = tokio::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temp)
            .await?;
        file.write_all(record.encode()?.as_bytes()).await?;
        file.flush().await?;
        file.sync_all().await?;
        drop(file);

        tokio::fs::hard_link(&temp, path).await.map_err(|error| {
            if error.kind() == std::io::ErrorKind::AlreadyExists {
                std::io::Error::new(
                    std::io::ErrorKind::AlreadyExists,
                    format!("pid file unexpectedly exists: {}", path.display()),
                )
            } else {
                error
            }
        })
    }
    .await;
    let _ = tokio::fs::remove_file(&temp).await;
    result
}

/// Remove a record only if the destination still contains the expected owner.
pub async fn remove_epoch_pid_file_if_matches(
    path: impl AsRef<Path>,
    expected: &EpochPidRecord,
) -> std::io::Result<()> {
    let path = path.as_ref();
    if read_epoch_pid_file(path).await?.as_ref() != Some(expected) {
        return Ok(());
    }
    match tokio::fs::remove_file(path).await {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

/// Reap a recorded orphan only after proving the full epoch and process identity.
pub async fn reap_epoch_pid_file(
    path: impl AsRef<Path>,
    runtime_dir: impl AsRef<Path>,
) -> std::io::Result<OrphanReapOutcome> {
    let path = path.as_ref();
    let runtime_dir = tokio::fs::canonicalize(runtime_dir.as_ref()).await?;
    let parent = path
        .parent()
        .ok_or_else(|| invalid_input("pid file has no parent directory"))?;
    if tokio::fs::canonicalize(parent).await? != runtime_dir {
        return Err(invalid_input("pid file escapes the runtime directory"));
    }
    let epoch = epoch_from_file_name(path, "core-", ".pid")?;
    let Some(record) = read_epoch_pid_file(path).await? else {
        return Ok(OrphanReapOutcome::NotFound);
    };
    if record.epoch != epoch {
        return Err(invalid_data("pid filename and embedded epoch differ"));
    }
    let runtime_parent = record
        .runtime_config
        .parent()
        .ok_or_else(|| invalid_input("runtime config has no parent directory"))?;
    if tokio::fs::canonicalize(runtime_parent).await? != runtime_dir {
        return Err(invalid_input(
            "runtime config escapes the runtime directory",
        ));
    }
    let runtime_epoch = epoch_from_file_name(&record.runtime_config, "config-", ".yaml")?;
    if runtime_epoch != epoch {
        return Err(invalid_data(
            "runtime config filename and embedded epoch differ",
        ));
    }
    validate_runtime_target(&record.runtime_config).await?;

    let outcome = reap_record(&record).await?;
    remove_epoch_pid_file_if_matches(path, &record).await?;
    Ok(outcome)
}

async fn reap_record(record: &EpochPidRecord) -> std::io::Result<OrphanReapOutcome> {
    let Some(identity) = inspect_process_identity(record.pid).await? else {
        return Ok(OrphanReapOutcome::AlreadyExited);
    };
    if !record_matches_identity(record, &identity) {
        return Err(identity_error(format!(
            "cannot prove ownership of live pid {}",
            record.pid
        )));
    }
    kill_recorded_process(record).await?;
    wait_for_recorded_exit(record).await?;
    Ok(OrphanReapOutcome::Killed)
}

async fn wait_for_recorded_exit(record: &EpochPidRecord) -> std::io::Result<()> {
    const ATTEMPTS: usize = 100;
    const DELAY: std::time::Duration = std::time::Duration::from_millis(50);

    for attempt in 0..ATTEMPTS {
        match process_identity(record.pid) {
            Ok(None) => return Ok(()),
            Ok(Some(identity)) if !record_matches_identity(record, &identity) => return Ok(()),
            Ok(Some(_)) => {}
            Err(error) if identity_query_is_provisional(&error) => {}
            Err(error) => return Err(error),
        }
        if attempt + 1 < ATTEMPTS {
            tokio::time::sleep(DELAY).await;
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::TimedOut,
        format!("recorded pid {} did not exit", record.pid),
    ))
}

#[cfg(windows)]
async fn kill_recorded_process(record: &EpochPidRecord) -> std::io::Result<()> {
    use windows::Win32::System::Threading::{
        PROCESS_ACCESS_RIGHTS, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_TERMINATE,
        TerminateProcess,
    };

    const PROCESS_SYNCHRONIZE: PROCESS_ACCESS_RIGHTS = PROCESS_ACCESS_RIGHTS(0x0010_0000);
    let handle = open_process(
        record.pid,
        PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_TERMINATE | PROCESS_SYNCHRONIZE,
    )?;
    let identity = process_identity_from_handle(&handle, record.pid)?;
    if !record_matches_identity(record, &identity) {
        return Err(identity_error(format!(
            "cannot prove ownership of live pid {}",
            record.pid
        )));
    }
    unsafe { TerminateProcess(handle.0, 1) }.map_err(windows_io_error)
}

#[cfg(target_os = "linux")]
async fn kill_recorded_process(record: &EpochPidRecord) -> std::io::Result<()> {
    let pidfd = LinuxPidFd::open(record.pid)?;
    let identity = process_identity(record.pid)?.ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("pid {} exited before pidfd validation", record.pid),
        )
    })?;
    if !record_matches_identity(record, &identity) {
        return Err(identity_error(format!(
            "cannot prove ownership of live pid {}",
            record.pid
        )));
    }
    pidfd.kill()
}

#[cfg(target_os = "linux")]
struct LinuxPidFd(std::os::fd::RawFd);

#[cfg(target_os = "linux")]
impl LinuxPidFd {
    fn open(pid: u32) -> std::io::Result<Self> {
        let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid as libc::pid_t, 0_u32) };
        if fd < 0 {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(Self(fd as std::os::fd::RawFd))
        }
    }

    fn kill(&self) -> std::io::Result<()> {
        let result = unsafe {
            libc::syscall(
                libc::SYS_pidfd_send_signal,
                self.0,
                libc::SIGKILL,
                std::ptr::null::<libc::siginfo_t>(),
                0_u32,
            )
        };
        if result < 0 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::ESRCH) {
                Ok(())
            } else {
                Err(error)
            }
        } else {
            Ok(())
        }
    }
}

#[cfg(target_os = "linux")]
impl Drop for LinuxPidFd {
    fn drop(&mut self) {
        unsafe { libc::close(self.0) };
    }
}

#[cfg(all(unix, not(target_os = "linux")))]
async fn kill_recorded_process(record: &EpochPidRecord) -> std::io::Result<()> {
    let identity = process_identity(record.pid)?.ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("pid {} exited before final validation", record.pid),
        )
    })?;
    if !record_matches_identity(record, &identity) {
        return Err(identity_error(format!(
            "cannot prove ownership of live pid {}",
            record.pid
        )));
    }
    kill_tree::tokio::kill_tree(record.pid)
        .await
        .map(|_| ())
        .map_err(|error| std::io::Error::other(error.to_string()))
}

async fn validate_runtime_target(path: &Path) -> std::io::Result<()> {
    match tokio::fs::symlink_metadata(path).await {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(invalid_input(format!(
            "runtime config must not be a symlink: {}",
            path.display()
        ))),
        Ok(metadata) if !metadata.is_file() => Err(invalid_input(format!(
            "runtime config must be a regular file: {}",
            path.display()
        ))),
        Ok(_) => Ok(()),
        Err(error) => Err(error),
    }
}

fn epoch_from_file_name(path: &Path, prefix: &str, suffix: &str) -> std::io::Result<u64> {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| invalid_input("epoch artifact filename must be UTF-8"))?;
    let epoch = name
        .strip_prefix(prefix)
        .and_then(|value| value.strip_suffix(suffix))
        .ok_or_else(|| invalid_input(format!("invalid epoch artifact name `{name}`")))?;
    epoch
        .parse()
        .map_err(|_| invalid_input(format!("invalid epoch in artifact name `{name}`")))
}

async fn validate_pid_target(path: &Path) -> std::io::Result<()> {
    match tokio::fs::symlink_metadata(path).await {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(invalid_input(format!(
            "pid file must not be a symlink: {}",
            path.display()
        ))),
        Ok(metadata) if !metadata.is_file() => Err(invalid_input(format!(
            "pid file must be a regular file: {}",
            path.display()
        ))),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn required<'a>(fields: &'a BTreeMap<&str, &str>, key: &str) -> std::io::Result<&'a str> {
    fields
        .get(key)
        .copied()
        .ok_or_else(|| invalid_data(format!("missing epoch pid field `{key}`")))
}

fn parse_field<T: std::str::FromStr>(
    fields: &BTreeMap<&str, &str>,
    key: &str,
) -> std::io::Result<T> {
    required(fields, key)?
        .parse()
        .map_err(|_| invalid_data(format!("invalid epoch pid field `{key}`")))
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(HEX[(byte >> 4) as usize] as char);
        output.push(HEX[(byte & 0x0f) as usize] as char);
    }
    output
}

fn hex_decode(value: &str) -> std::io::Result<Vec<u8>> {
    if value.len() % 2 != 0 {
        return Err(invalid_data("hex field has odd length"));
    }
    value
        .as_bytes()
        .chunks_exact(2)
        .map(|pair| {
            let high = hex_nibble(pair[0])?;
            let low = hex_nibble(pair[1])?;
            Ok((high << 4) | low)
        })
        .collect()
}

fn hex_nibble(value: u8) -> std::io::Result<u8> {
    match value {
        b'0'..=b'9' => Ok(value - b'0'),
        b'a'..=b'f' => Ok(value - b'a' + 10),
        _ => Err(invalid_data("invalid hex field")),
    }
}

fn invalid_input(message: impl Into<String>) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidInput, message.into())
}

fn invalid_data(message: impl Into<String>) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, message.into())
}

fn identity_error(message: impl Into<String>) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::PermissionDenied, message.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record() -> EpochPidRecord {
        EpochPidRecord {
            pid: 42,
            epoch: 7,
            executable: "core=name.exe".into(),
            start_token: 99,
            runtime_config: PathBuf::from(r"C:\run dir\config-7.yaml"),
        }
    }

    #[test]
    fn epoch_record_round_trips() {
        let record = record();
        assert_eq!(
            EpochPidRecord::decode(&record.encode().unwrap()).unwrap(),
            record
        );
    }

    #[test]
    fn malformed_or_incomplete_records_are_rejected() {
        assert_eq!(
            EpochPidRecord::decode("pid=1\n").unwrap_err().kind(),
            std::io::ErrorKind::InvalidData
        );
        let duplicate =
            "version=2\npid=1\npid=2\nepoch=1\nexecutable=61\nstart-token=1\nruntime-config=61\n";
        assert_eq!(
            EpochPidRecord::decode(duplicate).unwrap_err().kind(),
            std::io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn unknown_versions_and_invalid_hex_are_rejected() {
        let encoded = record()
            .encode()
            .unwrap()
            .replace("version=2", "version=99");
        assert_eq!(
            EpochPidRecord::decode(&encoded).unwrap_err().kind(),
            std::io::ErrorKind::InvalidData
        );
        let encoded = record()
            .encode()
            .unwrap()
            .replace("executable=", "executable=z");
        assert_eq!(
            EpochPidRecord::decode(&encoded).unwrap_err().kind(),
            std::io::ErrorKind::InvalidData
        );
    }

    #[tokio::test]
    async fn publish_then_read_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("core-7.pid");
        let record = record();
        publish_epoch_pid_file(&path, &record).await.unwrap();
        assert_eq!(read_epoch_pid_file(&path).await.unwrap(), Some(record));
        assert!(dir.path().read_dir().unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .contains(".tmp-")
        }));
    }

    #[tokio::test]
    async fn second_publish_does_not_clobber_the_first_owner() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("core-7.pid");
        let first = record();
        let mut second = record();
        second.pid = 99;
        second.executable = "second.exe".into();

        publish_epoch_pid_file(&path, &first).await.unwrap();
        let error = publish_epoch_pid_file(&path, &second)
            .await
            .expect_err("existing owner must not be replaced");
        assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(read_epoch_pid_file(&path).await.unwrap(), Some(first));
    }

    #[tokio::test]
    async fn conditional_remove_never_deletes_a_new_owner() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("core-7.pid");
        let current = record();
        let mut stale = record();
        stale.pid = 1;
        publish_epoch_pid_file(&path, &current).await.unwrap();

        remove_epoch_pid_file_if_matches(&path, &stale)
            .await
            .unwrap();
        assert_eq!(
            read_epoch_pid_file(&path).await.unwrap(),
            Some(current.clone())
        );
        remove_epoch_pid_file_if_matches(&path, &current)
            .await
            .unwrap();
        assert_eq!(read_epoch_pid_file(&path).await.unwrap(), None);
    }

    #[tokio::test]
    async fn live_identity_binds_pid_to_executable_and_start_time() {
        let identity = inspect_process_identity(std::process::id())
            .await
            .unwrap()
            .expect("the test process is live");
        assert!(!identity.executable.is_empty());
        assert_ne!(identity.start_token, 0);

        let mut record = record();
        record.pid = std::process::id();
        record.executable = identity.executable.clone();
        record.start_token = identity.start_token;
        assert!(record_matches_identity(&record, &identity));
        record.start_token = record.start_token.wrapping_add(1);
        assert!(!record_matches_identity(&record, &identity));
    }
}
