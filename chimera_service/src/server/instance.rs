use std::{
    borrow::Cow,
    collections::{BTreeSet, HashMap, VecDeque},
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicI64, AtomicU64, Ordering},
    },
};

use camino::{Utf8Path, Utf8PathBuf};
use chimera_ipc::{
    api::{
        CoreErrorKind,
        core::v2::{
            CoreApiConnection, CoreCommandInfo, CoreControllerInfo, CoreOperationReq,
            CoreSubmitReq, OperationInfo, OperationOutputInfo, ReconcileOutcomeInfo,
            ReconcileOutcomeKind, payload_digest,
        },
        status::{ConfigRevisionInfo, CoreHealthInfo, CoreHealthState, CoreState},
    },
    utils::get_current_ts,
};
use chimera_utils::core::{CoreType, instance::CoreInstance};
use nyanpasu_utils::process::{OrphanReapOutcome, ProcessEvent, reap_epoch_pid_file};
use tokio::sync::{Mutex, mpsc::Sender as MpscSender, watch};
use tokio_util::{sync::CancellationToken, task::task_tracker::TaskTracker};
use tracing::instrument;

use super::{
    consts,
    runtime_process::{ServiceProcessState, ServiceRuntimeProcess, epoch_pid_path},
    runtime_store::{RuntimeConfigBackup, RuntimeConfigStore, StagedRuntimeConfig},
};

const OPERATION_HISTORY_LIMIT: usize = 64;
const OPERATION_WAIT_LIMIT: std::time::Duration = std::time::Duration::from_secs(60);
const RECONCILE_PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(1);
const STARTUP_READINESS_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
const STARTUP_READINESS_INTERVAL: std::time::Duration = std::time::Duration::from_millis(250);
const HEALTH_FAILURE_THRESHOLD: u32 = 3;
const MAX_HEALTH_ERROR_BYTES: usize = 512;

#[derive(Debug, thiserror::Error)]
#[error("{message}")]
struct ClassifiedOperationError {
    kind: CoreErrorKind,
    message: String,
    retryable: bool,
}

impl ClassifiedOperationError {
    fn new(kind: CoreErrorKind, message: impl Into<String>, retryable: bool) -> Self {
        Self {
            kind,
            message: message.into(),
            retryable,
        }
    }

    fn domain(kind: CoreErrorKind, message: impl Into<String>) -> Self {
        Self::new(kind, message, kind.default_retryable())
    }
}

#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub(crate) struct OpError {
    kind: Option<CoreErrorKind>,
    message: String,
    retryable: Option<bool>,
}

impl OpError {
    fn plain(message: impl Into<String>) -> Self {
        Self {
            kind: None,
            message: message.into(),
            retryable: None,
        }
    }

    fn with_kind(kind: CoreErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind: Some(kind),
            message: message.into(),
            retryable: None,
        }
    }

    fn retryable(mut self, retryable: bool) -> Self {
        self.retryable = Some(retryable);
        self
    }

    #[cfg(test)]
    pub(crate) fn kind(&self) -> Option<CoreErrorKind> {
        self.kind
    }

    #[cfg(test)]
    pub(crate) fn retryable_hint(&self) -> Option<bool> {
        self.retryable
    }

    pub(crate) fn into_envelope<T>(self) -> chimera_ipc::api::R<'static, T>
    where
        T: serde::Serialize + serde::de::DeserializeOwned + std::fmt::Debug,
    {
        chimera_ipc::api::RBuilder::other_error_with_kind(
            Cow::Owned(self.message),
            self.kind,
            self.retryable,
        )
    }
}

impl From<anyhow::Error> for OpError {
    fn from(error: anyhow::Error) -> Self {
        if let Some(classified) = error
            .chain()
            .find_map(|cause| cause.downcast_ref::<ClassifiedOperationError>())
        {
            return Self::with_kind(classified.kind, error.to_string())
                .retryable(classified.retryable);
        }
        Self::plain(error.to_string())
    }
}

fn classified_operation_error(kind: CoreErrorKind, message: impl Into<String>) -> anyhow::Error {
    anyhow::Error::new(ClassifiedOperationError::domain(kind, message))
}

fn merge_warnings(left: Option<String>, right: Option<String>) -> Option<String> {
    match (left, right) {
        (Some(left), Some(right)) => Some(format!("{left}; {right}")),
        (Some(warning), None) | (None, Some(warning)) => Some(warning),
        (None, None) => None,
    }
}

fn cap_health_error(detail: &str) -> String {
    if detail.len() <= MAX_HEALTH_ERROR_BYTES {
        return detail.to_owned();
    }
    let mut end = MAX_HEALTH_ERROR_BYTES;
    while !detail.is_char_boundary(end) {
        end -= 1;
    }
    detail[..end].to_owned()
}

fn healthy_health(previous: Option<&CoreHealthInfo>) -> CoreHealthInfo {
    let now = get_current_ts();
    CoreHealthInfo {
        state: CoreHealthState::Healthy,
        changed_at: previous
            .filter(|health| health.state == CoreHealthState::Healthy)
            .map_or(now, |health| health.changed_at),
        consecutive_failures: 0,
        last_error: None,
        last_success_at: Some(now),
    }
}

fn unhealthy_observation(previous: Option<&CoreHealthInfo>, detail: &str) -> CoreHealthInfo {
    let now = get_current_ts();
    let failures = previous.map_or(1, |health| health.consecutive_failures.saturating_add(1));
    let state = if failures >= HEALTH_FAILURE_THRESHOLD {
        CoreHealthState::Unhealthy
    } else {
        previous.map_or(CoreHealthState::Starting, |health| health.state)
    };
    CoreHealthInfo {
        state,
        changed_at: previous
            .filter(|health| health.state == state)
            .map_or(now, |health| health.changed_at),
        consecutive_failures: failures,
        last_error: Some(cap_health_error(detail)),
        last_success_at: previous.and_then(|health| health.last_success_at),
    }
}

fn record_health_observation(
    health: &parking_lot::Mutex<Option<CoreHealthInfo>>,
    error: Option<&str>,
) {
    let mut current = health.lock();
    let next = match error {
        None => healthy_health(current.as_ref()),
        Some(error) => unhealthy_observation(current.as_ref(), error),
    };
    *current = Some(next);
}

fn epoch_from_runtime_path(path: &Utf8Path) -> Option<u64> {
    let file = path.file_name()?;
    let epoch = file.strip_prefix("config-")?.strip_suffix(".yaml")?;
    epoch.parse().ok()
}

fn with_durability_warning(message: impl Into<String>, warning: Option<&str>) -> String {
    let message = message.into();
    match warning {
        Some(warning) => format!("{message}; runtime durability warning: {warning}"),
        None => message,
    }
}

fn yaml_scalar(value: &str) -> Option<String> {
    let value = value.trim();
    if value.is_empty() || matches!(value, "null" | "~") {
        return None;
    }
    if value.starts_with('"') && value.ends_with('"') {
        return serde_json::from_str::<String>(value).ok();
    }
    if value.starts_with('\'') && value.ends_with('\'') && value.len() >= 2 {
        return Some(value[1..value.len() - 1].replace("''", "'"));
    }
    let value = value.split(" #").next().unwrap_or(value).trim();
    (!value.is_empty()).then(|| value.to_string())
}

fn top_level_yaml_scalar(config: &str, key: &str) -> Option<String> {
    config.lines().find_map(|line| {
        if line
            .chars()
            .next()
            .is_some_and(|character| character.is_whitespace())
        {
            return None;
        }
        let (name, value) = line.split_once(':')?;
        (name.trim() == key).then(|| yaml_scalar(value)).flatten()
    })
}

fn api_connection_from_config(
    config: &str,
    revision: &ConfigRevisionInfo,
) -> Option<CoreApiConnection> {
    let controller = if let Some(controller) = top_level_yaml_scalar(config, "external-controller")
    {
        let url = if controller.contains("://") {
            controller
        } else {
            format!("http://{controller}")
        };
        CoreControllerInfo::Http(url)
    } else if let Some(path) = top_level_yaml_scalar(config, "external-controller-unix") {
        CoreControllerInfo::UnixSocket(path)
    } else if let Some(path) = top_level_yaml_scalar(config, "external-controller-pipe") {
        CoreControllerInfo::NamedPipe(path)
    } else {
        return None;
    };

    Some(CoreApiConnection {
        instance_id: format!("{:016x}{:016x}", revision.epoch, revision.generation),
        controller,
        secret: top_level_yaml_scalar(config, "secret"),
    })
}

#[derive(Debug)]
struct OperationRecord {
    fingerprint: String,
    receiver: watch::Receiver<OperationInfo>,
}

#[derive(Debug, Default)]
struct OperationRegistryState {
    records: HashMap<String, OperationRecord>,
    order: VecDeque<String>,
}

enum OperationAdmission {
    Existing(OperationInfo),
    Registered {
        info: OperationInfo,
        sender: watch::Sender<OperationInfo>,
    },
}

struct CoreManager {
    instance: Arc<ServiceRuntimeProcess>,
    cancel_token: CancellationToken,
    config_path: Utf8PathBuf,
    tracker: Option<TaskTracker>,
    health: Arc<parking_lot::Mutex<Option<CoreHealthInfo>>>,
    probe_lock: Arc<Mutex<()>>,
}

#[derive(Debug, Clone)]
struct QuarantinedEpoch {
    epoch: u64,
    reason: String,
    death_proven: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReconcilePlan {
    Noop,
    Patch,
    Reload,
    Restart,
    Switch,
}

const PATCH_FIELDS: &[&str] = &[
    "port",
    "socks-port",
    "redir-port",
    "tproxy-port",
    "mixed-port",
    "tun",
    "tuic-server",
    "ss-config",
    "vmess-config",
    "tcptun-config",
    "udptun-config",
    "allow-lan",
    "skip-auth-prefixes",
    "lan-allowed-ips",
    "lan-disallowed-ips",
    "bind-address",
    "mode",
    "log-level",
    "ipv6",
    "sniffing",
    "tcp-concurrent",
    "find-process-mode",
    "interface-name",
];

const TUN_PATCH_FIELDS: &[&str] = &[
    "enable",
    "device",
    "stack",
    "dns-hijack",
    "auto-route",
    "auto-detect-interface",
    "mtu",
    "gso",
    "gso-max-size",
    "inet6-address",
    "iproute2-table-index",
    "iproute2-rule-index",
    "auto-redirect",
    "auto-redirect-input-mark",
    "auto-redirect-output-mark",
    "auto-redirect-iproute2-fallback-rule-index",
    "loopback-address",
    "strict-route",
    "route-address",
    "route-address-set",
    "route-exclude-address",
    "route-exclude-address-set",
    "include-interface",
    "exclude-interface",
    "include-uid",
    "include-uid-range",
    "exclude-uid",
    "exclude-uid-range",
    "include-android-user",
    "include-package",
    "exclude-package",
    "include-mac-address",
    "exclude-mac-address",
    "endpoint-independent-nat",
    "udp-timeout",
    "icmp-timeout",
    "file-descriptor",
    "inet4-route-address",
    "inet6-route-address",
    "inet4-route-exclude-address",
    "inet6-route-exclude-address",
    "recvmsgx",
    "sendmsgx",
];

const TUIC_SERVER_PATCH_FIELDS: &[&str] = &[
    "enable",
    "listen",
    "token",
    "users",
    "certificate",
    "private-key",
    "congestion-controller",
    "max-idle-time",
    "authentication-timeout",
    "alpn",
    "max-udp-relay-packet-size",
    "cwnd",
    "bbr-profile",
];

const RELOAD_FIELDS: &[&str] = &[
    "proxies",
    "proxy-groups",
    "proxy-providers",
    "rule-providers",
    "providers",
    "rules",
    "hosts",
    "dns",
];

fn controller_client(connection: &CoreApiConnection) -> anyhow::Result<clash_api::Client> {
    let host = match &connection.controller {
        CoreControllerInfo::Http(url) => clash_api::Host::url(url)?,
        CoreControllerInfo::UnixSocket(path) => clash_api::Host::unix_socket(path),
        CoreControllerInfo::NamedPipe(path) => clash_api::Host::named_pipe(path),
    };
    Ok(clash_api::Client::with_secret(
        host,
        connection.secret.clone().unwrap_or_default(),
    ))
}

fn mapping_keys_are_strings(mapping: &serde_yaml::Mapping) -> bool {
    mapping.keys().all(|key| key.as_str().is_some())
}

fn changed_mapping_keys<'a>(
    current: &'a serde_yaml::Mapping,
    desired: &'a serde_yaml::Mapping,
) -> Vec<&'a str> {
    current
        .keys()
        .chain(desired.keys())
        .filter_map(serde_yaml::Value::as_str)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .filter(|root| {
            let key = serde_yaml::Value::String((*root).to_owned());
            current.get(&key) != desired.get(&key)
        })
        .collect()
}

