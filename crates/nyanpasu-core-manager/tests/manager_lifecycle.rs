use std::time::Duration;

use camino::Utf8PathBuf;
use nyanpasu_core_manager::{
    CoreKind, CoreManager, CoreSpec, CoreState, Error, InstanceOptions, InstanceSpec,
    ManagerOptions, StopReason,
};

fn free_controller_address() -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    drop(listener);
    address.to_string()
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
    let spec = InstanceSpec {
        core: CoreSpec {
            kind: CoreKind::Mihomo,
            binary_path: Utf8PathBuf::from(env!("CARGO_BIN_EXE_nyanpasu-fake-core")),
            version: Some("v1.18.9".into()),
            features: Vec::new(),
        },
        config_path: source_config.clone(),
        working_dir: root.clone(),
        pid_file: None,
        options: InstanceOptions {
            startup_timeout: Duration::from_secs(5),
            ..InstanceOptions::default()
        },
    };

    manager.start(spec.clone()).await.unwrap();
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
    assert!(matches!(
        manager.start(spec).await,
        Err(Error::AlreadyRunning)
    ));

    manager.stop().await.unwrap();
    let stopped = manager.status();
    assert!(matches!(
        stopped.state,
        CoreState::Stopped {
            reason: Some(StopReason::User)
        }
    ));
    assert!(!revision.runtime_path.exists());
    assert!(matches!(manager.stop().await, Err(Error::NotStarted)));
}
