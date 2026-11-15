//! Host DNS override as an orchestrator-owned component (amendment A5 ③):
//! applied and restored at fixed phases of the core lifecycle transaction,
//! never by the app process.
//!
//! Invariants (audit §3):
//!
//! - The record is persisted *before* the side effect, so a crash between the
//!   two leaves an orphan record whose restore is a read-back no-op.
//! - Apply and restore both advance by read-back: an `Err` from the platform
//!   command never implies the side effect is absent.
//! - The manager converges DNS at the tail of every mutating transaction
//!   (apply when a runtime is up and wants an override, restore otherwise) and
//!   restores at the head of user-initiated stop/shutdown, so resolution never
//!   points at a core that is being torn down.

use serde::{Deserialize, Serialize};
use serde_yaml_ng::Mapping;

use crate::{Epoch, runtime::BoxFuture};

/// The override one host should hold while a given effective config runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DnsIntent {
    /// Interface-scoped resolver addresses, in platform-native spelling.
    pub servers: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DnsOverrideState {
    /// Recorded, side effect in flight or in place.
    Applied,
    /// Restore started but not yet read back as clean.
    RestorePending,
}

/// The persisted proof-of-ownership for one applied override.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DnsOverrideRecord {
    pub interface: String,
    /// Read-back before the apply; restore returns the interface here.
    pub previous: Vec<String>,
    pub applied: Vec<String>,
    pub runtime_epoch: u64,
    /// `ControllerGeneration` when a host layer supplies one; opaque here.
    pub owner_generation: Option<u64>,
    pub state: DnsOverrideState,
}

#[derive(Debug, thiserror::Error)]
pub enum DnsError {
    #[error("dns command failed: {0}")]
    Command(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// Platform DNS mechanics behind one narrow boundary. A host without DNS
/// responsibility simply injects no controller.
///
/// **Contract limit (audit L8).** This shape supports *structurally owned*
/// overrides only: ones whose restore removes an artifact the controller owns
/// and therefore does not replay `record.previous`. The orchestrator persists
/// its pre-record before the first `apply`, and on that first call the record
/// has no baseline yet — nothing has read one back. A write-back controller
/// (`networksetup -setdnsservers` and friends), whose restore *is* replaying
/// the remembered value, would be unrecoverable in exactly that window: side
/// effect landed, `apply` never returned, nothing recorded to write back.
///
/// Adding one therefore requires splitting `apply` into prepare (read back the
/// baseline, hand it to the orchestrator to persist) and commit, which is the
/// L8 migration. Until then, do not implement this trait with a write-back
/// mechanism.
///
/// Each call is bounded by
/// [`ManagerOptions::dns_timeout`](crate::spec::ManagerOptions::dns_timeout).
/// An implementation must therefore be cancel-safe in the weak sense the
/// orchestrator relies on: a dropped future may leave the side effect in place,
/// and it is treated as uncertain rather than absent.
pub trait DnsController: Send + Sync {
    /// The override this host should hold while `effective` runs, or `None`.
    ///
    /// The derivation is controller policy, not orchestrator policy — the
    /// orchestrator only knows the fixed phases.
    fn desired(&self, effective: &Mapping) -> Option<DnsIntent>;

    /// Applies `intent` and reads the result back. Must be idempotent: the
    /// converge tail calls this after every mutating transaction. Returns the
    /// record to persist (with `previous` captured by read-back).
    fn apply<'a>(
        &'a self,
        intent: &'a DnsIntent,
        runtime_epoch: Epoch,
    ) -> BoxFuture<'a, Result<DnsOverrideRecord, DnsError>>;

    /// Restores `record.previous` and reads the result back. Also the orphan
    /// reconcile path at manager construction — restore of a record whose
    /// side effect never landed must read back as an immediate no-op.
    fn restore<'a>(&'a self, record: &'a DnsOverrideRecord) -> BoxFuture<'a, Result<(), DnsError>>;
}

#[cfg(any(target_os = "macos", test))]
fn first_resolver_uses_loopback(output: &str) -> bool {
    let mut in_first_resolver = false;

    for line in output.lines() {
        let line = line.trim();
        if line.starts_with("resolver #") {
            if in_first_resolver {
                break;
            }
            in_first_resolver = true;
            continue;
        }

        if in_first_resolver && line.starts_with("nameserver[") {
            return line
                .split_once(':')
                .is_some_and(|(_, address)| address.trim() == "127.0.0.1");
        }
    }

    false
}

#[cfg(test)]
mod resolver_output_tests {
    use super::first_resolver_uses_loopback;

