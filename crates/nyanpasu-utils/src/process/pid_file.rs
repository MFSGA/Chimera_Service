use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

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
