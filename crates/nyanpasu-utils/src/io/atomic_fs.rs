use std::path::{Path, PathBuf};

#[cfg(windows)]
mod windows_acl;
#[cfg(windows)]
pub use windows_acl::{harden_windows_directory_acl, verify_windows_directory_acl};

#[derive(Debug, thiserror::Error)]
pub enum AtomicFsError {
    #[error("unsafe filesystem artifact: {0}")]
    UnsafePath(PathBuf),
    #[error("directory lock is held by another process: {0}")]
    Contended(PathBuf),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

#[derive(Debug)]
pub struct DirLock {
    #[cfg(unix)]
    _file: nix::fcntl::Flock<std::fs::File>,
    #[cfg(not(unix))]
    _file: std::fs::File,
}

pub fn acquire_dir_lock(path: impl AsRef<Path>) -> Result<DirLock, AtomicFsError> {
    let path = path.as_ref();
    match std::fs::symlink_metadata(path) {
        Ok(metadata)
            if metadata.file_type().is_symlink()
                || !metadata.is_file()
                || is_reparse_point(&metadata) =>
        {
            return Err(AtomicFsError::UnsafePath(path.to_owned()));
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }

    let mut options = std::fs::OpenOptions::new();
    options.create(true).read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.share_mode(0);
    }
    let file = options.open(path).map_err(|error| {
        if dir_lock_is_contended(&error) {
            AtomicFsError::Contended(path.to_owned())
        } else {
            error.into()
        }
    })?;

    #[cfg(unix)]
    {
        let file = nix::fcntl::Flock::lock(file, nix::fcntl::FlockArg::LockExclusiveNonblock)
            .map_err(|(_, errno)| {
                let error: std::io::Error = errno.into();
                if dir_lock_is_contended(&error) {
                    AtomicFsError::Contended(path.to_owned())
                } else {
                    error.into()
                }
            })?;
        Ok(DirLock { _file: file })
    }
    #[cfg(not(unix))]
    {
        Ok(DirLock { _file: file })
    }
}

#[cfg(unix)]
fn dir_lock_is_contended(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::PermissionDenied
    )
}

#[cfg(windows)]
fn dir_lock_is_contended(error: &std::io::Error) -> bool {
    matches!(error.raw_os_error(), Some(5 | 32 | 33))
}

#[cfg(not(any(unix, windows)))]
fn dir_lock_is_contended(_error: &std::io::Error) -> bool {
    false
}