    #[test]
    fn accepts_loopback_as_the_first_resolver() {
        let output =
            "resolver #1\n  nameserver[0] : 127.0.0.1\nresolver #2\n  nameserver[0] : 192.0.2.53\n";
        assert!(first_resolver_uses_loopback(output));
    }

    #[test]
    fn rejects_loopback_only_in_a_later_resolver() {
        let output =
            "resolver #1\n  nameserver[0] : 192.0.2.53\nresolver #2\n  nameserver[0] : 127.0.0.1\n";
        assert!(!first_resolver_uses_loopback(output));
    }

    #[test]
    fn rejects_missing_or_serverless_first_resolver() {
        assert!(!first_resolver_uses_loopback("DNS configuration\n"));
        assert!(!first_resolver_uses_loopback(
            "resolver #1\n  search domain[0] : example.test\nresolver #2\n  nameserver[0] : 127.0.0.1\n"
        ));
    }
}

/// Local macOS DNS integration: when the core DNS service listens on port 53,
/// point the host resolver at its loopback listener and verify resolver adoption.
///
/// A `scutil` dynamic-store `State:` key gives the controller a dedicated
/// artifact to remove, but writing and reading the key does not prove that
/// macOS adopted it as a resolver. `apply` therefore checks `scutil --dns` and
/// rejects the activation unless the first resolver is the loopback listener.
/// This local mechanism remains subject to real-host validation; the pinned
/// reference does not provide an equivalent host DNS override.
///
/// Phase-0 acceptance criteria (machine-checkable, run on macOS):
/// 1. after writing the key, `scutil --dns` lists the override as the first
///    resolver;
/// 2. after deleting the key, `scutil --dns` shows the pre-override resolver;
/// 3. after a reboot with the key left in place, the key does not exist.
#[cfg(target_os = "macos")]
pub mod macos {
    use super::*;
    use serde_yaml_ng::Value;

    const DNS_RESOLVER_SETTLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);
    const DNS_RESOLVER_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(100);

    pub struct MacosDnsController {
        /// The dynamic-store key this controller owns, e.g.
        /// `State:/Network/Service/nyanpasu-dns/DNS`.
        store_key: String,
        authorization_failed: std::sync::atomic::AtomicBool,
    }

    impl MacosDnsController {
        pub fn new(store_key: String) -> Self {
            Self {
                store_key,
                authorization_failed: std::sync::atomic::AtomicBool::new(false),
            }
        }

        async fn scutil(&self, script: String, mutating: bool) -> Result<String, DnsError> {
            use tokio::io::AsyncWriteExt;
            let elevated = mutating && !chimera_utils::os::is_elevated();
            if elevated
                && self
                    .authorization_failed
                    .load(std::sync::atomic::Ordering::Relaxed)
            {
                return Err(DnsError::Command(
                    "macOS DNS authorization previously failed; restart Chimera to retry".into(),
                ));
            }
            let mut command = if elevated {
                // Leave this latched on cancellation, timeout, or failure so
                // background recovery cannot keep requesting authorization.
                self.authorization_failed
                    .store(true, std::sync::atomic::Ordering::Relaxed);
                let mut command = tokio::process::Command::new("/usr/bin/osascript");
                command.args(["-e", &authorization_script(&script)]);
                command
            } else {
                tokio::process::Command::new("/usr/sbin/scutil")
            };
            let mut child = command
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                // The orchestrator bounds this call and drops the future on
                // timeout; without this the child would outlive it.
                .kill_on_drop(true)
                .spawn()?;
            if !elevated {
                child
                    .stdin
                    .as_mut()
                    .expect("stdin was piped")
                    .write_all(script.as_bytes())
                    .await?;
            }
            let output = child.wait_with_output().await?;
            if !output.status.success() {
                if elevated {
                    // Recovery runs in the background: do not repeatedly
                    // reopen a password dialog after denial or cancellation.
                    self.authorization_failed
                        .store(true, std::sync::atomic::Ordering::Relaxed);
                }
                return Err(DnsError::Command(
                    String::from_utf8_lossy(&output.stderr).into_owned(),
                ));
            }
            let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
            // scutil reports dynamic-store errors on stdout even when its
            // process exits successfully. A write normally produces no text.
            validate_scutil_output(&stdout, mutating)?;
            if elevated {
                self.authorization_failed
                    .store(false, std::sync::atomic::Ordering::Relaxed);
            }
            Ok(stdout)
        }

