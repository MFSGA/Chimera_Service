use interprocess::local_socket::{GenericFilePath, Name, ToFsName};

pub fn get_current_ts() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}
