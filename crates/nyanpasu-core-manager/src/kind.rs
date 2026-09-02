//! Core kinds, launch profiles, and one-shot config checking.

use std::{ffi::OsString, time::Duration};

use camino::Utf8Path;
use nyanpasu_utils::process::ProcessError;

use crate::{error::Error, spec::InstanceSpec};

pub use nyanpasu_core_metadata::ClashCoreKind as CoreKind;

pub const MIHOMO_SAFE_PATHS_ENV_NAME: &str = "SAFE_PATHS";
pub(crate) const CLICOLOR_FORCE_ENV_NAME: &str = "CLICOLOR_FORCE";

#[cfg(windows)]
const SAFE_PATHS_SEPARATOR: &str = ";";
#[cfg(not(windows))]
const SAFE_PATHS_SEPARATOR: &str = ":";

pub(crate) fn run_args(
    kind: CoreKind,
    working_dir: &Utf8Path,
    config_path: &Utf8Path,
) -> Vec<OsString> {
    let dir = OsString::from(working_dir.as_str());
    let config = OsString::from(config_path.as_str());
    match kind {
        CoreKind::Mihomo | CoreKind::Meow => {
            vec!["-m".into(), "-d".into(), dir, "-f".into(), config]
        }
        CoreKind::ClashRust => vec!["-d".into(), dir, "-c".into(), config],
        CoreKind::ClashPremium => vec!["-d".into(), dir, "-f".into(), config],
    }
}

/// clash-rs overwrites its local controller from the CLI, so a local endpoint
/// must be passed explicitly. Other core families consume the effective config.
pub(crate) fn controller_args(kind: CoreKind, host: &clash_api::Host) -> Vec<OsString> {
    if !matches!(kind, CoreKind::ClashRust) {
        return Vec::new();
    }
    match host {
        clash_api::Host::NamedPipe(path) | clash_api::Host::UnixSocket(path) => {
            vec!["--controller-ipc".into(), path.as_os_str().to_owned()]
        }
        _ => Vec::new(),
    }
}

pub(crate) fn check_args(working_dir: &Utf8Path, config_path: &Utf8Path) -> Vec<OsString> {
    vec![
        "-t".into(),
        "-d".into(),
        working_dir.as_str().into(),
        "-f".into(),
        config_path.as_str().into(),
    ]
}

pub fn mihomo_safe_paths(working_dir: &Utf8Path, config_dir: &Utf8Path) -> String {
    [working_dir.as_str(), config_dir.as_str()].join(SAFE_PATHS_SEPARATOR)
}

pub const CHECK_CONFIG_TIMEOUT: Duration = Duration::from_secs(30);

pub async fn check_config(spec: &InstanceSpec) -> Result<(), Error> {
    run_check(spec, CHECK_CONFIG_TIMEOUT).await
}

#[cfg(feature = "test-hooks")]
pub async fn check_config_within(spec: &InstanceSpec, timeout: Duration) -> Result<(), Error> {
    run_check(spec, timeout).await
}

async fn run_check(spec: &InstanceSpec, timeout: Duration) -> Result<(), Error> {
    let config_dir = spec
        .config_path
        .parent()
        .ok_or_else(|| Error::ConfigNotFound(spec.config_path.clone()))?;
    let output = nyanpasu_utils::process::Command::new(spec.core.binary_path.as_str())
        .args(check_args(&spec.working_dir, &spec.config_path))
        .env(
            MIHOMO_SAFE_PATHS_ENV_NAME,
            mihomo_safe_paths(&spec.working_dir, config_dir),
        )
        .env(CLICOLOR_FORCE_ENV_NAME, "0")
        .timeout(timeout)
        .output()
        .await
        .map_err(|error| match error {
            ProcessError::Timeout { after } => {
                Error::ConfigCheckFailed(format!("config check timed out after {after:?}"))
            }
            other => Error::Process(other),
        })?;
    if output.success() {
        return Ok(());
    }
    Err(Error::ConfigCheckFailed(summarize_output(
        &output.stdout,
        &output.stderr,
    )))
}

fn summarize_output(stdout: &str, stderr: &str) -> String {
    let stderr = stderr.trim();
    if !stderr.is_empty() {
        return stderr.to_owned();
    }
    let stdout = stdout.trim();
    if stdout.is_empty() {
        "core rejected the config without diagnostics".to_owned()
    } else {
        stdout.lines().last().unwrap_or(stdout).to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use camino::Utf8PathBuf;

    #[test]
    fn run_args_match_each_core_profile() {
        let dir = Utf8PathBuf::from("C:/data");
        let config = Utf8PathBuf::from("C:/data/config.yaml");
        assert_eq!(
            run_args(CoreKind::Mihomo, &dir, &config),
            ["-m", "-d", "C:/data", "-f", "C:/data/config.yaml"].map(OsString::from)
        );
        assert_eq!(
            run_args(CoreKind::ClashRust, &dir, &config),
            ["-d", "C:/data", "-c", "C:/data/config.yaml"].map(OsString::from)
        );
        assert_eq!(
            run_args(CoreKind::ClashPremium, &dir, &config),
            ["-d", "C:/data", "-f", "C:/data/config.yaml"].map(OsString::from)
        );
        assert_eq!(
            run_args(CoreKind::Meow, &dir, &config),
            run_args(CoreKind::Mihomo, &dir, &config)
        );
    }

    #[test]
    fn clash_rs_receives_local_controller_cli_args() {
        let host = clash_api::Host::unix_socket("/tmp/core.sock");
        assert_eq!(
            controller_args(CoreKind::ClashRust, &host),
            ["--controller-ipc", "/tmp/core.sock"].map(OsString::from)
        );
        assert!(controller_args(CoreKind::Mihomo, &host).is_empty());
    }

    #[test]
    fn safe_paths_use_the_platform_separator() {
        let joined = mihomo_safe_paths(Utf8Path::new("/a"), Utf8Path::new("/b"));
        #[cfg(windows)]
        assert_eq!(joined, "/a;/b");
        #[cfg(not(windows))]
        assert_eq!(joined, "/a:/b");
    }

    #[test]
    fn diagnostics_prefer_stderr_then_last_stdout_line() {
        assert_eq!(summarize_output("first\nlast", ""), "last");
        assert_eq!(summarize_output("ignored", "fatal"), "fatal");
    }
}