        /// Read-back: does the owned key currently exist with `servers`?
        async fn read_back(&self) -> Result<Option<Vec<String>>, DnsError> {
            let output = self
                .scutil(format!("show {}\n", self.store_key), false)
                .await?;
            if output.contains("No such key") {
                return Ok(None);
            }
            if !output.contains("<dictionary>") {
                return Err(DnsError::Command(output.trim().to_owned()));
            }
            let servers = output
                .lines()
                .filter_map(|line| {
                    let trimmed = line.trim();
                    trimmed
                        .split_once(':')
                        .filter(|(key, _)| key.trim().chars().all(|c| c.is_ascii_digit()))
                        .map(|(_, value)| value.trim().to_owned())
                })
                .collect();
            Ok(Some(servers))
        }

        async fn system_resolver_uses_loopback(&self) -> Result<bool, DnsError> {
            let output = tokio::process::Command::new("/usr/sbin/scutil")
                .arg("--dns")
                .kill_on_drop(true)
                .output()
                .await?;
            if !output.status.success() {
                return Err(DnsError::Command(format!(
                    "scutil --dns failed: {}",
                    String::from_utf8_lossy(&output.stderr).trim()
                )));
            }
            Ok(super::first_resolver_uses_loopback(
                &String::from_utf8_lossy(&output.stdout),
            ))
        }

        async fn wait_for_system_resolver(&self) -> Result<bool, DnsError> {
            let deadline = tokio::time::Instant::now() + DNS_RESOLVER_SETTLE_TIMEOUT;
            loop {
                if tokio::time::Instant::now() >= deadline {
                    return Ok(false);
                }
                match tokio::time::timeout_at(deadline, self.system_resolver_uses_loopback()).await
                {
                    Ok(Ok(true)) => return Ok(true),
                    Ok(Ok(false)) => {}
                    Ok(Err(error)) => return Err(error),
                    Err(_) => {
                        return Err(DnsError::Command(
                            "scutil --dns timed out while checking the system resolver".into(),
                        ));
                    }
                }
                let next_poll = std::cmp::min(
                    deadline,
                    tokio::time::Instant::now() + DNS_RESOLVER_POLL_INTERVAL,
                );
                tokio::time::sleep_until(next_poll).await;
            }
        }

        async fn remove_override(&self) -> Result<(), DnsError> {
            if self.read_back().await?.is_none() {
                return Ok(());
            }
            self.scutil(format!("remove {}\n", self.store_key), true)
                .await?;
            if self.read_back().await?.is_some() {
                return Err(DnsError::Command(
                    "read-back still shows the override after removal".into(),
                ));
            }
            Ok(())
        }
    }

    impl DnsController for MacosDnsController {
        fn desired(&self, effective: &Mapping) -> Option<DnsIntent> {
            desired_dns_intent(effective)
        }

