use std::{iter, os::windows::ffi::OsStrExt, path::Path};

use windows::{
    Win32::{
        Foundation::{HLOCAL, LocalFree},
        Security::{
            Authorization::{
                ConvertSecurityDescriptorToStringSecurityDescriptorW,
                ConvertStringSecurityDescriptorToSecurityDescriptorW,
            },
            DACL_SECURITY_INFORMATION, GetFileSecurityW, GetSecurityDescriptorControl,
            PSECURITY_DESCRIPTOR, SE_DACL_PROTECTED, SetFileSecurityW,
        },
    },
    core::{PCWSTR, PWSTR},
};

use super::AtomicFsError;

const SDDL_REVISION_1: u32 = 1;

pub fn harden_windows_directory_acl(path: impl AsRef<Path>) -> Result<(), AtomicFsError> {
    let path = path.as_ref();
    let sddl: Vec<u16> = "D:P(A;OICI;FA;;;OW)(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)"
        .encode_utf16()
        .chain(iter::once(0))
        .collect();
    let path_wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(iter::once(0))
        .collect();
    let mut descriptor = PSECURITY_DESCRIPTOR::default();
    unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            PCWSTR(sddl.as_ptr()),
            SDDL_REVISION_1,
            &mut descriptor,
            None,
        )
    }
    .map_err(windows_io_error)?;
    let result = unsafe {
        SetFileSecurityW(
            PCWSTR(path_wide.as_ptr()),
            DACL_SECURITY_INFORMATION,
            descriptor,
        )
    };
    unsafe { LocalFree(Some(HLOCAL(descriptor.0))) };
    if result.0 == 0 {
        Err(std::io::Error::last_os_error().into())
    } else {
        Ok(())
    }
}

pub fn verify_windows_directory_acl(path: impl AsRef<Path>) -> Result<(), AtomicFsError> {
    let path = path.as_ref();
    let path_wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(iter::once(0))
        .collect();
    let mut needed = 0_u32;
    #[allow(unused_must_use)]
    unsafe {
        let _ = GetFileSecurityW(
            PCWSTR(path_wide.as_ptr()),
            DACL_SECURITY_INFORMATION.0,
            None,
            0,
            &mut needed,
        );
    }
    if needed == 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let words = (needed as usize).div_ceil(std::mem::size_of::<usize>());
    let mut descriptor = vec![0_usize; words];
    let descriptor_ptr = PSECURITY_DESCRIPTOR(descriptor.as_mut_ptr().cast());
    if unsafe {
        GetFileSecurityW(
            PCWSTR(path_wide.as_ptr()),
            DACL_SECURITY_INFORMATION.0,
            Some(descriptor_ptr),
            needed,
            &mut needed,
        )
    }
    .0 == 0
    {
        return Err(std::io::Error::last_os_error().into());
    }

    let mut control = 0_u16;
    let mut revision = 0_u32;
    unsafe { GetSecurityDescriptorControl(descriptor_ptr, &mut control, &mut revision) }
        .map_err(windows_io_error)?;
    if control & SE_DACL_PROTECTED.0 == 0 {
        return Err(AtomicFsError::UnsafePath(path.to_owned()));
    }

    let mut sddl = PWSTR(std::ptr::null_mut());
    let mut sddl_len = 0_u32;
    unsafe {
        ConvertSecurityDescriptorToStringSecurityDescriptorW(
            descriptor_ptr,
            SDDL_REVISION_1,
            DACL_SECURITY_INFORMATION,
            &mut sddl,
            Some(&mut sddl_len),
        )
    }
    .map_err(windows_io_error)?;
    let text =
        String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(sddl.0, sddl_len as usize) });
    unsafe { LocalFree(Some(HLOCAL(sddl.0.cast()))) };
    let broad_principals = [";;;WD)", ";;;AU)", ";;;BU)", ";;;IU)", ";;;AN)", ";;;NU)"];
    if broad_principals
        .iter()
        .any(|principal| text.contains(principal))
    {
        return Err(AtomicFsError::UnsafePath(path.to_owned()));
    }
    Ok(())
}

fn windows_io_error(error: windows::core::Error) -> std::io::Error {
    let code = error.code().0 as u32;
    if code & 0xffff_0000 == 0x8007_0000 {
        std::io::Error::from_raw_os_error((code & 0xffff) as i32)
    } else {
        std::io::Error::other(error)
    }
}