fn filtered_nested_patch(
    current: &serde_yaml::Mapping,
    desired: &serde_yaml::Mapping,
    allowed: &[&str],
) -> Option<serde_yaml::Value> {
    if !mapping_keys_are_strings(current) || !mapping_keys_are_strings(desired) {
        return None;
    }
    let nested = changed_mapping_keys(current, desired);
    if nested.is_empty() || !nested.iter().all(|field| allowed.contains(field)) {
        return None;
    }
    let enable = serde_yaml::Value::String("enable".into());
    desired.get(&enable)?.as_bool()?;

    let mut filtered = serde_yaml::Mapping::new();
    for field in allowed {
        let field_key = serde_yaml::Value::String((*field).to_owned());
        if let Some(value) = desired.get(&field_key) {
            filtered.insert(field_key, value.clone());
        }
    }
    Some(serde_yaml::Value::Mapping(filtered))
}

fn build_patch(
    current: &serde_yaml::Mapping,
    desired: &serde_yaml::Mapping,
) -> Option<(clash_api::ConfigPatch, clash_api::RuntimeProjection)> {
    if !mapping_keys_are_strings(current) || !mapping_keys_are_strings(desired) {
        return None;
    }
    let changed = changed_mapping_keys(current, desired);
    if changed.is_empty() || !changed.iter().all(|root| PATCH_FIELDS.contains(root)) {
        return None;
    }

    let mut patch = serde_yaml::Mapping::new();
    for root in changed {
        let key = serde_yaml::Value::String(root.to_owned());
        let desired_value = desired.get(&key)?.clone();
        let desired_value = match root {
            "tun" => filtered_nested_patch(
                current.get(&key)?.as_mapping()?,
                desired_value.as_mapping()?,
                TUN_PATCH_FIELDS,
            )?,
            "tuic-server" => filtered_nested_patch(
                current.get(&key)?.as_mapping()?,
                desired_value.as_mapping()?,
                TUIC_SERVER_PATCH_FIELDS,
            )?,
            _ => desired_value,
        };
        patch.insert(key, desired_value);
    }

    let patch =
        serde_yaml::from_value::<clash_api::ConfigPatch>(serde_yaml::Value::Mapping(patch)).ok()?;
    let projection = clash_api::RuntimeProjection::from_patch(&patch).ok()?;
    Some((patch, projection))
}

fn classify_config_documents(current: &str, desired: &str) -> ReconcilePlan {
    let Ok(serde_yaml::Value::Mapping(current)) = serde_yaml::from_str(current) else {
        return ReconcilePlan::Restart;
    };
    let Ok(serde_yaml::Value::Mapping(desired)) = serde_yaml::from_str(desired) else {
        return ReconcilePlan::Restart;
    };
    if current == desired {
        return ReconcilePlan::Noop;
    }
    if !mapping_keys_are_strings(&current) || !mapping_keys_are_strings(&desired) {
        return ReconcilePlan::Switch;
    }
    if build_patch(&current, &desired).is_some() {
        return ReconcilePlan::Patch;
    }

    let changed = changed_mapping_keys(&current, &desired);
    if !changed.iter().all(|root| RELOAD_FIELDS.contains(root)) {
        return ReconcilePlan::Switch;
    }

    if changed.contains(&"dns") {
        let key = serde_yaml::Value::String("dns".into());
        let Some(current_dns) = current.get(&key).and_then(serde_yaml::Value::as_mapping) else {
            return ReconcilePlan::Switch;
        };
        let Some(desired_dns) = desired.get(&key).and_then(serde_yaml::Value::as_mapping) else {
            return ReconcilePlan::Switch;
        };
        let listen = serde_yaml::Value::String("listen".into());
        if current_dns.get(&listen) != desired_dns.get(&listen) {
            return ReconcilePlan::Switch;
        }
    }
    ReconcilePlan::Reload
}

fn classify_reconcile(
    current_core: Option<&CoreType>,
    current_revision: Option<&ConfigRevisionInfo>,
    current_config: Option<&str>,
    desired_core: &CoreType,
    desired_digest: &str,
    desired_config: &str,
) -> ReconcilePlan {
    match current_core {
        Some(current_core) if current_core != desired_core => ReconcilePlan::Switch,
        Some(_)
            if current_revision.is_some_and(|revision| revision.source_hash == desired_digest) =>
        {
            ReconcilePlan::Noop
        }
        Some(_) => current_config
            .map(|current| classify_config_documents(current, desired_config))
            .unwrap_or(ReconcilePlan::Switch),
        None => ReconcilePlan::Restart,
    }
}

#[derive(Clone)]
pub struct CoreManagerService {
    manager: Arc<Mutex<Option<CoreManager>>>,
    state_changed_at: Arc<AtomicI64>,
    state_changed_notify: Arc<Option<MpscSender<CoreState>>>,
    cancel_token: CancellationToken,
    operations: Arc<parking_lot::Mutex<OperationRegistryState>>,
    operation_lock: Arc<Mutex<()>>,
    applied_revision: Arc<parking_lot::Mutex<Option<ConfigRevisionInfo>>>,
    api_connection: Arc<parking_lot::Mutex<Option<CoreApiConnection>>>,
    quarantine: Arc<parking_lot::Mutex<Vec<QuarantinedEpoch>>>,
    next_epoch: Arc<AtomicU64>,
}

impl CoreManagerService {
    pub fn new_with_notify(notify: MpscSender<CoreState>, cancel_token: CancellationToken) -> Self {
        Self {
            manager: Arc::new(Mutex::new(None)),
            state_changed_at: Arc::new(AtomicI64::new(0)),
            state_changed_notify: Arc::new(Some(notify)),
            cancel_token,
            operations: Arc::new(parking_lot::Mutex::new(OperationRegistryState::default())),
            operation_lock: Arc::new(Mutex::new(())),
            applied_revision: Arc::new(parking_lot::Mutex::new(None)),
            api_connection: Arc::new(parking_lot::Mutex::new(None)),
            quarantine: Arc::new(parking_lot::Mutex::new(Vec::new())),
            next_epoch: Arc::new(AtomicU64::new(1)),
        }
    }

