use std::time::Duration;

use camino::Utf8PathBuf;
use nyanpasu_core_manager::{
    ApplyOutcome, CoreKind, CoreManager, CoreSpec, CoreState, DegradeReason, Error,
    InstanceOptions, InstanceSpec, ManagerOptions, RevisionId, StopReason, SwitchOutcome,
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

#[tokio::test]
async fn managed_epoch_runs_from_preflight_through_cleanup() {
    let temp = tempfile::tempdir().unwrap();
    let root = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();
    let runtime_dir = root.join("runtime");
    let source_config = root.join("source.yaml");
    let controller = free_controller_address();
    tokio::fs::write(
        &source_config,
        format!("external-controller: {controller}\nsecret: test-secret\n"),
    )
    .await
    .unwrap();

    let manager = CoreManager::new(ManagerOptions {
        runtime_dir: Some(runtime_dir.clone()),
        ..ManagerOptions::default()
    })
    .await
    .unwrap();
    let spec = fake_core_spec(&root, source_config.clone());
    let mut logs = manager.subscribe_logs();

    manager.start(spec.clone()).await.unwrap();
    let first_log = tokio::time::timeout(Duration::from_secs(2), logs.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(first_log.epoch, 1);
    assert_eq!(first_log.raw, "fake core started");
    let running = manager.status();
    let CoreState::Running { epoch, pid } = running.state else {
        panic!("expected running manager snapshot: {running:?}");
    };
    assert_eq!(epoch, 1);
    assert!(pid > 0);
    let revision = running.revision.expect("running revision");
    assert!(revision.runtime_path.exists());
    assert_eq!(revision.generation, 1);
    assert_eq!(running.spec.as_ref().unwrap().config_path, source_config);
    assert_eq!(
        manager
            .apply_config(spec.clone(), Some(revision.id()))
            .await
            .unwrap(),
        ApplyOutcome::Noop {
            revision: revision.clone()
        }
    );
    assert!(matches!(
        manager
            .apply_config(
                spec.clone(),
                Some(RevisionId {
                    epoch: 99,
                    generation: 1,
                    effective_hash: "stale".into(),
                }),
            )
            .await,
        Err(Error::RevisionConflict { .. })
    ));
    assert!(matches!(manager.status().state, CoreState::Running { epoch: 1, .. }));
    assert!(matches!(
        manager.start(spec.clone()).await,
        Err(Error::AlreadyRunning)
    ));
    assert!(manager.reconcile().await.unwrap().is_healthy());

    assert_eq!(
        manager.restart().await.unwrap(),
        SwitchOutcome::Hard {
            reason: DegradeReason::HttpController
        }
    );
    let restarted = manager.status();
    assert!(matches!(restarted.state, CoreState::Running { epoch: 2, .. }));
    let restarted_revision = restarted.revision.expect("restarted revision");
    assert!(restarted_revision.runtime_path.exists());
    assert!(!revision.runtime_path.exists());

    assert_eq!(
        manager.switch(spec).await.unwrap(),
        SwitchOutcome::Hard {
            reason: DegradeReason::HttpController
        }
    );
    let switched = manager.status();
    assert!(matches!(switched.state, CoreState::Running { epoch: 3, .. }));
    let switched_revision = switched.revision.expect("switched revision");
    assert!(switched_revision.runtime_path.exists());
    assert!(!restarted_revision.runtime_path.exists());

    manager.stop().await.unwrap();
    let stopped = manager.status();
    assert!(matches!(
        stopped.state,
        CoreState::Stopped {
            reason: Some(StopReason::User)
        }
    ));
    assert!(!switched_revision.runtime_path.exists());
    assert!(matches!(manager.stop().await, Err(Error::NotStarted)));
}

#[tokio::test]
async fn failed_apply_rolls_back_to_the_previous_spec() {
    let temp = tempfile::tempdir().unwrap();
    let root = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();
    let runtime_dir = root.join("runtime");
    let controller = free_controller_address();
    let source_config = root.join("source.yaml");
    tokio::fs::write(
        &source_config,
        format!("external-controller: {controller}\n"),
    )
    .await
    .unwrap();
    let manager = CoreManager::new(ManagerOptions {
        runtime_dir: Some(runtime_dir),
        ..ManagerOptions::default()
    })
    .await
    .unwrap();
    let original = fake_core_spec(&root, source_config);
    manager.start(original.clone()).await.unwrap();
    let expected = manager.status().revision.unwrap().id();

    let rejected_config = root.join("finish.yaml");
    tokio::fs::write(
        &rejected_config,
        format!("external-controller: {controller}\nfinish: true\n"),
    )
    .await
    .unwrap();
    let outcome = manager
        .apply_config(fake_core_spec(&root, rejected_config), Some(expected))
        .await
        .unwrap();
    let ApplyOutcome::RolledBack {
        revision,
        failed_apply,
    } = outcome
    else {
        panic!("expected rolled back apply: {outcome:?}");
    };
    assert_eq!(revision.epoch, 3);
    assert!(failed_apply.contains("failed to start") || failed_apply.contains("stopped before"));
    assert!(matches!(manager.status().state, CoreState::Running { epoch: 3, .. }));
    assert_eq!(manager.status().spec.unwrap().config_path, original.config_path);
    manager.stop().await.unwrap();
}

#[tokio::test]
async fn failed_preflight_removes_the_staged_epoch_artifacts() {
    let temp = tempfile::tempdir().unwrap();
    let root = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();
    let runtime_dir = root.join("runtime");
    let source_config = root.join("invalid.yaml");
    tokio::fs::write(
        &source_config,
        format!(
            "external-controller: {}\nreject: true\n",
            free_controller_address()
        ),
    )
    .await
    .unwrap();
    let manager = CoreManager::new(ManagerOptions {
        runtime_dir: Some(runtime_dir.clone()),
        ..ManagerOptions::default()
    })
    .await
    .unwrap();

    assert!(matches!(
        manager.start(fake_core_spec(&root, source_config)).await,
        Err(Error::ConfigCheckFailed(_))
    ));
    assert!(!runtime_dir.join("config-1.yaml").exists());
    assert!(!runtime_dir.join("core-1.pid").exists());
    assert!(matches!(
        manager.status().state,
        CoreState::Stopped {
            reason: Some(StopReason::Error(_))
        }
    ));
}
