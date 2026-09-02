use std::time::Duration;

use camino::Utf8PathBuf;
use nyanpasu_core_manager::{
    ApplyOutcome, CoreKind, CoreManager, CoreSpec, CoreState, Error, InstanceOptions, InstanceSpec,
    ManagerOptions, RevisionId,
};

fn free_controller_address() -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    drop(listener);
    address.to_string()
}

fn fake_core_spec(root: &Utf8PathBuf, config_path: Utf8PathBuf) -> InstanceSpec {
    InstanceSpec {
        core: CoreSpec {
            kind: CoreKind::Mihomo,
            binary_path: Utf8PathBuf::from(env!("CARGO_BIN_EXE_nyanpasu-fake-core")),
            version: Some("v1.18.9".into()),
            features: Vec::new(),
        },
        config_path,
        working_dir: root.clone(),
        pid_file: None,
        options: InstanceOptions {
            startup_timeout: Duration::from_secs(5),
            ..InstanceOptions::default()
        },
    }
}

async fn manager(root: &Utf8PathBuf) -> CoreManager {
    CoreManager::new(ManagerOptions {
        runtime_dir: Some(root.join("runtime")),
        ..ManagerOptions::default()
    })
    .await
    .unwrap()
}

#[tokio::test]
async fn reconcile_starts_cold_then_noops_on_the_same_config() {
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
    let manager = manager(&root).await;

    let outcome = manager.reconcile(spec.clone(), None).await.unwrap();
    let ApplyOutcome::Started { revision } = outcome else {
        panic!("cold reconcile must report Started, got {outcome:?}");
    };
    assert!(matches!(manager.status().state, CoreState::Running { .. }));

    let outcome = manager
        .reconcile(spec, Some(revision.id()))
        .await
        .unwrap();
    assert!(matches!(outcome, ApplyOutcome::Noop { .. }));

    manager.shutdown().await.unwrap();
}

#[tokio::test]
async fn reconcile_with_a_stale_expectation_changes_nothing() {
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
    let manager = manager(&root).await;

    let ApplyOutcome::Started { revision } = manager.reconcile(spec.clone(), None).await.unwrap()
    else {
        panic!("cold reconcile must report Started");
    };
    let running = manager.status();
    let stale = RevisionId {
        epoch: 99,
        generation: 9,
        effective_hash: "deadbeefdeadbeef".into(),
    };
    let error = manager
        .reconcile(spec, Some(stale))
        .await
        .expect_err("stale expectation must conflict");
    let Error::RevisionConflict { actual, .. } = error else {
        panic!("expected RevisionConflict, got {error}");
    };
    assert_eq!(actual, Some(revision.id()));
    let status = manager.status();
    assert_eq!(status.state, running.state);
    assert_eq!(
        status.revision.as_ref().map(|revision| revision.id()),
        Some(revision.id())
    );

    manager.shutdown().await.unwrap();
}

#[tokio::test]
async fn reconcile_against_a_stopped_manager_rejects_a_believed_revision() {
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
    let manager = manager(&root).await;
    let expected = RevisionId {
        epoch: 1,
        generation: 1,
        effective_hash: "0123456789abcdef".into(),
    };

    let error = manager
        .reconcile(spec, Some(expected))
        .await
        .expect_err("a believed revision cannot match a stopped manager");
    let Error::RevisionConflict { actual, .. } = error else {
        panic!("expected RevisionConflict, got {error}");
    };
    assert_eq!(actual, None);
    assert!(matches!(manager.status().state, CoreState::Stopped { .. }));

    manager.shutdown().await.unwrap();
}

#[tokio::test]
async fn rejected_config_keeps_the_old_runtime_and_revision() {
    let temp = tempfile::tempdir().unwrap();
    let root = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();
    let config = root.join("config.yaml");
    let controller = free_controller_address();
    tokio::fs::write(&config, format!("external-controller: {controller}\n"))
        .await
        .unwrap();
    let spec = fake_core_spec(&root, config);
    let manager = manager(&root).await;

    let ApplyOutcome::Started { revision } = manager.reconcile(spec.clone(), None).await.unwrap()
    else {
        panic!("cold reconcile must report Started");
    };

    let rejected = root.join("rejected.yaml");
    tokio::fs::write(
        &rejected,
        format!(
            "external-controller: {controller}\nx-fake-core:\n  check-fail: port already in use\n"
        ),
    )
    .await
    .unwrap();
    let mut rejected_spec = spec;
    rejected_spec.config_path = rejected;
    let error = manager
        .reconcile(rejected_spec, Some(revision.id()))
        .await
        .expect_err("dry-run rejection must abort");
    assert!(matches!(error, Error::ConfigCheckFailed(_)), "{error}");

    let status = manager.status();
    assert!(matches!(status.state, CoreState::Running { .. }));
    assert_eq!(
        status.revision.as_ref().map(|current| current.id()),
        Some(revision.id())
    );

    manager.shutdown().await.unwrap();
}
