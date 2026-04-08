use std::{os::windows::ffi::OsStrExt, path::Path};

use anyhow::Context;
use tokio::{
    fs::{File, OpenOptions},
    io::{AsyncReadExt, AsyncWriteExt},
};
use windows::{
    Win32::{
        Foundation::*,
        Security::{
            Authorization::{
                ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
            },
            DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, SetFileSecurityW,
        },
    },
    core::{Free, PCWSTR},
};

const ACL_FILE_NAME: &str = "acl.list";

fn acl_path() -> std::path::PathBuf {
    crate::utils::dirs::service_config_dir().join(ACL_FILE_NAME)
}

fn set_file_acl_from_sddl(file: &Path, sddl: &str) -> anyhow::Result<()> {
    let file_path_wide: Vec<u16> = file.as_os_str().encode_wide().chain(Some(0)).collect();
    let sddl_wide: Vec<u16> = sddl.encode_utf16().chain(Some(0)).collect();

    let mut security_descriptor_ptr = PSECURITY_DESCRIPTOR::default();
    let mut sd_size = 0;

    unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            PCWSTR::from_raw(sddl_wide.as_ptr()),
            SDDL_REVISION_1,
            &mut security_descriptor_ptr,
            Some(&mut sd_size),
        )
        .context("failed to convert sddl to security descriptor")?;
    }

    unsafe {
        let result = SetFileSecurityW(
            PCWSTR::from_raw(file_path_wide.as_ptr()),
            DACL_SECURITY_INFORMATION,
            security_descriptor_ptr,
        );

        HLOCAL(security_descriptor_ptr.0).free();

        if !result.as_bool() {
            anyhow::bail!("failed to set file acl");
        }
    }

    Ok(())
}

pub async fn create_acl_file() -> Result<(), anyhow::Error> {
    let path = acl_path();
    if path.exists() {
        return Ok(());
    }

    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }

    let sddl =
        chimera_ipc::utils::acl::generate_windows_security_descriptor::<&str>(&[], None, None)
            .context("failed to generate sddl")?;
    File::create(&path)
        .await
        .context("failed to create acl file")?;
    set_file_acl_from_sddl(&path, &sddl)?;
    Ok(())
}

pub async fn read_acl_file() -> Result<Vec<String>, anyhow::Error> {
    let path = acl_path();
    if !path.exists() {
        return Ok(vec![]);
    }

    let mut file = File::open(path).await.context("failed to open acl file")?;
    let mut content = String::with_capacity(4096);
    file.read_to_string(&mut content)
        .await
        .context("failed to read acl file")?;

    Ok(content
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.starts_with("S-") {
                Some(line.to_string())
            } else {
                None
            }
        })
        .collect())
}

pub async fn write_acl_file<T: AsRef<str>>(entries: &[T]) -> Result<(), anyhow::Error> {
    let path = acl_path();
    let mut file = OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(path)
        .await
        .context("failed to open acl file")?;
    file.write_all(
        entries
            .iter()
            .map(|x| x.as_ref())
            .collect::<Vec<_>>()
            .join("\n")
            .as_bytes(),
    )
    .await
    .context("failed to write acl file")?;
    Ok(())
}