    fn validate_operation_id(id: &str) -> Result<(), OpError> {
        if id.len() == 32
            && id
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            Ok(())
        } else {
            Err(OpError::plain(
                "operation id must be exactly 32 lowercase hexadecimal characters",
            ))
        }
    }

    #[cfg(test)]
    fn operation_snapshot(&self, id: &str) -> Option<OperationInfo> {
        self.operations
            .lock()
            .records
            .get(id)
            .map(|record| record.receiver.borrow().clone())
    }

    fn admit_operation(
        &self,
        id: &str,
        fingerprint: String,
    ) -> Result<OperationAdmission, OpError> {
        let mut operations = self.operations.lock();
        if let Some(existing) = operations.records.get(id) {
            if existing.fingerprint != fingerprint {
                return Err(OpError::with_kind(
                    CoreErrorKind::OperationConflict,
                    "operation conflict: id already exists with a different command",
                )
                .retryable(false));
            }
            return Ok(OperationAdmission::Existing(
                existing.receiver.borrow().clone(),
            ));
        }

        let id = id.to_owned();
        let info = OperationInfo::queued(id.clone());
        let (sender, receiver) = watch::channel(info.clone());
        operations.records.insert(
            id.clone(),
            OperationRecord {
                fingerprint,
                receiver,
            },
        );
        operations.order.push_back(id);
        while operations.records.len() > OPERATION_HISTORY_LIMIT {
            let Some(position) = operations.order.iter().position(|candidate| {
                operations
                    .records
                    .get(candidate)
                    .is_some_and(|record| record.receiver.borrow().is_terminal())
            }) else {
                break;
            };
            if let Some(evicted) = operations.order.remove(position) {
                operations.records.remove(&evicted);
            }
        }
        Ok(OperationAdmission::Registered { info, sender })
    }

    fn quarantine_reason(&self) -> Option<String> {
        let quarantine = self.quarantine.lock();
        let first = quarantine.first()?;
        if quarantine.len() == 1 {
            return Some(format!("epoch {}: {}", first.epoch, first.reason));
        }
        let additional = quarantine
            .iter()
            .skip(1)
            .map(|entry| entry.epoch.to_string())
            .collect::<Vec<_>>()
            .join(", ");
        Some(format!(
            "epoch {}: {}; additional uncertain epochs: {additional}",
            first.epoch, first.reason
        ))
    }

    fn ensure_not_quarantined(&self) -> anyhow::Result<()> {
        if let Some(reason) = self.quarantine_reason() {
            return Err(classified_operation_error(
                CoreErrorKind::Quarantined,
                format!("core manager is quarantined after an unsafe apply failure: {reason}"),
            ));
        }
        Ok(())
    }

    fn latch_quarantine(&self, epoch: u64, reason: impl Into<String>) {
        let reason = reason.into();
        let mut quarantine = self.quarantine.lock();
        if let Some(existing) = quarantine.iter_mut().find(|entry| entry.epoch == epoch) {
            existing.reason = reason.clone();
            existing.death_proven = false;
        } else {
            quarantine.push(QuarantinedEpoch {
                epoch,
                reason: reason.clone(),
                death_proven: false,
            });
        }
        drop(quarantine);
        self.state_changed_at
            .store(get_current_ts(), Ordering::Relaxed);
        Self::notify_state_changed(
            self.state_changed_notify.clone(),
            CoreState::Stopped(Some(format!("epoch {epoch}: {reason}"))),
        );
    }

    async fn recover_quarantine(&self) -> anyhow::Result<()> {
        if self.quarantine.lock().is_empty() {
            return Ok(());
        }
        let store = RuntimeConfigStore::open(crate::utils::dirs::service_config_dir()).await?;
        self.recover_quarantine_with_store(&store).await
    }

    async fn recover_quarantine_with_store(
        &self,
        store: &RuntimeConfigStore,
    ) -> anyhow::Result<()> {
        let quarantined = self.quarantine.lock().clone();
        if quarantined.is_empty() {
            return Ok(());
        }
        let mut failures = Vec::new();

        for entry in quarantined {
            if !entry.death_proven {
                let pid_path = epoch_pid_path(store.dir(), entry.epoch);
                match reap_epoch_pid_file(pid_path.as_std_path(), store.dir().as_std_path()).await {
                    Ok(OrphanReapOutcome::AlreadyExited | OrphanReapOutcome::Killed) => {
                        if let Some(current) = self
                            .quarantine
                            .lock()
                            .iter_mut()
                            .find(|current| current.epoch == entry.epoch)
                        {
                            current.death_proven = true;
                        }
                    }
                    Ok(OrphanReapOutcome::NotFound) => {
                        failures.push(format!(
                            "epoch {}: {}; authoritative epoch pid record is unavailable",
                            entry.epoch, entry.reason
                        ));
                        continue;
                    }
                    Err(error) => {
                        failures.push(format!(
                            "epoch {}: {}; recovery failed: {error}",
                            entry.epoch, entry.reason
                        ));
                        continue;
                    }
                }
            }

            match store.cleanup_epoch(entry.epoch).await {
                Ok(()) => {
                    let tracker = {
                        let mut manager = self.manager.lock().await;
                        let matches_epoch = manager
                            .as_ref()
                            .and_then(|manager| epoch_from_runtime_path(&manager.config_path))
                            == Some(entry.epoch);
                        if matches_epoch {
                            manager
                                .take()
                                .and_then(|mut manager| manager.tracker.take())
                        } else {
                            None
                        }
                    };
                    if let Some(tracker) = tracker {
                        tracker.wait().await;
                    }
                    self.quarantine
                        .lock()
                        .retain(|current| current.epoch != entry.epoch);
                    if self
                        .applied_revision
                        .lock()
                        .as_ref()
                        .is_some_and(|revision| revision.epoch == entry.epoch)
                    {
                        *self.applied_revision.lock() = None;
                        *self.api_connection.lock() = None;
                    }
                }
                Err(error) => failures.push(format!(
                    "epoch {}: {}; artifact cleanup failed: {error}",
                    entry.epoch, entry.reason
                )),
            }
        }

        if !failures.is_empty() {
            return Err(classified_operation_error(
                CoreErrorKind::Quarantined,
                format!(
                    "core manager remains quarantined after recovery: {}",
                    failures.join(" | ")
                ),
            ));
        }
        self.state_changed_at
            .store(get_current_ts(), Ordering::Relaxed);
        Self::notify_state_changed(self.state_changed_notify.clone(), CoreState::Stopped(None));
        Ok(())
    }

    fn launch_paths(core_type: &CoreType) -> anyhow::Result<(Utf8PathBuf, Utf8PathBuf)> {
        let infos = consts::RuntimeInfos::global();
        let app_dir = Utf8PathBuf::from_path_buf(infos.nyanpasu_data_dir.clone())
            .map_err(|_| anyhow::anyhow!("failed to convert app_dir to Utf8PathBuf"))?;
        let binary_path = find_binary_path(core_type).map_err(|error| {
            classified_operation_error(CoreErrorKind::BinaryNotFound, error.to_string())
        })?;
        let binary_path = Utf8PathBuf::from_path_buf(binary_path)
            .map_err(|_| anyhow::anyhow!("failed to convert binary_path to Utf8PathBuf"))?;
        Ok((app_dir, binary_path))
    }

    async fn sync_epoch_allocator(&self, store: &RuntimeConfigStore) -> anyhow::Result<()> {
        let next = store
            .artifact_epochs()
            .await?
            .into_iter()
            .max()
            .unwrap_or(0)
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("runtime epoch space exhausted"))?;
        self.next_epoch.fetch_max(next, Ordering::Relaxed);
        Ok(())
    }

    async fn sweep_orphans_with_store(&self, store: &RuntimeConfigStore) -> anyhow::Result<u64> {
        let artifacts = store.artifact_epochs().await?;
        let max_seen = artifacts.iter().copied().max().unwrap_or(0);
        for epoch in artifacts {
            let pid_path = epoch_pid_path(store.dir(), epoch);
            if tokio::fs::try_exists(pid_path.as_std_path()).await? {
                reap_epoch_pid_file(pid_path.as_std_path(), store.dir().as_std_path()).await?;
            }
            store.cleanup_epoch(epoch).await?;
        }
        let next = max_seen
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("runtime epoch space exhausted"))?;
        self.next_epoch.fetch_max(next, Ordering::Relaxed);
        Ok(max_seen)
    }

    pub(crate) async fn initialize_runtime_store(&self) -> anyhow::Result<()> {
        let store = RuntimeConfigStore::open(crate::utils::dirs::service_config_dir()).await?;
        self.sweep_orphans_with_store(&store).await?;
        Ok(())
    }

    fn revision_for_plan(
        &self,
        plan: ReconcilePlan,
        current: Option<&ConfigRevisionInfo>,
        digest: &str,
    ) -> anyhow::Result<ConfigRevisionInfo> {
        match (plan, current) {
            (ReconcilePlan::Noop, Some(current)) => Ok(current.clone()),
            (
                ReconcilePlan::Patch | ReconcilePlan::Reload | ReconcilePlan::Restart,
                Some(current),
            ) => Ok(ConfigRevisionInfo {
                epoch: current.epoch,
                generation: current
                    .generation
                    .checked_add(1)
                    .ok_or_else(|| anyhow::anyhow!("config revision generation space exhausted"))?,
                source_hash: digest.to_owned(),
                effective_hash: digest.to_owned(),
            }),
            (
                ReconcilePlan::Switch
                | ReconcilePlan::Patch
                | ReconcilePlan::Reload
                | ReconcilePlan::Restart,
                _,
            ) => Ok(ConfigRevisionInfo {
                epoch: self.next_epoch.fetch_add(1, Ordering::Relaxed),
                generation: 1,
                source_hash: digest.to_owned(),
                effective_hash: digest.to_owned(),
            }),
            (ReconcilePlan::Noop, None) => {
                anyhow::bail!("cannot produce a noop revision without an applied runtime")
            }
        }
    }

    async fn stage_v2_config(
        &self,
        store: &RuntimeConfigStore,
        core_type: &CoreType,
        config: &str,
        epoch: u64,
    ) -> anyhow::Result<StagedRuntimeConfig> {
        let (app_dir, binary_path) = Self::launch_paths(core_type)?;
        let staged = store.stage(epoch, config.as_bytes()).await?;
        if let Err(error) =
            CoreInstance::check_config_(core_type, staged.path(), &binary_path, &app_dir).await
        {
            return Err(classified_operation_error(
                CoreErrorKind::ConfigCheckFailed,
                format!("core config preflight failed: {error}"),
            ));
        }
        Ok(staged)
    }

    async fn wait_controller_startup_readiness<F>(
        connection: &CoreApiConnection,
        is_running: F,
    ) -> anyhow::Result<()>
    where
        F: Fn() -> bool,
    {
        let client = controller_client(connection).map_err(|error| {
            anyhow::anyhow!("failed to build startup controller client: {error}")
        })?;
        let deadline = tokio::time::Instant::now() + STARTUP_READINESS_TIMEOUT;

        loop {
            anyhow::ensure!(
                is_running(),
                "core process stopped before startup readiness"
            );

            let probe_error =
                match tokio::time::timeout(RECONCILE_PROBE_TIMEOUT, client.version()).await {
                    Ok(Ok(_)) => {
                        anyhow::ensure!(
                            is_running(),
                            "core process stopped during startup readiness"
                        );
                        return Ok(());
                    }
                    Ok(Err(error)) => error.to_string(),
                    Err(_) => format!(
                        "controller version probe timed out after {}ms",
                        RECONCILE_PROBE_TIMEOUT.as_millis()
                    ),
                };

            if tokio::time::Instant::now() >= deadline {
                anyhow::bail!(
                    "core did not become healthy before the startup timeout: {probe_error}"
                );
            }
            tokio::time::sleep(STARTUP_READINESS_INTERVAL).await;
        }
    }

    async fn wait_startup_readiness(
        instance: Arc<ServiceRuntimeProcess>,
        connection: Option<CoreApiConnection>,
    ) -> anyhow::Result<()> {
        let Some(connection) = connection else {
            tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
            anyhow::ensure!(
                matches!(instance.state(), ServiceProcessState::Running),
                "core process stopped before the legacy startup survival gate"
            );
            return Ok(());
        };

        Self::wait_controller_startup_readiness(&connection, || {
            matches!(instance.state(), ServiceProcessState::Running)
        })
        .await
    }

    async fn run_controller_health_driver<F>(
        connection: CoreApiConnection,
        health: Arc<parking_lot::Mutex<Option<CoreHealthInfo>>>,
        probe_lock: Arc<Mutex<()>>,
        cancel_token: CancellationToken,
        is_running: F,
    ) where
        F: Fn() -> bool + Send + Sync + 'static,
    {
        let client = match controller_client(&connection) {
            Ok(client) => client,
            Err(error) => {
                record_health_observation(
                    &health,
                    Some(&format!(
                        "failed to build liveness controller client: {error}"
                    )),
                );
                return;
            }
        };

        loop {
            tokio::select! {
                _ = cancel_token.cancelled() => break,
                _ = tokio::time::sleep(STARTUP_READINESS_INTERVAL) => {}
            }
            if !is_running() {
                break;
            }

            let _probe_guard = probe_lock.lock().await;
            let error = match tokio::time::timeout(RECONCILE_PROBE_TIMEOUT, client.version()).await
            {
                Ok(Ok(_)) => None,
                Ok(Err(error)) => Some(error.to_string()),
                Err(_) => Some(format!(
                    "controller version probe timed out after {}ms",
                    RECONCILE_PROBE_TIMEOUT.as_millis()
                )),
            };
            drop(_probe_guard);
            record_health_observation(&health, error.as_deref());
        }
    }

    async fn probe_controller_version(&self, connection: &CoreApiConnection) -> bool {
        let client = match controller_client(connection) {
            Ok(client) => client,
            Err(error) => {
                tracing::warn!("failed to build controller client for reconcile probe: {error}");
                return false;
            }
        };
        match tokio::time::timeout(RECONCILE_PROBE_TIMEOUT, client.version()).await {
            Ok(Ok(_)) => true,
            Ok(Err(error)) => {
                tracing::warn!("reconcile health probe GET /version failed: {error}");
                false
            }
            Err(_) => {
                tracing::warn!(
                    timeout_ms = RECONCILE_PROBE_TIMEOUT.as_millis(),
                    "reconcile health probe timed out"
                );
                false
            }
        }
    }

    async fn probe_reconcile_health(&self, connection: &CoreApiConnection) -> bool {
        let runtime = {
            let manager = self.manager.lock().await;
            manager.as_ref().map(|manager| {
                (
                    manager.instance.clone(),
                    manager.probe_lock.clone(),
                    manager.health.clone(),
                )
            })
        };
        let Some((instance, probe_lock, health)) = runtime else {
            tracing::warn!("reconcile health probe found no running service runtime owner");
            return false;
        };
        if !matches!(instance.state(), ServiceProcessState::Running) {
            tracing::warn!("reconcile health probe found the core process stopped");
            return false;
        }
        let _probe_guard = probe_lock.lock().await;
        let healthy = self.probe_controller_version(connection).await;
        drop(_probe_guard);
        record_health_observation(
            &health,
            (!healthy).then_some("reconcile controller-version probe failed"),
        );
        if !healthy {
            return false;
        }
        if !matches!(instance.state(), ServiceProcessState::Running) {
            tracing::warn!("core process stopped during reconcile health probe");
            return false;
        }
        true
    }

    async fn patch_and_verify_controller(
        &self,
        connection: &CoreApiConnection,
        patch: &clash_api::ConfigPatch,
        projection: &clash_api::RuntimeProjection,
    ) -> bool {
        let client = match controller_client(connection) {
            Ok(client) => client,
            Err(error) => {
                tracing::warn!("failed to build controller client for patch: {error}");
                return false;
            }
        };
        if let Err(error) = client.patch_config(patch).await {
            // Match the ref transaction semantics: PATCH may have been applied
            // before the transport reported an error, so verification decides.
            tracing::warn!("controller config PATCH returned an uncertain result: {error}");
        }
        match client.configs().await {
            Ok(runtime) => match projection.verify(&runtime) {
                Ok(true) => true,
                Ok(false) => false,
                Err(error) => {
                    tracing::warn!("controller config patch projection failed: {error}");
                    false
                }
            },
            Err(error) => {
                tracing::warn!("controller config patch verification failed: {error}");
                false
            }
        }
    }

    async fn try_patch(
        &self,
        connection: &CoreApiConnection,
        patch: &clash_api::ConfigPatch,
        projection: &clash_api::RuntimeProjection,
    ) -> bool {
        self.patch_and_verify_controller(connection, patch, projection)
            .await
            && self.probe_reconcile_health(connection).await
    }

    async fn reload_and_verify_controller(
        &self,
        connection: &CoreApiConnection,
        config_path: &Utf8Path,
    ) -> bool {
        let client = match controller_client(connection) {
            Ok(client) => client,
            Err(error) => {
                tracing::warn!("failed to build controller client for reload: {error}");
                return false;
            }
        };
        if let Err(error) = client
            .update_config_from_path(config_path.as_std_path())
            .await
        {
            tracing::warn!("controller config reload failed: {error}");
            return false;
        }
        if let Err(error) = client.configs().await {
            tracing::warn!("controller config reload verification failed: {error}");
            return false;
        }
        true
    }

    async fn try_reload(&self, connection: &CoreApiConnection, config_path: &Utf8Path) -> bool {
        self.reload_and_verify_controller(connection, config_path)
            .await
            && self.probe_reconcile_health(connection).await
    }

    async fn execute_v2(
        &self,
        command: CoreCommandInfo<'static>,
    ) -> anyhow::Result<OperationOutputInfo> {
        let _guard = self.operation_lock.lock().await;
        match command {
            CoreCommandInfo::Reconcile {
                core_type,
                config,
                expected_digest,
                expected_applied,
            } => {
                self.ensure_not_quarantined()?;
                let computed_digest = payload_digest(config.as_bytes());
                if let Some(expected_digest) = expected_digest
                    && expected_digest.as_ref() != computed_digest
                {
                    return Err(classified_operation_error(
                        CoreErrorKind::InvalidConfig,
                        format!(
                            "config digest mismatch: declared {}, computed {}",
                            expected_digest, computed_digest
                        ),
                    ));
                }

                let status = self.status().await;
                let was_running = matches!(status.state, CoreState::Running);
                let current_applied = status.revision.as_ref().map(ConfigRevisionInfo::id);
                if let Some(expected) = expected_applied
                    && current_applied.as_ref() != Some(&expected)
                {
                    return Err(classified_operation_error(
                        CoreErrorKind::RevisionConflict,
                        format!(
                            "revision conflict: expected {:?}, applied {:?}",
                            expected, current_applied
                        ),
                    ));
                }
                if was_running && status.revision.is_none() {
                    return Err(classified_operation_error(
                        CoreErrorKind::Internal,
                        "running service runtime has no authoritative v2 revision",
                    ));
                }

                let mut previous_runtime = if was_running {
                    let manager = self.manager.lock().await;
                    let manager = manager.as_ref().ok_or_else(|| {
                        classified_operation_error(
                            CoreErrorKind::Internal,
                            "running core status had no service runtime owner",
                        )
                    })?;
                    Some((
                        manager.instance.core_type.clone(),
                        manager.config_path.clone(),
                        status.revision.clone(),
                        self.api_connection.lock().clone(),
                    ))
                } else {
                    None
                };
                let current_config = match previous_runtime.as_ref() {
                    Some((_, path, _, _)) => tokio::fs::read_to_string(path).await.ok(),
                    None => None,
                };
                let mut plan = if was_running {
                    classify_reconcile(
                        previous_runtime
                            .as_ref()
                            .map(|(current_core, _, _, _)| current_core)
                            .or(status.r#type.as_ref()),
                        status.revision.as_ref(),
                        current_config.as_deref(),
                        &core_type,
                        &computed_digest,
                        config.as_ref(),
                    )
                } else {
                    ReconcilePlan::Restart
                };
                if plan == ReconcilePlan::Noop {
                    let revision = status
                        .revision
                        .expect("running v2 runtime was checked to have a revision");
                    return Ok(OperationOutputInfo::Reconciled(ReconcileOutcomeInfo {
                        outcome: ReconcileOutcomeKind::Noop,
                        revision,
                        warning: None,
                        failed_apply: None,
                    }));
                }

                let store =
                    RuntimeConfigStore::open(crate::utils::dirs::service_config_dir()).await?;
                self.sync_epoch_allocator(&store).await?;
                let desired_revision =
                    self.revision_for_plan(plan, status.revision.as_ref(), &computed_digest)?;
                // Match the ref manager's clean-abort boundary: stage + fsync the
                // candidate and validate it with the target core before mutation.
                let staged = self
                    .stage_v2_config(&store, &core_type, config.as_ref(), desired_revision.epoch)
                    .await?;

                let same_epoch = status
                    .revision
                    .as_ref()
                    .is_some_and(|current| current.epoch == desired_revision.epoch);
                let (config_path, mut durability_warning, mut backup): (
                    Utf8PathBuf,
                    Option<String>,
                    Option<RuntimeConfigBackup>,
                ) = if same_epoch {
                    let previous_path = previous_runtime
                        .as_ref()
                        .map(|(_, path, _, _)| path.as_path())
                        .ok_or_else(|| {
                            classified_operation_error(
                                CoreErrorKind::Internal,
                                "same-epoch apply has no current runtime",
                            )
                        })?;
                    let seeded = store
                        .seed_current(previous_path, desired_revision.epoch)
                        .await?;
                    let (stable_path, seed_warning) = seeded.into_parts();
                    if let Some((_, path, _, _)) = previous_runtime.as_mut() {
                        *path = stable_path.clone();
                    }
                    let current_backup = store
                        .backup(
                            stable_path.as_path(),
                            desired_revision.epoch,
                            desired_revision.generation,
                        )
                        .await?;
                    let commit = match store.commit_replace(staged, desired_revision.epoch).await {
                        Ok(commit) => commit,
                        Err(error) => {
                            let _ = store.remove_backup(current_backup).await;
                            return Err(error);
                        }
                    };
                    let (path, commit_warning) = commit.into_parts();
                    (
                        path,
                        merge_warnings(seed_warning, commit_warning),
                        Some(current_backup),
                    )
                } else {
                    let commit = match store.commit_new(staged, desired_revision.epoch).await {
                        Ok(commit) => commit,
                        Err(error) => {
                            let _ = store.cleanup_epoch(desired_revision.epoch).await;
                            return Err(error);
                        }
                    };
                    let (path, warning) = commit.into_parts();
                    (path, warning, None)
                };

                if plan == ReconcilePlan::Patch {
                    let patch = current_config.as_deref().and_then(|current| {
                        let serde_yaml::Value::Mapping(current) =
                            serde_yaml::from_str::<serde_yaml::Value>(current).ok()?
                        else {
                            return None;
                        };
                        let serde_yaml::Value::Mapping(desired) =
                            serde_yaml::from_str::<serde_yaml::Value>(config.as_ref()).ok()?
                        else {
                            return None;
                        };
                        build_patch(&current, &desired)
                    });
                    let patched = match (previous_runtime.as_ref(), patch.as_ref()) {
                        (Some((_, _, _, Some(connection))), Some((patch, projection))) => {
                            self.try_patch(connection, patch, projection).await
                        }
                        _ => false,
                    };
                    if patched {
                        let revision = desired_revision.clone();
                        {
                            let mut manager = self.manager.lock().await;
                            let manager = manager.as_mut().ok_or_else(|| {
                                classified_operation_error(
                                    CoreErrorKind::Internal,
                                    "patched core lost its service runtime owner",
                                )
                            })?;
                            manager.config_path = config_path;
                        }
                        *self.applied_revision.lock() = Some(revision.clone());
                        *self.api_connection.lock() =
                            api_connection_from_config(config.as_ref(), &revision);
                        if let Some(current_backup) = backup.take()
                            && let Err(error) = store.remove_backup(current_backup).await
                        {
                            tracing::warn!("failed to remove successful patch backup: {error}");
                        }
                        return Ok(OperationOutputInfo::Reconciled(ReconcileOutcomeInfo {
                            outcome: ReconcileOutcomeKind::Patched,
                            revision,
                            warning: durability_warning.clone(),
                            failed_apply: None,
                        }));
                    }
                    plan = ReconcilePlan::Restart;
                }

                if plan == ReconcilePlan::Reload {
                    let reloaded = match previous_runtime.as_ref() {
                        Some((_, _, _, Some(connection))) => {
                            self.try_reload(connection, config_path.as_path()).await
                        }
                        _ => false,
                    };
                    if reloaded {
                        let revision = desired_revision.clone();
                        {
                            let mut manager = self.manager.lock().await;
                            let manager = manager.as_mut().ok_or_else(|| {
                                classified_operation_error(
                                    CoreErrorKind::Internal,
                                    "reloaded core lost its service runtime owner",
                                )
                            })?;
                            manager.config_path = config_path;
                        }
                        *self.applied_revision.lock() = Some(revision.clone());
                        *self.api_connection.lock() =
                            api_connection_from_config(config.as_ref(), &revision);
                        if let Some(current_backup) = backup.take()
                            && let Err(error) = store.remove_backup(current_backup).await
                        {
                            tracing::warn!("failed to remove successful reload backup: {error}");
                        }
                        return Ok(OperationOutputInfo::Reconciled(ReconcileOutcomeInfo {
                            outcome: ReconcileOutcomeKind::Reloaded,
                            revision,
                            warning: durability_warning.clone(),
                            failed_apply: None,
                        }));
                    }
                    plan = ReconcilePlan::Restart;
                }

                if was_running && let Err(stop_error) = self.stop().await {
                    let reason = with_durability_warning(
                        format!("failed to stop current runtime: {stop_error}"),
                        durability_warning.as_deref(),
                    );
                    return Err(classified_operation_error(
                        CoreErrorKind::StopUnconfirmed,
                        reason,
                    ));
                }

                if let Err(apply_error) = self
                    .start(&core_type, config_path.as_path(), desired_revision.epoch)
                    .await
                {
                    let apply_error = apply_error.to_string();
                    let Some((previous_core, mut previous_path, previous_revision, previous_api)) =
                        previous_runtime
                    else {
                        let _ = store.cleanup_epoch(desired_revision.epoch).await;
                        let reason = with_durability_warning(
                            format!("desired runtime failed to start: {apply_error}"),
                            durability_warning.as_deref(),
                        );
                        return Err(classified_operation_error(
                            CoreErrorKind::ApplyFailed,
                            reason,
                        ));
                    };

                    if let Some(current_backup) = backup.as_ref() {
                        let restored = match store.restore(current_backup).await {
                            Ok(restored) => restored,
                            Err(restore_error) => {
                                let reason = with_durability_warning(
                                    format!(
                                        "desired runtime failed to start ({apply_error}); runtime restore failed: {restore_error}"
                                    ),
                                    durability_warning.as_deref(),
                                );
                                return Err(classified_operation_error(
                                    CoreErrorKind::ApplyRollbackFailed,
                                    reason,
                                ));
                            }
                        };
                        let (restored_path, restore_warning) = restored.into_parts();
                        previous_path = restored_path;
                        durability_warning = merge_warnings(durability_warning, restore_warning);
                    }

                    let previous_epoch = previous_revision
                        .as_ref()
                        .map(|revision| revision.epoch)
                        .ok_or_else(|| {
                            classified_operation_error(
                                CoreErrorKind::Internal,
                                format!(
                                    "desired runtime failed to start ({apply_error}); previous runtime has no authoritative v2 revision"
                                ),
                            )
                        })?;
                    if let Err(rollback_error) = self
                        .start(&previous_core, previous_path.as_path(), previous_epoch)
                        .await
                    {
                        let reason = with_durability_warning(
                            format!(
                                "desired runtime failed to start ({apply_error}); rollback failed: {rollback_error}"
                            ),
                            durability_warning.as_deref(),
                        );
                        return Err(classified_operation_error(
                            CoreErrorKind::ApplyRollbackFailed,
                            reason,
                        ));
                    }

                    if let Some(current_backup) = backup.take()
                        && let Err(error) = store.remove_backup(current_backup).await
                    {
                        tracing::warn!("failed to remove successful rollback backup: {error}");
                    }
                    if desired_revision.epoch
                        != previous_revision
                            .as_ref()
                            .map_or(desired_revision.epoch, |r| r.epoch)
                        && let Err(error) = store.cleanup_epoch(desired_revision.epoch).await
                    {
                        tracing::warn!("failed to clean rejected switch epoch config: {error}");
                    }

                    *self.applied_revision.lock() = previous_revision.clone();
                    *self.api_connection.lock() = previous_api;
                    let revision = previous_revision.ok_or_else(|| {
                        classified_operation_error(
                            CoreErrorKind::Internal,
                            with_durability_warning(
                                format!(
                                    "desired runtime failed to start ({apply_error}); previous runtime was restored but has no v2 revision"
                                ),
                                durability_warning.as_deref(),
                            ),
                        )
                    })?;
                    return Ok(OperationOutputInfo::Reconciled(ReconcileOutcomeInfo {
                        outcome: ReconcileOutcomeKind::RolledBack,
                        revision,
                        warning: durability_warning,
                        failed_apply: Some(apply_error),
                    }));
                }

                if let Some(current_backup) = backup.take()
                    && let Err(error) = store.remove_backup(current_backup).await
                {
                    tracing::warn!("failed to remove successful restart backup: {error}");
                }
                if plan == ReconcilePlan::Switch
                    && let Some(previous_revision) = status.revision.as_ref()
                    && let Err(error) = store.cleanup_epoch(previous_revision.epoch).await
                {
                    tracing::warn!("failed to clean retired switch epoch config: {error}");
                }

                let revision = desired_revision;
                *self.applied_revision.lock() = Some(revision.clone());
                *self.api_connection.lock() =
                    api_connection_from_config(config.as_ref(), &revision);
                Ok(OperationOutputInfo::Reconciled(ReconcileOutcomeInfo {
                    outcome: if !was_running {
                        ReconcileOutcomeKind::Started
                    } else if plan == ReconcilePlan::Switch {
                        ReconcileOutcomeKind::Switched
                    } else {
                        ReconcileOutcomeKind::Restarted
                    },
                    revision,
                    warning: durability_warning,
                    failed_apply: None,
                }))
            }
            CoreCommandInfo::Stop => {
                if matches!(self.status().await.state, CoreState::Running) {
                    self.stop().await?;
                } else {
                    *self.applied_revision.lock() = None;
                    *self.api_connection.lock() = None;
                }
                Ok(OperationOutputInfo::Stopped)
            }
            CoreCommandInfo::Recover => {
                self.recover_quarantine().await?;
                Ok(OperationOutputInfo::Recovered)
            }
        }
    }

    pub async fn submit_v2(&self, request: &CoreSubmitReq<'_>) -> Result<OperationInfo, OpError> {
        let id = request.operation_id.as_ref();
        Self::validate_operation_id(id)?;
        let command = request.command.clone().into_owned();
        let fingerprint =
            serde_json::to_string(&command).map_err(|error| OpError::plain(error.to_string()))?;

        let (queued, sender) = match self.admit_operation(id, fingerprint)? {
            OperationAdmission::Existing(info) => return Ok(info),
            OperationAdmission::Registered { info, sender } => (info, sender),
        };

        let id = id.to_owned();
        let service = self.clone();
        tokio::spawn(async move {
            sender.send_replace(OperationInfo::running(id.clone()));
            let terminal = match service.execute_v2(command).await {
                Ok(output) => OperationInfo::succeeded(id.clone(), output),
                Err(error) => {
                    let error = OpError::from(error);
                    OperationInfo::failed_with_kind(
                        id.clone(),
                        error.kind,
                        error.message,
                        error.retryable.unwrap_or(false),
                    )
                }
            };
            sender.send_replace(terminal);
        });

        Ok(queued)
    }

    pub async fn operation_v2(
        &self,
        request: &CoreOperationReq<'_>,
    ) -> Result<OperationInfo, OpError> {
        let id = request.operation_id.as_ref();
        Self::validate_operation_id(id)?;
        let mut receiver = self
            .operations
            .lock()
            .records
            .get(id)
            .map(|record| record.receiver.clone())
            .ok_or_else(|| OpError::plain(format!("unknown operation id: {id}")))?;
        let current = receiver.borrow().clone();
        if current.is_terminal() || request.wait_ms.unwrap_or(0) == 0 {
            return Ok(current);
        }

        let wait = std::time::Duration::from_millis(request.wait_ms.unwrap_or(0))
            .min(OPERATION_WAIT_LIMIT);
        let _ = tokio::time::timeout(wait, async {
            loop {
                if receiver.borrow().is_terminal() {
                    break;
                }
                if receiver.changed().await.is_err() {
                    break;
                }
            }
        })
        .await;
        Ok(receiver.borrow().clone())
    }

    pub fn api_connection_v2(&self) -> Option<CoreApiConnection> {
        self.api_connection.lock().clone()
    }

    /// Get the status of the core instance
    pub async fn status(&self) -> chimera_ipc::api::status::CoreInfos {
        let manager = self.manager.lock().await;
        let state_changed_at = self
            .state_changed_at
            .load(std::sync::atomic::Ordering::Relaxed);
        let mut state = Self::state_(manager.as_ref()).into_owned();
        if !matches!(state, CoreState::Running)
            && let Some(reason) = self.quarantine_reason()
        {
            state = CoreState::Stopped(Some(reason));
        }
        match *manager {
            Some(ref manager) => chimera_ipc::api::status::CoreInfos {
                r#type: Some(manager.instance.core_type.clone()),
                health: matches!(state, CoreState::Running)
                    .then(|| manager.health.lock().clone())
                    .flatten(),
                revision: matches!(state, CoreState::Running)
                    .then(|| self.applied_revision.lock().clone())
                    .flatten(),
                state,
                state_changed_at,
                config_path: Some(manager.config_path.clone().into()),
            },
            None => chimera_ipc::api::status::CoreInfos {
                r#type: None,
                state,
                state_changed_at,
                config_path: None,
                health: None,
                revision: None,
            },
        }
    }

    fn state_(manager: Option<&CoreManager>) -> Cow<'static, CoreState> {
        match manager {
            None => Cow::Borrowed(&CoreState::Stopped(None)),
            Some(manager) => Cow::Owned(match manager.instance.state() {
                ServiceProcessState::Running => CoreState::Running,
                ServiceProcessState::Stopped => CoreState::Stopped(None),
            }),
        }
    }

    fn notify_state_changed(tx: Arc<Option<MpscSender<CoreState>>>, state: CoreState) {
        tokio::spawn(async move {
            if let Some(notify) = tx.as_ref() {
                let _ = notify.send(state).await;
            }
        });
    }

    #[allow(clippy::manual_async_fn)]
    fn recover_core(self, counter: usize) -> impl Future<Output = ()> + Send + Sync + 'static {
        async move {
            tracing::info!("Try to recover the core instance");
            if let Err(e) = self.restart().await {
                tracing::error!("Failed to recover the core instance: {}", e);
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                if counter < 5 {
                    Box::pin(self.recover_core(counter + 1)).await;
                } else {
                    tracing::error!("Failed to recover the core instance after 5 times");
                }
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn handle_process_event(
        break_loop: &mut bool,
        err_buf: &mut Vec<String>,
        state_changed_at: &AtomicI64,
        state_changed_notify: &Arc<Option<MpscSender<CoreState>>>,
        tx: &MpscSender<anyhow::Result<()>>,
        cancel_token: &CancellationToken,
        service_manager: CoreManagerService,
        instance: &Arc<ServiceRuntimeProcess>,
        event: ProcessEvent,
    ) {
        match event {
            ProcessEvent::Stdout(line) => {
                tracing::info!("{}", line);
            }
            ProcessEvent::Stderr(line) => {
                tracing::error!("{}", line);
                err_buf.push(line);
            }
            ProcessEvent::Error(error) => {
                tracing::warn!("core output pump warning: {error}");
                err_buf.push(error);
            }
            ProcessEvent::Terminated(status) => {
                instance.mark_stopped();
                if let Err(error) = instance.cleanup_pid_record().await {
                    tracing::warn!("failed to clean epoch pid record after core exit: {error}");
                }
                tracing::info!("core terminated with status: {:?}", status);
                state_changed_at.store(get_current_ts(), Ordering::Relaxed);
                Self::notify_state_changed(state_changed_notify.clone(), CoreState::Stopped(None));
                let err = anyhow::anyhow!(format!(
                    "core terminated with status: {:?}\n{}",
                    status,
                    err_buf.join("\n")
                ));
                if !cancel_token.is_cancelled() {
                    tracing::error!("{err}");
                }
                if tx.send(Err(err)).await.is_err() && !cancel_token.is_cancelled() {
                    tokio::spawn(async move {
                        service_manager.recover_core(0).await;
                    });
                }
                *break_loop = true;
            }
            _ => {}
        }
    }

    pub async fn start_legacy(
        &self,
        core_type: &CoreType,
        source_config: &Utf8Path,
    ) -> Result<(), anyhow::Error> {
        let source = tokio::fs::read_to_string(source_config).await?;
        let store = RuntimeConfigStore::open(crate::utils::dirs::service_config_dir()).await?;
        self.sync_epoch_allocator(&store).await?;
        let epoch = self.next_epoch.fetch_add(1, Ordering::Relaxed);
        let staged = self
            .stage_v2_config(&store, core_type, &source, epoch)
            .await?;
        let commit = match store.commit_new(staged, epoch).await {
            Ok(commit) => commit,
            Err(error) => {
                let _ = store.cleanup_epoch(epoch).await;
                return Err(error);
            }
        };
        let (config_path, warning) = commit.into_parts();
        if let Some(warning) = warning {
            tracing::warn!("legacy service start durability warning: {warning}");
        }
        if let Err(error) = self.start(core_type, config_path.as_path(), epoch).await {
            let _ = store.cleanup_epoch(epoch).await;
            return Err(error);
        }
        Ok(())
    }

    #[instrument(skip(self))]
    async fn start(
        &self,
        core_type: &CoreType,
        config_path: &Utf8Path,
        epoch: u64,
    ) -> Result<(), anyhow::Error> {
        let mut manager = self.manager.lock().await;
        let state = Self::state_(manager.as_ref());
        if matches!(state.as_ref(), CoreState::Running) {
            return Err(classified_operation_error(
                CoreErrorKind::AlreadyRunning,
                "core is already running",
            ));
        }
        *self.applied_revision.lock() = None;
        *self.api_connection.lock() = None;

        let config_path = config_path.canonicalize_utf8()?;
        let config_path =
            Utf8PathBuf::from_path_buf(dunce::simplified(config_path.as_std_path()).to_path_buf())
                .map_err(|_| anyhow::anyhow!("runtime config path is not valid UTF-8"))?;
        tokio::fs::metadata(&config_path).await?;
        let startup_config = tokio::fs::read_to_string(&config_path).await?;
        let startup_probe_revision = ConfigRevisionInfo {
            epoch,
            generation: 0,
            source_hash: String::new(),
            effective_hash: String::new(),
        };
        let startup_connection =
            api_connection_from_config(&startup_config, &startup_probe_revision);
        anyhow::ensure!(
            config_path.file_name() == Some(format!("config-{epoch}.yaml").as_str()),
            "runtime config path does not match epoch {epoch}: {config_path}"
        );
        let runtime_dir = config_path
            .parent()
            .ok_or_else(|| anyhow::anyhow!("runtime config path has no parent"))?;
        let pid_path = epoch_pid_path(runtime_dir, epoch);
        let (app_dir, binary_path) = Self::launch_paths(core_type)?;
        tracing::info!(
            core_type = ?core_type,
            app_dir = %app_dir,
            binary_path = %binary_path,
            pid_path = %pid_path,
            config_path = %config_path,
            epoch,
            "Starting Core"
        );

        let cancel_token = self.cancel_token.child_token();
        let health = Arc::new(parking_lot::Mutex::new(startup_connection.as_ref().map(
            |_| CoreHealthInfo {
                state: CoreHealthState::Starting,
                changed_at: get_current_ts(),
                consecutive_failures: 0,
                last_error: None,
                last_success_at: None,
            },
        )));
        let probe_lock = Arc::new(Mutex::new(()));
        let liveness_connection = startup_connection.clone();
        let (instance, mut events) = ServiceRuntimeProcess::spawn(
            core_type.clone(),
            app_dir,
            binary_path,
            config_path.clone(),
            pid_path,
            epoch,
        )
        .await?;

        let state_changed_at = self.state_changed_at.clone();
        let cancel_token_clone = cancel_token.clone();
        let (tx, mut rx) = tokio::sync::mpsc::channel::<anyhow::Result<()>>(1);
        let service = self.clone();
        let state_changed_notify = self.state_changed_notify.clone();
        let instance_events = instance.clone();
        let tracker = TaskTracker::new();
        let tx_events = tx.clone();
        tracker.spawn(async move {
            let mut err_buf: Vec<String> = Vec::with_capacity(6);
            let mut break_loop = false;
            while let Some(event) = events.recv().await {
                Self::handle_process_event(
                    &mut break_loop,
                    &mut err_buf,
                    &state_changed_at,
                    &state_changed_notify,
                    &tx_events,
                    &cancel_token_clone,
                    service.clone(),
                    &instance_events,
                    event,
                )
                .await;
                if break_loop {
                    break;
                }
            }
        });

        let readiness_instance = instance.clone();
        let readiness_tx = tx.clone();
        tracker.spawn(async move {
            let result = Self::wait_startup_readiness(readiness_instance, startup_connection).await;
            let _ = readiness_tx.send(result).await;
        });

        let cancel_token_clone = cancel_token.clone();
        let service = self.clone();
        tracker.spawn(async move {
            cancel_token_clone.cancelled().await;
            if service.manager.try_lock().is_ok() {
                let _ = service.stop().await;
            }
        });

        match rx.recv().await {
            Some(Ok(())) => {
                if let Some(connection) = liveness_connection {
                    record_health_observation(&health, None);
                    let liveness_instance = instance.clone();
                    let liveness_health = health.clone();
                    let liveness_probe_lock = probe_lock.clone();
                    let liveness_cancel = cancel_token.clone();
                    tracker.spawn(async move {
                        Self::run_controller_health_driver(
                            connection,
                            liveness_health,
                            liveness_probe_lock,
                            liveness_cancel,
                            move || {
                                matches!(liveness_instance.state(), ServiceProcessState::Running)
                            },
                        )
                        .await;
                    });
                }
                tracker.close();
            }
            Some(Err(error)) => {
                tracker.close();
                cancel_token.cancel();
                if let Err(kill_error) = instance.kill().await {
                    let reason = format!(
                        "{error}; failed to confirm rejected startup process dead: {kill_error}"
                    );
                    self.latch_quarantine(epoch, reason.clone());
                    tracker.wait().await;
                    return Err(classified_operation_error(
                        CoreErrorKind::StopUnconfirmed,
                        reason,
                    ));
                }
                tracker.wait().await;
                return Err(error);
            }
            None => {
                tracker.close();
                cancel_token.cancel();
                let startup_error = "core startup task ended without a readiness result";
                if let Err(kill_error) = instance.kill().await {
                    let reason = format!(
                        "{startup_error}; failed to confirm rejected startup process dead: {kill_error}"
                    );
                    self.latch_quarantine(epoch, reason.clone());
                    tracker.wait().await;
                    return Err(classified_operation_error(
                        CoreErrorKind::StopUnconfirmed,
                        reason,
                    ));
                }
                tracker.wait().await;
                anyhow::bail!("{startup_error}");
            }
        }
        drop(rx);
        self.state_changed_at
            .store(get_current_ts(), Ordering::Relaxed);
        Self::notify_state_changed(self.state_changed_notify.clone(), CoreState::Running);
        *manager = Some(CoreManager {
            instance,
            config_path,
            cancel_token,
            tracker: Some(tracker),
            health,
            probe_lock,
        });
        Ok(())
    }

    pub async fn restart(&self) -> Result<(), anyhow::Error> {
        let (core_type, config_path, epoch, running) = {
            let manager = self.manager.lock().await;
            let manager = manager.as_ref().ok_or_else(|| {
                classified_operation_error(
                    CoreErrorKind::NotStarted,
                    "core have not been started yet",
                )
            })?;
            let epoch = self
                .applied_revision
                .lock()
                .as_ref()
                .map(|revision| revision.epoch)
                .or_else(|| epoch_from_runtime_path(manager.config_path.as_path()))
                .ok_or_else(|| {
                    classified_operation_error(
                        CoreErrorKind::Internal,
                        "core restart has no authoritative epoch",
                    )
                })?;
            (
                manager.instance.core_type.clone(),
                manager.config_path.clone(),
                epoch,
                matches!(Self::state_(Some(manager)).as_ref(), CoreState::Running),
            )
        };
        if running {
            self.stop().await?;
        }
        self.start(&core_type, config_path.as_path(), epoch).await
    }

    pub async fn stop(&self) -> Result<(), anyhow::Error> {
        let mut manager = self.manager.lock().await;
        let state = Self::state_(manager.as_ref());
        if matches!(state.as_ref(), CoreState::Stopped(_)) {
            return Err(classified_operation_error(
                CoreErrorKind::NotStarted,
                "core is already stopped",
            ));
        }

        if let Some(manager) = manager.as_mut() {
            let epoch = self
                .applied_revision
                .lock()
                .as_ref()
                .map(|revision| revision.epoch)
                .or_else(|| epoch_from_runtime_path(manager.config_path.as_path()))
                .ok_or_else(|| {
                    classified_operation_error(
                        CoreErrorKind::Internal,
                        "stopping core has no authoritative epoch",
                    )
                })?;
            manager.cancel_token.cancel();
            if let Err(error) = manager.instance.kill().await {
                let reason = format!("core process death could not be confirmed: {error}");
                self.latch_quarantine(epoch, reason.clone());
                return Err(classified_operation_error(
                    CoreErrorKind::StopUnconfirmed,
                    reason,
                ));
            }
            if let Some(tracker) = manager.tracker.take() {
                tracker.wait().await;
            }
        }

        Self::notify_state_changed(self.state_changed_notify.clone(), CoreState::Stopped(None));
        *self.applied_revision.lock() = None;
        *self.api_connection.lock() = None;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::{
        borrow::Cow,
        sync::{Arc, atomic::Ordering},
    };

    use chimera_ipc::api::{
        CoreErrorKind,
        core::v2::{
            CoreApiConnection, CoreCommandInfo, CoreControllerInfo, CoreOperationReq,
            CoreSubmitReq, OperationOutputInfo, OperationPhase, payload_digest,
        },
        status::{ConfigRevisionInfo, CoreHealthInfo, CoreHealthState, CoreState, RevisionIdInfo},
    };
    use tokio_util::sync::CancellationToken;

    use super::{
        CoreManagerService, OperationAdmission, ReconcilePlan, api_connection_from_config,
        classify_config_documents, classify_reconcile, controller_client, healthy_health,
        with_durability_warning,
    };

    const OPERATION_ID: &str = "00112233445566778899aabbccddeeff";

    fn service() -> CoreManagerService {
        let (notify, _receiver) = tokio::sync::mpsc::channel(4);
        CoreManagerService::new_with_notify(notify, CancellationToken::new())
    }

    #[test]
    fn durability_warning_preserves_primary_terminal_error_message() {
        assert_eq!(
            with_durability_warning(
                "desired runtime failed",
                Some("parent-directory synchronization failed"),
            ),
            "desired runtime failed; runtime durability warning: parent-directory synchronization failed"
        );
        assert_eq!(
            with_durability_warning("desired runtime failed", None),
            "desired runtime failed"
        );
    }

    #[test]
    fn reconcile_classification_preserves_noop_patch_reload_restart_and_switch_boundaries() {
        let mihomo =
            chimera_utils::core::CoreType::Clash(chimera_utils::core::ClashCoreType::Mihomo);
        let clash_rs =
            chimera_utils::core::CoreType::Clash(chimera_utils::core::ClashCoreType::ClashRust);
        let revision = ConfigRevisionInfo {
            epoch: 4,
            generation: 2,
            source_hash: "same-digest".into(),
            effective_hash: "same-digest".into(),
        };

        let current = "external-controller: 127.0.0.1:9090\nmode: rule\nrules:\n  - MATCH,DIRECT\n";
        let semantically_same =
            "mode: rule\nrules: ['MATCH,DIRECT']\nexternal-controller: 127.0.0.1:9090\n";
        let reloadable = "external-controller: 127.0.0.1:9090\nmode: rule\nrules:\n  - DOMAIN,example.com,DIRECT\n  - MATCH,DIRECT\n";
        let patchable =
            "external-controller: 127.0.0.1:9090\nmode: global\nrules:\n  - MATCH,DIRECT\n";
        let controller_switch =
            "external-controller: 127.0.0.1:9999\nmode: rule\nrules:\n  - MATCH,DIRECT\n";
        let dns_current =
            "external-controller: 127.0.0.1:9090\ndns:\n  listen: 127.0.0.1:1053\n  ipv6: false\n";
        let dns_reload =
            "external-controller: 127.0.0.1:9090\ndns:\n  listen: 127.0.0.1:1053\n  ipv6: true\n";
        let dns_switch =
            "external-controller: 127.0.0.1:9090\ndns:\n  listen: 127.0.0.1:2053\n  ipv6: true\n";

        assert_eq!(
            classify_reconcile(
                Some(&mihomo),
                Some(&revision),
                Some(current),
                &mihomo,
                "same-digest",
                current,
            ),
            ReconcilePlan::Noop
        );
        assert_eq!(
            classify_reconcile(
                Some(&mihomo),
                Some(&revision),
                Some(current),
                &mihomo,
                "format-only-digest",
                semantically_same,
            ),
            ReconcilePlan::Noop
        );
        assert_eq!(
            classify_reconcile(
                Some(&mihomo),
                Some(&revision),
                Some(current),
                &mihomo,
                "changed-digest",
                reloadable,
            ),
            ReconcilePlan::Reload
        );
        assert_eq!(
            classify_reconcile(
                Some(&mihomo),
                Some(&revision),
                Some(dns_current),
                &mihomo,
                "dns-reload",
                dns_reload,
            ),
            ReconcilePlan::Reload
        );
        assert_eq!(
            classify_reconcile(
                Some(&mihomo),
                Some(&revision),
                Some(dns_current),
                &mihomo,
                "dns-switch",
                dns_switch,
            ),
            ReconcilePlan::Switch
        );
        assert_eq!(
            classify_reconcile(
                Some(&mihomo),
                Some(&revision),
                Some(current),
                &mihomo,
                "patch-digest",
                patchable,
            ),
            ReconcilePlan::Patch
        );
        assert_eq!(
            classify_reconcile(
                Some(&mihomo),
                Some(&revision),
                Some(current),
                &mihomo,
                "controller-switch",
                controller_switch,
            ),
            ReconcilePlan::Switch
        );
        assert_eq!(
            classify_reconcile(
                Some(&mihomo),
                Some(&revision),
                Some(current),
                &clash_rs,
                "same-digest",
                current,
            ),
            ReconcilePlan::Switch
        );
        assert_eq!(
            classify_reconcile(
                Some(&mihomo),
                Some(&revision),
                None,
                &mihomo,
                "missing-current",
                current,
            ),
            ReconcilePlan::Switch
        );
        assert_eq!(
            classify_reconcile(None, None, None, &mihomo, "same-digest", current),
            ReconcilePlan::Restart
        );
    }

    #[test]
    fn extended_patch_surface_matches_ref_safe_fields() {
        let current = r#"
tun:
  enable: true
  stack: mixed
  auto-route: false
  include-interface: [Ethernet]
tuic-server:
  enable: true
  listen: 127.0.0.1:10443
skip-auth-prefixes: [127.0.0.0/8]
ss-config: ss://old
"#;
        let desired = r#"
tun:
  enable: true
  stack: mixed
  auto-route: true
  include-interface: [Ethernet, Wi-Fi]
tuic-server:
  enable: true
  listen: 127.0.0.1:20443
skip-auth-prefixes: [127.0.0.0/8, 192.168.0.0/16]
ss-config: ss://new
"#;
        assert_eq!(
            classify_config_documents(current, desired),
            ReconcilePlan::Patch
        );

        let missing_enable = r#"
tun:
  auto-route: true
  include-interface: [Ethernet, Wi-Fi]
tuic-server:
  enable: true
  listen: 127.0.0.1:10443
skip-auth-prefixes: [127.0.0.0/8]
ss-config: ss://old
"#;
        assert_eq!(
            classify_config_documents(current, missing_enable),
            ReconcilePlan::Switch
        );
    }

    #[tokio::test]
    async fn v2_reload_uses_shared_controller_client_and_verifies_controller() {
        use std::sync::atomic::AtomicUsize;

        use axum::{Json, Router, http::StatusCode, routing::get};

        let puts = Arc::new(AtomicUsize::new(0));
        let gets = Arc::new(AtomicUsize::new(0));
        let puts_handler = puts.clone();
        let gets_handler = gets.clone();
        let app = Router::new().route(
            "/configs",
            get(move || {
                let gets = gets_handler.clone();
                async move {
                    gets.fetch_add(1, Ordering::Relaxed);
                    Json(serde_json::json!({"mode": "rule"}))
                }
            })
            .put(move || {
                let puts = puts_handler.clone();
                async move {
                    puts.fetch_add(1, Ordering::Relaxed);
                    StatusCode::NO_CONTENT
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let config_path = std::env::temp_dir().join(format!(
            "chimera-reload-{}-{}.yaml",
            std::process::id(),
            OPERATION_ID
        ));
        tokio::fs::write(&config_path, "mode: rule\n")
            .await
            .unwrap();
        let config_path = camino::Utf8PathBuf::from_path_buf(config_path).unwrap();
        let connection = CoreApiConnection {
            instance_id: "reload-test".into(),
            controller: CoreControllerInfo::Http(format!("http://{address}")),
            secret: None,
        };

        assert!(
            service()
                .reload_and_verify_controller(&connection, config_path.as_path())
                .await
        );
        assert_eq!(puts.load(Ordering::Relaxed), 1);
        assert_eq!(gets.load(Ordering::Relaxed), 1);

        server.abort();
        let _ = tokio::fs::remove_file(config_path).await;
    }

    #[tokio::test]
    async fn v2_patch_verifies_read_back_after_uncertain_patch_error() {
        use axum::{Json, Router, http::StatusCode, routing::get};

        let state = Arc::new(tokio::sync::Mutex::new(false));
        let read_state = state.clone();
        let patch_state = state.clone();
        let app = Router::new().route(
            "/configs",
            get(move || {
                let state = read_state.clone();
                async move {
                    Json(serde_json::json!({
                        "allow-lan": *state.lock().await,
                    }))
                }
            })
            .patch(move |Json(_body): Json<serde_json::Value>| {
                let state = patch_state.clone();
                async move {
                    *state.lock().await = true;
                    StatusCode::INTERNAL_SERVER_ERROR
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let connection = CoreApiConnection {
            instance_id: "patch-test".into(),
            controller: CoreControllerInfo::Http(format!("http://{address}")),
            secret: None,
        };
        let patch = clash_api::ConfigPatch {
            allow_lan: Some(true),
            ..clash_api::ConfigPatch::default()
        };
        let projection = clash_api::RuntimeProjection::from_patch(&patch).unwrap();

        assert!(
            service()
                .patch_and_verify_controller(&connection, &patch, &projection)
                .await
        );

        server.abort();
    }

    #[tokio::test]
    async fn v2_in_place_reconcile_requires_running_process_health() {
        use std::sync::atomic::AtomicUsize;

        use axum::{Json, Router, http::StatusCode, routing::get};

        let patches = Arc::new(AtomicUsize::new(0));
        let puts = Arc::new(AtomicUsize::new(0));
        let versions = Arc::new(AtomicUsize::new(0));
        let patch_handler = patches.clone();
        let put_handler = puts.clone();
        let version_handler = versions.clone();
        let app = Router::new()
            .route(
                "/configs",
                get(|| async { Json(serde_json::json!({"allow-lan": true})) })
                    .patch(move || {
                        let patches = patch_handler.clone();
                        async move {
                            patches.fetch_add(1, Ordering::Relaxed);
                            StatusCode::NO_CONTENT
                        }
                    })
                    .put(move || {
                        let puts = put_handler.clone();
                        async move {
                            puts.fetch_add(1, Ordering::Relaxed);
                            StatusCode::NO_CONTENT
                        }
                    }),
            )
            .route(
                "/version",
                get(move || {
                    let versions = version_handler.clone();
                    async move {
                        versions.fetch_add(1, Ordering::Relaxed);
                        Json(serde_json::json!({"meta": true, "version": "test"}))
                    }
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let connection = CoreApiConnection {
            instance_id: "health-gate-test".into(),
            controller: CoreControllerInfo::Http(format!("http://{address}")),
            secret: None,
        };
        let patch = clash_api::ConfigPatch {
            allow_lan: Some(true),
            ..clash_api::ConfigPatch::default()
        };
        let projection = clash_api::RuntimeProjection::from_patch(&patch).unwrap();
        let service = service();

        assert!(!service.try_patch(&connection, &patch, &projection).await);

        let config_path = std::env::temp_dir().join(format!(
            "chimera-health-reload-{}-{}.yaml",
            std::process::id(),
            OPERATION_ID
        ));
        tokio::fs::write(&config_path, "allow-lan: true\n")
            .await
            .unwrap();
        let config_path = camino::Utf8PathBuf::from_path_buf(config_path).unwrap();
        assert!(!service.try_reload(&connection, config_path.as_path()).await);

        assert_eq!(patches.load(Ordering::Relaxed), 1);
        assert_eq!(puts.load(Ordering::Relaxed), 1);
        assert_eq!(versions.load(Ordering::Relaxed), 0);

        server.abort();
        let _ = tokio::fs::remove_file(config_path).await;
    }

    #[tokio::test]
    async fn v2_startup_readiness_retries_controller_until_healthy() {
        use std::sync::atomic::AtomicUsize;

        use axum::{Json, Router, http::StatusCode, response::IntoResponse, routing::get};

        let attempts = Arc::new(AtomicUsize::new(0));
        let handler_attempts = attempts.clone();
        let app = Router::new().route(
            "/version",
            get(move || {
                let attempts = handler_attempts.clone();
                async move {
                    let attempt = attempts.fetch_add(1, Ordering::Relaxed);
                    if attempt < 2 {
                        StatusCode::SERVICE_UNAVAILABLE.into_response()
                    } else {
                        Json(serde_json::json!({"meta": true, "version": "test"})).into_response()
                    }
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let connection = CoreApiConnection {
            instance_id: "startup-readiness-test".into(),
            controller: CoreControllerInfo::Http(format!("http://{address}")),
            secret: None,
        };

        CoreManagerService::wait_controller_startup_readiness(&connection, || true)
            .await
            .unwrap();

        assert!(attempts.load(Ordering::Relaxed) >= 3);
        server.abort();
    }

    #[tokio::test]
    async fn v2_startup_readiness_fails_closed_when_process_is_not_running() {
        let connection = CoreApiConnection {
            instance_id: "startup-stopped-test".into(),
            controller: CoreControllerInfo::Http("http://127.0.0.1:9".into()),
            secret: None,
        };

        let error = CoreManagerService::wait_controller_startup_readiness(&connection, || false)
            .await
            .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("stopped before startup readiness")
        );
    }

    #[tokio::test]
    async fn v2_liveness_health_driver_tracks_unhealthy_and_recovery() {
        use std::sync::atomic::{AtomicBool, Ordering};

        use axum::{Json, Router, http::StatusCode, response::IntoResponse, routing::get};

        let controller_healthy = Arc::new(AtomicBool::new(false));
        let handler_health = controller_healthy.clone();
        let app = Router::new().route(
            "/version",
            get(move || {
                let healthy = handler_health.load(Ordering::Relaxed);
                async move {
                    if healthy {
                        Json(serde_json::json!({"meta": true, "version": "test"})).into_response()
                    } else {
                        StatusCode::SERVICE_UNAVAILABLE.into_response()
                    }
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let connection = CoreApiConnection {
            instance_id: "liveness-health-test".into(),
            controller: CoreControllerInfo::Http(format!("http://{address}")),
            secret: None,
        };
        let health = Arc::new(parking_lot::Mutex::new(Some(healthy_health(None))));
        let probe_lock = Arc::new(tokio::sync::Mutex::new(()));
        let cancel = CancellationToken::new();
        let task = tokio::spawn(CoreManagerService::run_controller_health_driver(
            connection,
            health.clone(),
            probe_lock,
            cancel.clone(),
            || true,
        ));

        tokio::time::sleep(std::time::Duration::from_millis(900)).await;
        {
            let current = health.lock().clone().unwrap();
            assert_eq!(current.state, CoreHealthState::Unhealthy);
            assert!(current.consecutive_failures >= 3);
            assert!(current.last_error.is_some());
            assert!(current.last_success_at.is_some());
        }

        controller_healthy.store(true, Ordering::Relaxed);
        tokio::time::sleep(std::time::Duration::from_millis(350)).await;
        {
            let current = health.lock().clone().unwrap();
            assert_eq!(current.state, CoreHealthState::Healthy);
            assert_eq!(current.consecutive_failures, 0);
            assert!(current.last_error.is_none());
            assert!(current.last_success_at.is_some());
        }

        cancel.cancel();
        task.await.unwrap();
        server.abort();
    }

    #[test]
    fn v2_health_error_detail_is_capped_without_breaking_utf8() {
        let detail = format!("{}{}", "x".repeat(511), "界".repeat(4));
        let health = super::unhealthy_observation(
            Some(&CoreHealthInfo {
                state: CoreHealthState::Healthy,
                changed_at: 1,
                consecutive_failures: 0,
                last_error: None,
                last_success_at: Some(1),
            }),
            &detail,
        );
        let error = health.last_error.unwrap();
        assert!(error.len() <= super::MAX_HEALTH_ERROR_BYTES);
        assert!(error.is_char_boundary(error.len()));
    }

    #[tokio::test]
    async fn v2_reconcile_controller_probe_uses_version_endpoint() {
        use axum::{Json, Router, routing::get};

        let app = Router::new().route(
            "/version",
            get(|| async { Json(serde_json::json!({"meta": true, "version": "test"})) }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let connection = CoreApiConnection {
            instance_id: "version-probe-test".into(),
            controller: CoreControllerInfo::Http(format!("http://{address}")),
            secret: None,
        };

        assert!(service().probe_controller_version(&connection).await);

        server.abort();
    }

    #[tokio::test]
    async fn v2_startup_orphan_sweep_reaps_owned_process_and_seeds_allocator() {
        use nyanpasu_utils::process::{Command, EpochPidFile, EpochPidFileSpec};

        let service = service();
        let epoch = 21;
        let dir = std::env::temp_dir().join(format!(
            "chimera-orphan-sweep-owned-{}-{}",
            std::process::id(),
            OPERATION_ID
        ));
        let store = super::RuntimeConfigStore::open(dir.clone()).await.unwrap();
        let staged = store.stage(epoch, b"mode: rule\n").await.unwrap();
        let commit = store.commit_new(staged, epoch).await.unwrap();
        let (config_path, warning) = commit.into_parts();
        assert!(warning.is_none());
        let pid_path = super::epoch_pid_path(store.dir(), epoch);

        #[cfg(windows)]
        let command = Command::new("ping.exe").args(["-n", "30", "127.0.0.1"]);
        #[cfg(unix)]
        let command = Command::new("/bin/sleep").arg("30");

        let (handle, _events) = command
            .epoch_pid_file(EpochPidFile::new(EpochPidFileSpec {
                pid_path: pid_path.as_std_path(),
                runtime_config: config_path.as_std_path(),
                epoch,
            }))
            .spawn()
            .await
            .unwrap();

        let max_seen = service.sweep_orphans_with_store(&store).await.unwrap();

        assert_eq!(max_seen, epoch);
        assert_eq!(
            service.next_epoch.fetch_add(1, Ordering::Relaxed),
            epoch + 1
        );
        assert!(!pid_path.exists());
        assert!(!config_path.exists());
        let _ = handle.wait().await.unwrap();

        let _ = tokio::fs::remove_dir_all(dir).await;
    }

    #[tokio::test]
    async fn v2_startup_orphan_sweep_rejects_unverifiable_pid_record() {
        let service = service();
        let epoch = 22;
        let dir = std::env::temp_dir().join(format!(
            "chimera-orphan-sweep-unverified-{}-{}",
            std::process::id(),
            OPERATION_ID
        ));
        let store = super::RuntimeConfigStore::open(dir.clone()).await.unwrap();
        let staged = store.stage(epoch, b"mode: rule\n").await.unwrap();
        let commit = store.commit_new(staged, epoch).await.unwrap();
        let (config_path, _warning) = commit.into_parts();
        let pid_path = super::epoch_pid_path(store.dir(), epoch);
        tokio::fs::write(&pid_path, b"12345\n").await.unwrap();

        let error = service.sweep_orphans_with_store(&store).await.unwrap_err();

        assert!(error.to_string().contains("malformed epoch pid record"));
        assert!(pid_path.exists());
        assert!(config_path.exists());

        let _ = tokio::fs::remove_dir_all(dir).await;
    }

    #[tokio::test]
    async fn v2_epoch_allocator_advances_past_existing_runtime_artifacts() {
        let service = service();
        let dir = std::env::temp_dir().join(format!(
            "chimera-epoch-seed-{}-{}",
            std::process::id(),
            OPERATION_ID
        ));
        let store = super::RuntimeConfigStore::open(dir.clone()).await.unwrap();
        let staged = store.stage(9, b"mode: rule\n").await.unwrap();
        store.commit_new(staged, 9).await.unwrap();
        tokio::fs::write(store.dir().join("core-12.pid"), b"stale")
            .await
            .unwrap();

        service.sync_epoch_allocator(&store).await.unwrap();

        assert_eq!(service.next_epoch.fetch_add(1, Ordering::Relaxed), 13);
        let _ = tokio::fs::remove_dir_all(dir).await;
    }

    #[test]
    fn reconcile_revision_preserves_epoch_for_patch_reload_restart_and_allocates_for_switch() {
        let service = service();
        let current = ConfigRevisionInfo {
            epoch: 9,
            generation: 4,
            source_hash: "old".into(),
            effective_hash: "old".into(),
        };

        let noop = service
            .revision_for_plan(ReconcilePlan::Noop, Some(&current), "same")
            .unwrap();
        assert_eq!(noop, current);

        let patched = service
            .revision_for_plan(ReconcilePlan::Patch, Some(&current), "patch")
            .unwrap();
        assert_eq!(patched.epoch, 9);
        assert_eq!(patched.generation, 5);
        assert_eq!(patched.source_hash, "patch");
        assert_eq!(service.next_epoch.load(Ordering::Relaxed), 1);

        let reloaded = service
            .revision_for_plan(ReconcilePlan::Reload, Some(&current), "reload")
            .unwrap();
        assert_eq!(reloaded.epoch, 9);
        assert_eq!(reloaded.generation, 5);
        assert_eq!(reloaded.source_hash, "reload");
        assert_eq!(service.next_epoch.load(Ordering::Relaxed), 1);

        let restarted = service
            .revision_for_plan(ReconcilePlan::Restart, Some(&current), "next")
            .unwrap();
        assert_eq!(restarted.epoch, 9);
        assert_eq!(restarted.generation, 5);
        assert_eq!(restarted.source_hash, "next");
        assert_eq!(service.next_epoch.load(Ordering::Relaxed), 1);

        let switched = service
            .revision_for_plan(ReconcilePlan::Switch, Some(&current), "switch")
            .unwrap();
        assert_eq!(switched.epoch, 1);
        assert_eq!(switched.generation, 1);
        assert_eq!(service.next_epoch.load(Ordering::Relaxed), 2);

        let cold = service
            .revision_for_plan(ReconcilePlan::Restart, None, "cold")
            .unwrap();
        assert_eq!(cold.epoch, 2);
        assert_eq!(cold.generation, 1);
        assert_eq!(service.next_epoch.load(Ordering::Relaxed), 3);
    }

    #[test]
    fn controller_binding_maps_all_transports_to_shared_client() {
        let http = controller_client(&CoreApiConnection {
            instance_id: "http".into(),
            controller: CoreControllerInfo::Http("http://127.0.0.1:9090".into()),
            secret: Some("secret".into()),
        })
        .unwrap();
        assert!(matches!(http.host(), clash_api::Host::Http(_)));

        let unix = controller_client(&CoreApiConnection {
            instance_id: "unix".into(),
            controller: CoreControllerInfo::UnixSocket("/tmp/chimera.sock".into()),
            secret: None,
        })
        .unwrap();
        assert!(matches!(
            unix.host(),
            clash_api::Host::UnixSocket(path) if path == std::path::Path::new("/tmp/chimera.sock")
        ));

        let pipe = controller_client(&CoreApiConnection {
            instance_id: "pipe".into(),
            controller: CoreControllerInfo::NamedPipe("chimera-pipe".into()),
            secret: None,
        })
        .unwrap();
        assert!(matches!(
            pipe.host(),
            clash_api::Host::NamedPipe(path) if path == std::path::Path::new("chimera-pipe")
        ));
    }

    #[test]
    fn api_binding_is_derived_from_applied_config() {
        let revision = ConfigRevisionInfo {
            epoch: 4,
            generation: 2,
            source_hash: "source".to_string(),
            effective_hash: "effective".to_string(),
        };
        let connection = api_connection_from_config(
            "external-controller: 127.0.0.1:9090\nsecret: 'token-value'\nmode: rule\n",
            &revision,
        )
        .expect("controller should produce a binding");
        assert_eq!(
            connection,
            CoreApiConnection {
                instance_id: "00000000000000040000000000000002".to_string(),
                controller: CoreControllerInfo::Http("http://127.0.0.1:9090".to_string()),
                secret: Some("token-value".to_string()),
            }
        );
        assert!(api_connection_from_config("mode: rule\n", &revision).is_none());
    }

    fn stop_request() -> CoreSubmitReq<'static> {
        CoreSubmitReq {
            operation_id: Cow::Borrowed(OPERATION_ID),
            command: CoreCommandInfo::Stop,
        }
    }

    #[test]
    fn v2_concurrent_same_id_admission_registers_exactly_once() {
        let service = service();
        let barrier = Arc::new(std::sync::Barrier::new(16));
        let handles = (0..16)
            .map(|_| {
                let service = service.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    service
                        .admit_operation(OPERATION_ID, "same-command".into())
                        .expect("same command should attach or register")
                })
            })
            .collect::<Vec<_>>();

        let mut registered = 0;
        let mut existing = 0;
        for handle in handles {
            match handle.join().expect("admission thread should not panic") {
                OperationAdmission::Registered { info, .. } => {
                    registered += 1;
                    assert_eq!(info.id, OPERATION_ID);
                }
                OperationAdmission::Existing(info) => {
                    existing += 1;
                    assert_eq!(info.id, OPERATION_ID);
                }
            }
        }

        assert_eq!(registered, 1);
        assert_eq!(existing, 15);
        let operations = service.operations.lock();
        assert_eq!(operations.records.len(), 1);
        assert_eq!(operations.order.len(), 1);
        assert_eq!(
            operations.order.front().map(String::as_str),
            Some(OPERATION_ID)
        );
    }

    #[tokio::test]
    async fn v2_stop_is_durable_idempotent_and_queryable() {
        let service = service();
        let request = stop_request();

        let admitted = service.submit_v2(&request).await.unwrap();
        assert_eq!(admitted.id, OPERATION_ID);

        let attached = service.submit_v2(&request).await.unwrap();
        assert_eq!(attached.id, OPERATION_ID);

        let terminal = service
            .operation_v2(&CoreOperationReq {
                operation_id: Cow::Borrowed(OPERATION_ID),
                wait_ms: Some(1_000),
            })
            .await
            .unwrap();
        assert_eq!(terminal.phase, OperationPhase::Succeeded);
        assert_eq!(terminal.output, Some(OperationOutputInfo::Stopped));
        assert_eq!(service.operation_snapshot(OPERATION_ID), Some(terminal));
    }

    #[tokio::test]
    async fn v2_recover_is_idempotent_without_applied_runtime() {
        let service = service();
        let request = CoreSubmitReq {
            operation_id: Cow::Borrowed(OPERATION_ID),
            command: CoreCommandInfo::Recover,
        };

        service.submit_v2(&request).await.unwrap();
        let terminal = service
            .operation_v2(&CoreOperationReq {
                operation_id: Cow::Borrowed(OPERATION_ID),
                wait_ms: Some(1_000),
            })
            .await
            .unwrap();
        assert_eq!(terminal.phase, OperationPhase::Succeeded);
        assert_eq!(terminal.output, Some(OperationOutputInfo::Recovered));
        let status = service.status().await;
        assert!(matches!(status.state, CoreState::Stopped(_)));
        assert!(status.revision.is_none());
    }

    #[tokio::test]
    async fn v2_reconcile_digest_mismatch_fails_before_mutation() {
        let service = service();
        let request = CoreSubmitReq {
            operation_id: Cow::Borrowed(OPERATION_ID),
            command: CoreCommandInfo::Reconcile {
                core_type: Cow::Owned(chimera_utils::core::CoreType::Clash(
                    chimera_utils::core::ClashCoreType::Mihomo,
                )),
                config: Cow::Borrowed("mode: rule\n"),
                expected_digest: Some(Cow::Borrowed("0000000000000000")),
                expected_applied: None,
            },
        };

        service.submit_v2(&request).await.unwrap();
        let terminal = service
            .operation_v2(&CoreOperationReq {
                operation_id: Cow::Borrowed(OPERATION_ID),
                wait_ms: Some(1_000),
            })
            .await
            .unwrap();
        assert_eq!(terminal.phase, OperationPhase::Failed);
        assert!(terminal.error.as_ref().is_some_and(|error| {
            error.kind.as_deref() == Some(CoreErrorKind::InvalidConfig.as_str())
                && !error.retryable
                && error.message.contains("config digest mismatch")
        }));
        let status = service.status().await;
        assert!(matches!(status.state, CoreState::Stopped(_)));
        assert!(status.revision.is_none());
    }

    #[tokio::test]
    async fn v2_reconcile_stale_cas_fails_before_mutation() {
        let service = service();
        let config = "mode: rule\n";
        let digest = payload_digest(config.as_bytes());
        let request = CoreSubmitReq {
            operation_id: Cow::Borrowed(OPERATION_ID),
            command: CoreCommandInfo::Reconcile {
                core_type: Cow::Owned(chimera_utils::core::CoreType::Clash(
                    chimera_utils::core::ClashCoreType::Mihomo,
                )),
                config: Cow::Borrowed(config),
                expected_digest: Some(Cow::Owned(digest)),
                expected_applied: Some(RevisionIdInfo {
                    epoch: 9,
                    generation: 1,
                    effective_hash: "deadbeefdeadbeef".to_string(),
                }),
            },
        };

        service.submit_v2(&request).await.unwrap();
        let terminal = service
            .operation_v2(&CoreOperationReq {
                operation_id: Cow::Borrowed(OPERATION_ID),
                wait_ms: Some(1_000),
            })
            .await
            .unwrap();
        assert_eq!(terminal.phase, OperationPhase::Failed);
        assert!(terminal.error.as_ref().is_some_and(|error| {
            error.kind.as_deref() == Some(CoreErrorKind::RevisionConflict.as_str())
                && !error.retryable
                && error.message.contains("revision conflict")
        }));
        let status = service.status().await;
        assert!(matches!(status.state, CoreState::Stopped(_)));
        assert!(status.revision.is_none());
    }

    #[tokio::test]
    async fn v2_quarantined_reconcile_reports_typed_terminal_error() {
        let service = service();
        service.latch_quarantine(77, "simulated uncertain process");

        let request = CoreSubmitReq {
            operation_id: Cow::Borrowed(OPERATION_ID),
            command: CoreCommandInfo::Reconcile {
                core_type: Cow::Owned(chimera_utils::core::CoreType::Clash(
                    chimera_utils::core::ClashCoreType::Mihomo,
                )),
                config: Cow::Borrowed("mode: rule\n"),
                expected_digest: None,
                expected_applied: None,
            },
        };

        service.submit_v2(&request).await.unwrap();
        let terminal = service
            .operation_v2(&CoreOperationReq {
                operation_id: Cow::Borrowed(OPERATION_ID),
                wait_ms: Some(1_000),
            })
            .await
            .unwrap();

        assert_eq!(terminal.phase, OperationPhase::Failed);
        assert!(terminal.error.as_ref().is_some_and(|error| {
            error.kind.as_deref() == Some(CoreErrorKind::Quarantined.as_str())
                && !error.retryable
                && error.message.contains("quarantined")
        }));
    }

    #[tokio::test]
    async fn v2_quarantine_recovery_requires_authoritative_epoch_pid_record() {
        let service = service();
        service.latch_quarantine(
            77,
            "desired runtime failed to start; process death is uncertain",
        );

        let status = service.status().await;
        assert!(matches!(
            status.state,
            CoreState::Stopped(Some(ref reason))
                if reason.contains("epoch 77") && reason.contains("death is uncertain")
        ));

        let dir = std::env::temp_dir().join(format!(
            "chimera-quarantine-missing-pid-{}-{}",
            std::process::id(),
            OPERATION_ID
        ));
        let store = super::RuntimeConfigStore::open(dir.clone()).await.unwrap();
        let recover_error = service
            .recover_quarantine_with_store(&store)
            .await
            .unwrap_err();
        assert!(
            recover_error
                .to_string()
                .contains("authoritative epoch pid record is unavailable")
        );

        let stopped = service.execute_v2(CoreCommandInfo::Stop).await.unwrap();
        assert_eq!(stopped, OperationOutputInfo::Stopped);
        assert!(!service.quarantine.lock().is_empty());

        let _ = tokio::fs::remove_dir_all(dir).await;
    }

    #[tokio::test]
    async fn v2_quarantine_recovery_reaps_identity_verified_epoch_process() {
        use nyanpasu_utils::process::{
            Command, EpochPidFile, EpochPidFileSpec, read_epoch_pid_file,
        };

        let service = service();
        let epoch = 88;
        let dir = std::env::temp_dir().join(format!(
            "chimera-quarantine-owned-{}-{}",
            std::process::id(),
            OPERATION_ID
        ));
        let store = super::RuntimeConfigStore::open(dir.clone()).await.unwrap();
        let staged = store.stage(epoch, b"mode: rule\n").await.unwrap();
        let commit = store.commit_new(staged, epoch).await.unwrap();
        let (config_path, warning) = commit.into_parts();
        assert!(warning.is_none());
        let pid_path = super::epoch_pid_path(store.dir(), epoch);

        #[cfg(windows)]
        let command = Command::new("ping.exe").args(["-n", "30", "127.0.0.1"]);
        #[cfg(unix)]
        let command = Command::new("/bin/sleep").arg("30");

        let (handle, mut events) = command
            .epoch_pid_file(EpochPidFile::new(EpochPidFileSpec {
                pid_path: pid_path.as_std_path(),
                runtime_config: config_path.as_std_path(),
                epoch,
            }))
            .spawn()
            .await
            .unwrap();
        let record = read_epoch_pid_file(pid_path.as_std_path())
            .await
            .unwrap()
            .expect("structured epoch pid record");
        assert_eq!(record.pid, handle.pid());
        assert_eq!(record.epoch, epoch);
        assert_eq!(record.runtime_config, config_path.as_std_path());

        service.latch_quarantine(epoch, "simulated uncertain process");
        service.recover_quarantine_with_store(&store).await.unwrap();

        assert!(service.quarantine.lock().is_empty());
        assert!(!pid_path.exists());
        assert!(!config_path.exists());
        let terminated = handle.wait().await.unwrap();
        assert!(terminated.code.is_some() || terminated.signal.is_some());
        while events.recv().await.is_some() {}

        let _ = tokio::fs::remove_dir_all(dir).await;
    }

    #[test]
    fn v2_operation_conflict_envelope_carries_kind_and_retryable() {
        let envelope: chimera_ipc::api::R<'static, Option<()>> = super::OpError::with_kind(
            CoreErrorKind::OperationConflict,
            "operation conflict: id already exists with a different command",
        )
        .retryable(false)
        .into_envelope();

        assert_eq!(
            envelope.error_kind.as_deref(),
            Some(CoreErrorKind::OperationConflict.as_str())
        );
        assert_eq!(envelope.retryable, Some(false));
        assert!(envelope.msg.contains("operation conflict"));
    }

    #[tokio::test]
    async fn v2_operation_id_validation_and_conflict_fail_closed() {
        let service = service();
        let invalid = CoreSubmitReq {
            operation_id: Cow::Borrowed("not-hex"),
            command: CoreCommandInfo::Stop,
        };
        assert!(service.submit_v2(&invalid).await.is_err());

        let request = stop_request();
        service.submit_v2(&request).await.unwrap();
        service
            .operations
            .lock()
            .records
            .get_mut(OPERATION_ID)
            .unwrap()
            .fingerprint = "different-command".to_string();

        let error = service.submit_v2(&request).await.unwrap_err();
        assert_eq!(error.kind(), Some(CoreErrorKind::OperationConflict));
        assert_eq!(error.retryable_hint(), Some(false));
        assert!(error.to_string().contains("operation conflict"));
    }
}

// TODO: support system path search via a config or flag
/// Search the binary path of the core: Data Dir -> Sidecar Dir
pub fn find_binary_path(core_type: &CoreType) -> std::io::Result<PathBuf> {
    let infos = consts::RuntimeInfos::global();
    let data_dir = &infos.nyanpasu_data_dir;
    let binary_path = data_dir.join(core_type.get_executable_name());
    if binary_path.exists() {
        return Ok(binary_path);
    }
    let app_dir = &infos.nyanpasu_app_dir;
    let binary_path = app_dir.join(core_type.get_executable_name());
    if binary_path.exists() {
        return Ok(binary_path);
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::NotFound,
        format!("{} not found", core_type.get_executable_name()),
    ))
}