        fn apply<'a>(
            &'a self,
            intent: &'a DnsIntent,
            runtime_epoch: Epoch,
        ) -> BoxFuture<'a, Result<DnsOverrideRecord, DnsError>> {
            Box::pin(async move {
                let previous = self.read_back().await?.unwrap_or_default();
                let addresses = intent.servers.join(" ");
                if previous != intent.servers {
                    self.scutil(
                        format!(
                            "d.init\nd.add ServerAddresses * {addresses}\nset {}\n",
                            self.store_key
                        ),
                        true,
                    )
                    .await?;
                }
                let observed = self.read_back().await?;
                if observed.as_deref() != Some(intent.servers.as_slice()) {
                    return Err(DnsError::Command(format!(
                        "read-back mismatch after apply: {observed:?}"
                    )));
                }
                let resolver_check = self.wait_for_system_resolver().await;
                let resolver_status = match &resolver_check {
                    Ok(true) => None,
                    Ok(false) => {
                        Some("the system resolver did not select 127.0.0.1 first".to_owned())
                    }
                    Err(error) => Some(format!("could not verify the system resolver: {error}")),
                };
                if let Some(resolver_status) = resolver_status {
                    let cleanup = self.remove_override().await;
                    let cleanup_status = match cleanup {
                        Ok(()) => "the unverified override was removed".to_owned(),
                        Err(error) => format!("failed to remove the unverified override: {error}"),
                    };
                    return Err(DnsError::Command(format!(
                        "scutil accepted the DNS key, but {resolver_status}; {cleanup_status}"
                    )));
                }
                Ok(DnsOverrideRecord {
                    interface: self.store_key.clone(),
                    previous,
                    applied: intent.servers.clone(),
                    runtime_epoch: runtime_epoch.get(),
                    owner_generation: None,
                    state: DnsOverrideState::Applied,
                })
            })
        }

        fn restore<'a>(
            &'a self,
            _record: &'a DnsOverrideRecord,
        ) -> BoxFuture<'a, Result<(), DnsError>> {
            Box::pin(self.remove_override())
        }
    }

    fn authorization_script(script: &str) -> String {
        let quoted = format!("'{}'", script.replace('\'', "'\\''"));
        let shell = format!("/usr/bin/printf '%s' {quoted} | /usr/sbin/scutil");
        let escaped = shell.replace('\\', "\\\\").replace('"', "\\\"");
        format!("do shell script \"{escaped}\" with administrator privileges")
    }

    fn validate_scutil_output(stdout: &str, mutating: bool) -> Result<(), DnsError> {
        if mutating && !stdout.trim().is_empty() {
            return Err(DnsError::Command(stdout.trim().to_owned()));
        }
        Ok(())
    }

    #[cfg(test)]
    mod authorization_tests {
        use super::*;

        #[test]
        fn successful_exit_cannot_hide_dynamic_store_permission_failure() {
            let error = validate_scutil_output("  Access denied\n", true).unwrap_err();
            assert_eq!(error.to_string(), "dns command failed: Access denied");
            assert!(validate_scutil_output("\n", true).is_ok());
        }

        #[test]
        fn dns_script_is_literal_shell_data_inside_applescript() {
            let script = authorization_script("show State:/Test/it's\n");
            assert!(script.contains("it'\\\\''s"));
            assert!(script.ends_with("with administrator privileges"));
            assert!(script.contains("/usr/sbin/scutil"));
        }
    }

    /// Whether the effective config exposes the DNS listener expected by the
    /// reference macOS resolver flow.
    /// Kept independent of the controller instance so the manager can reject
    /// configs that require the host DNS integration if it was omitted.
    pub fn desired_dns_intent(effective: &Mapping) -> Option<DnsIntent> {
        let dns = effective.get("dns")?.as_mapping()?;
        if dns.get("enable")?.as_bool()? != true {
            return None;
        }
        let listen = dns.get(Value::String("listen".into()))?.as_str()?;
        let (_, port) = listen.rsplit_once(':')?;
        (port == "53").then(|| DnsIntent {
            servers: vec!["127.0.0.1".to_owned()],
        })
    }

    #[cfg(test)]
    mod desired_tests {
        use super::*;

        #[test]
        fn enabled_dns_listener_on_port_53_uses_loopback_resolver() {
            let config: Mapping =
                serde_yaml_ng::from_str("dns:\n  enable: true\n  listen: 0.0.0.0:53\n").unwrap();
            assert_eq!(
                desired_dns_intent(&config).unwrap().servers,
                vec!["127.0.0.1"]
            );
        }

        #[test]
        fn disabled_or_non_53_dns_listener_does_not_override_system_dns() {
            for config in [
                "dns:\n  enable: false\n  listen: 0.0.0.0:53\n",
                "dns:\n  enable: true\n  listen: 127.0.0.1:1053\n",
            ] {
                let config: Mapping = serde_yaml_ng::from_str(config).unwrap();
                assert!(desired_dns_intent(&config).is_none());
            }
        }
    }
}