pub async fn validate_absent_regular_target(path: impl AsRef<Path>) -> Result<(), AtomicFsError> {
    let path = path.as_ref();
    match tokio::fs::symlink_metadata(path).await {
        Ok(metadata)
            if metadata.file_type().is_symlink()
                || !metadata.is_file()
                || is_reparse_point(&metadata) =>
        {
            Err(AtomicFsError::UnsafePath(path.to_owned()))
        }
        Ok(_) => Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            format!("runtime artifact already exists: {}", path.display()),
        )
        .into()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

pub async fn validate_existing_regular_target(path: impl AsRef<Path>) -> Result<(), AtomicFsError> {
    let path = path.as_ref();
    let metadata = tokio::fs::symlink_metadata(path).await?;
    if metadata.file_type().is_symlink() || !metadata.is_file() || is_reparse_point(&metadata) {
        return Err(AtomicFsError::UnsafePath(path.to_owned()));
    }
    Ok(())
}

pub async fn remove_regular_file(path: impl AsRef<Path>) -> Result<(), AtomicFsError> {
    let path = path.as_ref();
    match tokio::fs::symlink_metadata(path).await {
        Ok(metadata)
            if metadata.file_type().is_symlink()
                || !metadata.is_file()
                || is_reparse_point(&metadata) =>
        {
            Err(AtomicFsError::UnsafePath(path.to_owned()))
        }
        Ok(_) => {
            tokio::fs::remove_file(path).await?;
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

#[cfg(windows)]
pub fn is_reparse_point(metadata: &std::fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    metadata.file_attributes() & 0x400 != 0
}

#[cfg(not(windows))]
pub fn is_reparse_point(_metadata: &std::fs::Metadata) -> bool {
    false
}

#[cfg(unix)]
pub async fn atomic_move_new(
    source: impl AsRef<Path>,
    target: impl AsRef<Path>,
) -> Result<(), AtomicFsError> {
    tokio::fs::hard_link(source.as_ref(), target.as_ref()).await?;
    tokio::fs::remove_file(source.as_ref()).await?;
    Ok(())
}

#[cfg(unix)]
pub async fn atomic_replace(
    source: impl AsRef<Path>,
    target: impl AsRef<Path>,
) -> Result<(), AtomicFsError> {
    tokio::fs::rename(source.as_ref(), target.as_ref()).await?;
    Ok(())
}

#[cfg(windows)]
pub async fn atomic_move_new(
    source: impl AsRef<Path>,
    target: impl AsRef<Path>,
) -> Result<(), AtomicFsError> {
    windows_move_file(source, target, false)
}

#[cfg(windows)]
pub async fn atomic_replace(
    source: impl AsRef<Path>,
    target: impl AsRef<Path>,
) -> Result<(), AtomicFsError> {
    const RETRIES: usize = 20;
    for attempt in 0..RETRIES {
        match windows_replace_file(source.as_ref(), target.as_ref()) {
            Ok(()) => return Ok(()),
            Err(AtomicFsError::Io(error))
                if matches!(error.raw_os_error(), Some(5 | 32 | 33)) && attempt + 1 < RETRIES =>
            {
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            }
            Err(error) => return Err(error),
        }
    }
    unreachable!("bounded retry loop returns on its final attempt")
}

#[cfg(windows)]
fn windows_move_file(
    source: impl AsRef<Path>,
    target: impl AsRef<Path>,
    replace: bool,
) -> Result<(), AtomicFsError> {
    use std::{iter, os::windows::ffi::OsStrExt};
    const MOVEFILE_REPLACE_EXISTING: u32 = 0x1;
    const MOVEFILE_WRITE_THROUGH: u32 = 0x8;
    unsafe extern "system" {
        fn MoveFileExW(existing: *const u16, new: *const u16, flags: u32) -> i32;
    }
    let source: Vec<u16> = source
        .as_ref()
        .as_os_str()
        .encode_wide()
        .chain(iter::once(0))
        .collect();
    let target: Vec<u16> = target
        .as_ref()
        .as_os_str()
        .encode_wide()
        .chain(iter::once(0))
        .collect();
    let flags = MOVEFILE_WRITE_THROUGH
        | if replace {
            MOVEFILE_REPLACE_EXISTING
        } else {
            0
        };
    if unsafe { MoveFileExW(source.as_ptr(), target.as_ptr(), flags) } == 0 {
        Err(std::io::Error::last_os_error().into())
    } else {
        Ok(())
    }
}

#[cfg(windows)]
fn windows_replace_file(
    source: impl AsRef<Path>,
    target: impl AsRef<Path>,
) -> Result<(), AtomicFsError> {
    use std::{iter, os::windows::ffi::OsStrExt};
    unsafe extern "system" {
        fn ReplaceFileW(
            replaced: *const u16,
            replacement: *const u16,
            backup: *const u16,
            flags: u32,
            exclude: *mut core::ffi::c_void,
            reserved: *mut core::ffi::c_void,
        ) -> i32;
    }
    let source: Vec<u16> = source
        .as_ref()
        .as_os_str()
        .encode_wide()
        .chain(iter::once(0))
        .collect();
    let target: Vec<u16> = target
        .as_ref()
        .as_os_str()
        .encode_wide()
        .chain(iter::once(0))
        .collect();
    if unsafe {
        ReplaceFileW(
            target.as_ptr(),
            source.as_ptr(),
            std::ptr::null(),
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    } == 0
    {
        Err(std::io::Error::last_os_error().into())
    } else {
        Ok(())
    }
}

#[cfg(unix)]
pub async fn sync_dir(dir: impl AsRef<Path>) -> std::io::Result<()> {
    let dir = dir.as_ref().to_owned();
    tokio::task::spawn_blocking(move || std::fs::File::open(dir)?.sync_all())
        .await
        .map_err(std::io::Error::other)?
}

#[cfg(windows)]
pub async fn sync_dir(_dir: impl AsRef<Path>) -> std::io::Result<()> {
    Ok(())
}
