use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use camino::{Utf8Path, Utf8PathBuf};
use nyanpasu_core_manager::{
    ApplyOutcome, CoreKind, CoreManager, CoreSpec, CoreState, DnsController, DnsError, DnsIntent,
    DnsOverrideRecord, DnsOverrideState, Error, InstanceOptions, InstanceSpec, ManagerOptions,
    RevisionId, runtime::BoxFuture,
};

struct FakeDns {
    applied: Mutex<Vec<(DnsIntent, u64)>>,
    restored: Mutex<Vec<DnsOverrideRecord>>,
    current: Mutex<Vec<String>>,
    fail_apply: AtomicBool,
}

impl Default for FakeDns {
    fn default() -> Self {
        Self {
            applied: Mutex::new(Vec::new()),
            restored: Mutex::new(Vec::new()),
            current: Mutex::new(vec!["10.0.0.1".into()]),
            fail_apply: AtomicBool::new(false),
        }
    }
}

impl DnsController for FakeDns {
    fn desired(&self, _effective: &serde_yaml_ng::Mapping) -> Option<DnsIntent> {
        Some(DnsIntent {
            servers: vec!["198.18.0.2".into()],
        })
    }

    fn apply<'a>(
        &'a self,
        intent: &'a DnsIntent,
        runtime_epoch: u64,
    ) -> BoxFuture<'a, Result<DnsOverrideRecord, DnsError>> {
        Box::pin(async move {
            if self.fail_apply.load(Ordering::SeqCst) {
                return Err(DnsError::Command("injected apply failure".into()));
            }
            let previous =
                std::mem::replace(&mut *self.current.lock().unwrap(), intent.servers.clone());
            self.applied
                .lock()
                .unwrap()
                .push((intent.clone(), runtime_epoch));
            Ok(DnsOverrideRecord {
                interface: "fake-interface".into(),
                previous,
                applied: intent.servers.clone(),
                runtime_epoch,
                owner_generation: None,
                state: DnsOverrideState::Applied,
            })
        })
    }

    fn restore<'a>(&'a self, record: &'a DnsOverrideRecord) -> BoxFuture<'a, Result<(), DnsError>> {
        Box::pin(async move {
            self.restored.lock().unwrap().push(record.clone());
            *self.current.lock().unwrap() = record.previous.clone();
            Ok(())
        })
    }
}

fn free_controller_address() -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    drop(listener);
    address.to_string()
}

fn fake_core_spec(root: &Utf8Path, config_path: Utf8PathBuf) -> InstanceSpec {
    InstanceSpec {
        core: CoreSpec {
            kind: CoreKind::Mihomo,
            binary_path: Utf8PathBuf::from(env!("CARGO_BIN_EXE_nyanpasu-fake-core")),
            version: Some("v1.18.9".into()),
            features: Vec::new(),
        },
        config_path,
        working_dir: root.to_owned(),
        pid_file: None,
        options: InstanceOptions {
            startup_timeout: Duration::from_secs(5),
            ..InstanceOptions::default()
        },
    }
}

async fn manager_with_dns(root: &Utf8Path, dns: Arc<FakeDns>) -> CoreManager {
    CoreManager::builder(ManagerOptions {
        runtime_dir: Some(root.join("runtime")),
        ..ManagerOptions::default()
    })
    .dns_controller(dns)
    .build()
    .await
    .unwrap()
}

fn record_path(root: &Utf8Path) -> Utf8PathBuf {
    root.join("runtime").join("dns-override.json")
}

#[tokio::test]
async fn reconcile_applies_override_and_stop_restores_it() {
    let temp = tempfile::tempdir().unwrap();
    let root = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();
    let config = root.join("config.yaml");
    tokio::fs::write(
        &config,
        format!("external-controller: {}\n", free_controller_address()),
    )
    .await
    .unwrap();
    let spec = fake_core_spec(&root, config);
    let dns = Arc::new(FakeDns::default());
    let manager = manager_with_dns(&root, dns.clone()).await;

    let outcome = manager.reconcile(spec, None).await.unwrap();
    assert!(matches!(outcome, ApplyOutcome::Started { .. }));
    assert_eq!(dns.applied.lock().unwrap().len(), 1);
    let record: DnsOverrideRecord =
        serde_json::from_slice(&std::fs::read(record_path(&root)).unwrap()).unwrap();
    assert_eq!(record.previous, vec!["10.0.0.1"]);
    assert_eq!(record.state, DnsOverrideState::Applied);

    manager.stop().await.unwrap();
    assert_eq!(dns.restored.lock().unwrap().len(), 1);
    assert_eq!(*dns.current.lock().unwrap(), vec!["10.0.0.1"]);
    assert!(!record_path(&root).exists());

    manager.shutdown().await.unwrap();
}

