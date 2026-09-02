use std::time::Duration;

use camino::Utf8PathBuf;
use nyanpasu_core_manager::{
    ApplyOutcome, CoreKind, CoreManager, CoreSpec, CoreState, InstanceOptions, InstanceSpec,
    ManagerOptions,
};

fn free_controller_address() -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    drop(listener);
    address.to_string()
}

fn spec(root: &Utf8PathBuf, config_path: Utf8PathBuf) -> InstanceSpec {
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

#[tokio::test]
async fn patch_updates_generation_without_restarting_the_epoch() {
    let temp = tempfile::tempdir().unwrap();
    let root = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();
    let controller = free_controller_address();
    let initial = root.join("initial.yaml");
    let desired = root.join("desired.yaml");
    tokio::fs::write(
        &initial,
        format!("external-controller: {controller}\nallow-lan: false\n"),
    )
    .await
    .unwrap();
    tokio::fs::write(
        &desired,
        format!("external-controller: {controller}\nallow-lan: true\n"),
    )
    .await
    .unwrap();

    let manager = CoreManager::new(ManagerOptions {
        runtime_dir: Some(root.join("runtime")),
        ..ManagerOptions::default()
    })
    .await
    .unwrap();
    manager.start(spec(&root, initial)).await.unwrap();
    let before = manager.status();
    let before_revision = before.revision.unwrap();
    let CoreState::Running { pid: before_pid, .. } = before.state else {
        panic!("expected running state")
    };

    let outcome = manager
        .apply_config(spec(&root, desired), Some(before_revision.id()))
        .await
        .unwrap();
    let ApplyOutcome::Patched { revision } = outcome else {
        panic!("expected in-place patch: {outcome:?}")
    };
    assert_eq!(revision.epoch, before_revision.epoch);
    assert_eq!(revision.generation, before_revision.generation + 1);
    let CoreState::Running { pid: after_pid, .. } = manager.status().state else {
        panic!("expected running state")
    };
    assert_eq!(before_pid, after_pid);
    manager.shutdown().await.unwrap();
}

#[tokio::test]
async fn reload_updates_generation_without_restarting_the_epoch() {
    let temp = tempfile::tempdir().unwrap();
    let root = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();
    let controller = free_controller_address();
    let initial = root.join("initial.yaml");
    let desired = root.join("desired.yaml");
    tokio::fs::write(
        &initial,
        format!("external-controller: {controller}\nrules:\n  - MATCH,DIRECT\n"),
    )
    .await
    .unwrap();
    tokio::fs::write(
        &desired,
        format!(
            "external-controller: {controller}\nrules:\n  - DOMAIN,example.com,DIRECT\n  - MATCH,DIRECT\n"
        ),
    )
    .await
    .unwrap();

    let manager = CoreManager::new(ManagerOptions {
        runtime_dir: Some(root.join("runtime")),
        ..ManagerOptions::default()
    })
    .await
    .unwrap();
    manager.start(spec(&root, initial)).await.unwrap();
    let before = manager.status();
    let before_revision = before.revision.unwrap();
    let CoreState::Running { pid: before_pid, .. } = before.state else {
        panic!("expected running state")
    };

    let outcome = manager
        .apply_config(spec(&root, desired), Some(before_revision.id()))
        .await
        .unwrap();
    let ApplyOutcome::Reloaded { revision } = outcome else {
        panic!("expected in-place reload: {outcome:?}")
    };
    assert_eq!(revision.epoch, before_revision.epoch);
    assert_eq!(revision.generation, before_revision.generation + 1);
    let CoreState::Running { pid: after_pid, .. } = manager.status().state else {
        panic!("expected running state")
    };
    assert_eq!(before_pid, after_pid);
    manager.shutdown().await.unwrap();
}

#[cfg(feature = "test-hooks")]
#[tokio::test]
async fn installed_patch_reports_parent_sync_uncertainty_without_losing_the_outcome() {
    let temp = tempfile::tempdir().unwrap();
    let root = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();
    let controller = free_controller_address();
    let initial = root.join("initial.yaml");
    let desired = root.join("desired.yaml");
    tokio::fs::write(
        &initial,
        format!("external-controller: {controller}\nallow-lan: false\n"),
    )
    .await
    .unwrap();
    tokio::fs::write(
        &desired,
        format!("external-controller: {controller}\nallow-lan: true\n"),
    )
    .await
    .unwrap();
    let manager = CoreManager::new(ManagerOptions {
        runtime_dir: Some(root.join("runtime")),
        ..ManagerOptions::default()
    })
    .await
    .unwrap();
    manager.start(spec(&root, initial)).await.unwrap();
    let before = manager.status().revision.unwrap();
    manager.inject_runtime_parent_sync_failure_once_for_test();

    let outcome = manager
        .apply_config(spec(&root, desired), Some(before.id()))
        .await
        .unwrap();
    let ApplyOutcome::DurabilityUncertain { outcome, warning } = outcome else {
        panic!("expected installed-but-uncertain outcome: {outcome:?}")
    };
    assert!(matches!(*outcome, ApplyOutcome::Patched { .. }));
    assert!(warning.contains("parent-directory synchronization failed"));
    assert!(matches!(manager.status().state, CoreState::Running { epoch: 1, .. }));
    manager.shutdown().await.unwrap();
}

#[tokio::test]
async fn patch_verification_mismatch_falls_back_to_a_restart() {
    let temp = tempfile::tempdir().unwrap();
    let root = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();
    let controller = free_controller_address();
    let initial = root.join("initial.yaml");
    let desired = root.join("desired.yaml");
    let behavior = "x-fake-core:\n  patch-no-effect: true\n";
    tokio::fs::write(
        &initial,
        format!("external-controller: {controller}\nallow-lan: false\n{behavior}"),
    )
    .await
    .unwrap();
    tokio::fs::write(
        &desired,
        format!("external-controller: {controller}\nallow-lan: true\n{behavior}"),
    )
    .await
    .unwrap();

    let manager = CoreManager::new(ManagerOptions {
        runtime_dir: Some(root.join("runtime")),
        ..ManagerOptions::default()
    })
    .await
    .unwrap();
    manager.start(spec(&root, initial)).await.unwrap();
    let before = manager.status().revision.unwrap();
    let outcome = manager
        .apply_config(spec(&root, desired), Some(before.id()))
        .await
        .unwrap();
    let ApplyOutcome::Restarted { revision } = outcome else {
        panic!("expected hard fallback after verification mismatch: {outcome:?}")
    };
    assert!(revision.epoch > before.epoch);
    manager.shutdown().await.unwrap();
}
