use interprocess::local_socket::{GenericFilePath, Name, ToFsName};

#[cfg(windows)]
pub mod acl;
pub mod os;

#[inline]
pub(crate) fn get_name_string(placeholder: &str) -> String {
    if cfg!(windows) {
        format!("\\\\.\\pipe\\{placeholder}")
    } else {
        format!("/var/run/{placeholder}.sock")
    }
}

pub fn get_current_ts() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}