#[tokio::test]
async fn orphan_record_is_restored_during_construction() {
    let temp = tempfile::tempdir().unwrap();
    let root = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();
    let runtime = root.join("runtime");
    std::fs::create_dir_all(&runtime).unwrap();
    let orphan = DnsOverrideRecord {
        interface: "fake-interface".into(),
        previous: vec!["10.0.0.1".into()],
        applied: vec!["198.18.0.2".into()],
        runtime_epoch: 7,
        owner_generation: None,
        state: DnsOverrideState::Applied,
    };
    std::fs::write(record_path(&root), serde_json::to_vec(&orphan).unwrap()).unwrap();

    let dns = Arc::new(FakeDns::default());
    let manager = manager_with_dns(&root, dns.clone()).await;
    assert_eq!(dns.restored.lock().unwrap().as_slice(), &[orphan]);
    assert!(!record_path(&root).exists());
    manager.shutdown().await.unwrap();
}

#[tokio::test]
async fn dns_apply_failure_does_not_fail_core_transaction() {
    let temp = tempfile::tempdir().unwrap();
    let root = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();
    let config = root.join("config.yaml");
    tokio::fs::write(
        &config,
        format!("external-controller: {}\n", free_controller_address()),
    )
    .await
    .unwrap();
    let spec = fake_core_spec(&root, config);
    let dns = Arc::new(FakeDns::default());
    dns.fail_apply.store(true, Ordering::SeqCst);
    let manager = manager_with_dns(&root, dns.clone()).await;

    let outcome = manager.reconcile(spec, None).await.unwrap();
    assert!(matches!(outcome, ApplyOutcome::Started { .. }));
    assert!(matches!(manager.status().state, CoreState::Running { .. }));
    let record: DnsOverrideRecord =
        serde_json::from_slice(&std::fs::read(record_path(&root)).unwrap()).unwrap();
    assert!(
        record.previous.is_empty(),
        "failed apply keeps the pre-record because the side effect is uncertain"
    );

    manager.shutdown().await.unwrap();
    assert_eq!(dns.restored.lock().unwrap().len(), 1);
    assert!(!record_path(&root).exists());
}

#[tokio::test]
async fn revision_conflict_has_no_dns_side_effect() {
    let temp = tempfile::tempdir().unwrap();
    let root = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();
    let config = root.join("config.yaml");
    tokio::fs::write(
        &config,
        format!("external-controller: {}\n", free_controller_address()),
    )
    .await
    .unwrap();
    let spec = fake_core_spec(&root, config);
    let dns = Arc::new(FakeDns::default());
    let manager = manager_with_dns(&root, dns.clone()).await;

    manager.reconcile(spec.clone(), None).await.unwrap();
    let before = std::fs::read(record_path(&root)).unwrap();
    let stale = RevisionId {
        epoch: 99,
        generation: 99,
        effective_hash: "deadbeef".into(),
    };
    let error = manager.reconcile(spec, Some(stale)).await.unwrap_err();
    assert!(matches!(error, Error::RevisionConflict { .. }));
    assert_eq!(dns.applied.lock().unwrap().len(), 1);
    assert_eq!(std::fs::read(record_path(&root)).unwrap(), before);

    manager.shutdown().await.unwrap();
}

#[tokio::test]
async fn noop_reconcile_preserves_original_dns_baseline() {
    let temp = tempfile::tempdir().unwrap();
    let root = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();
    let config = root.join("config.yaml");
    tokio::fs::write(
        &config,
        format!("external-controller: {}\n", free_controller_address()),
    )
    .await
    .unwrap();
    let spec = fake_core_spec(&root, config);
    let dns = Arc::new(FakeDns::default());
    let manager = manager_with_dns(&root, dns.clone()).await;

    manager.reconcile(spec.clone(), None).await.unwrap();
    let outcome = manager.reconcile(spec, None).await.unwrap();
    assert!(matches!(outcome, ApplyOutcome::Noop { .. }));
    assert_eq!(dns.applied.lock().unwrap().len(), 2);
    let record: DnsOverrideRecord =
        serde_json::from_slice(&std::fs::read(record_path(&root)).unwrap()).unwrap();
    assert_eq!(record.previous, vec!["10.0.0.1"]);

    manager.stop().await.unwrap();
    assert_eq!(*dns.current.lock().unwrap(), vec!["10.0.0.1"]);
    manager.shutdown().await.unwrap();
}
