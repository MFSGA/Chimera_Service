//! Immutable launch specifications and manager options.

use std::time::Duration;

use camino::Utf8PathBuf;
use nyanpasu_utils::process::{Backoff, RestartPolicy};
use tokio_util::sync::CancellationToken;

use crate::{HealthPolicy, kind::CoreKind};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoreSpec {
    pub kind: CoreKind,
    /// Resolved by the caller; the service keeps binary discovery policy.
    pub binary_path: Utf8PathBuf,
    /// Authoritative capability version. The manager probes `-v` when absent.
    pub version: Option<String>,
    pub features: Vec<String>,
}

/// Immutable per-epoch launch spec. Changing the config means a new epoch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstanceSpec {
    pub core: CoreSpec,
    pub config_path: Utf8PathBuf,
    pub working_dir: Utf8PathBuf,
    pub pid_file: Option<Utf8PathBuf>,
    pub options: InstanceOptions,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstanceOptions {
    pub startup_timeout: Duration,
    pub health: HealthPolicy,
    pub restart_policy: RestartPolicy,
    pub backoff: Backoff,
}

impl Default for InstanceOptions {
    fn default() -> Self {
        Self {
            startup_timeout: Duration::from_secs(30),
            health: HealthPolicy::default(),
            restart_policy: RestartPolicy::OnFailure { max_restarts: 5 },
            backoff: Backoff::exponential(Duration::from_secs(1), Duration::from_secs(30))
                .with_jitter(),
        }
    }
}

/// The probe/control endpoint an instance actually uses.
#[derive(Debug, Clone)]
pub struct ResolvedController {
    pub host: clash_api::Host,
    pub secret: Option<String>,
}

/// How the manager selects the core's primary controller transport.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocalIpcPolicy {
    /// Require local IPC and fail when the core cannot support it.
    Force,
    /// Prefer local IPC and fall back to the source HTTP controller.
    Prefer,
    /// Never rewrite the source HTTP controller.
    Disable,
}

#[derive(Debug, Clone)]
pub struct ManagerOptions {
    /// Manager-owned runtime artifact directory. Required by the full manager.
    pub runtime_dir: Option<Utf8PathBuf>,
    pub local_ipc_policy: LocalIpcPolicy,
    /// Endpoint template containing `{epoch}`; platform default when `None`.
    pub controller_template: Option<String>,
    pub control_timeout: Duration,
    pub reconcile_timeout: Duration,
    pub stop_timeout: Duration,
    pub cancel_token: CancellationToken,
    /// Write structured core logs under `{runtime_dir}/logs/`.
    pub log_sink_enabled: bool,
    /// Soft rotation threshold for one JSONL file.
    pub log_max_bytes: u64,
    /// Number of JSONL files retained, including the active file.
    pub log_max_files: usize,
}

impl Default for ManagerOptions {
    fn default() -> Self {
        Self {
            runtime_dir: None,
            local_ipc_policy: LocalIpcPolicy::Disable,
            controller_template: None,
            control_timeout: Duration::from_secs(10),
            reconcile_timeout: Duration::from_secs(30),
            stop_timeout: Duration::from_secs(10),
            cancel_token: CancellationToken::new(),
            log_sink_enabled: true,
            log_max_bytes: 4 * 1024 * 1024,
            log_max_files: 5,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn instance_options_defaults_match_spec() {
        let options = InstanceOptions::default();
        assert_eq!(options.startup_timeout, Duration::from_secs(30));
        assert_eq!(options.health.interval(), Duration::from_millis(250));
        assert_eq!(options.health.timeout(), Duration::from_secs(1));
        assert_eq!(options.health.failure_threshold().get(), 3);
        assert_eq!(options.health.success_threshold().get(), 1);
        assert_eq!(options.health.start_period(), Duration::ZERO);
        assert_eq!(
            options.restart_policy,
            RestartPolicy::OnFailure { max_restarts: 5 }
        );
    }

    #[test]
    fn manager_defaults_keep_the_source_http_controller() {
        let options = ManagerOptions::default();
        assert_eq!(options.local_ipc_policy, LocalIpcPolicy::Disable);
        assert!(options.controller_template.is_none());
        assert!(options.runtime_dir.is_none());
        assert_eq!(options.control_timeout, Duration::from_secs(10));
        assert_eq!(options.reconcile_timeout, Duration::from_secs(30));
        assert_eq!(options.stop_timeout, Duration::from_secs(10));
        assert!(options.log_sink_enabled);
        assert_eq!(options.log_max_bytes, 4 * 1024 * 1024);
        assert_eq!(options.log_max_files, 5);
    }
}
