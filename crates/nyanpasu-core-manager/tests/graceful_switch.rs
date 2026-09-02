use std::time::Duration;

use camino::Utf8PathBuf;
use nyanpasu_core_manager::{
    CoreKind, CoreManager, CoreSpec, CoreState, Host, InstanceOptions, InstanceSpec,
    LocalIpcPolicy, ManagerOptions, SwitchOutcome,
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

async fn manager(root: &Utf8PathBuf) -> CoreManager {
    CoreManager::new(ManagerOptions {
        runtime_dir: Some(root.join("runtime")),
        local_ipc_policy: LocalIpcPolicy::Force,
        ..ManagerOptions::default()
    })
    .await
    .unwrap()
}

#[tokio::test]
async fn force_local_ipc_injects_an_epoch_scoped_controller() {
    let temp = tempfile::tempdir().unwrap();
    let root = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();
    let source = root.join("local.yaml");
    tokio::fs::write(
        &source,
        format!("external-controller: {}\n", free_controller_address()),
    )
    .await
    .unwrap();
    let manager = manager(&root).await;
    manager.start(spec(&root, source)).await.unwrap();

    let status = manager.status();
    assert!(matches!(status.state, CoreState::Running { epoch: 1, .. }));
    match status.controller.expect("controller") {
        #[cfg(windows)]
        Host::NamedPipe(path) => assert!(path.to_string_lossy().contains("core-1")),
        #[cfg(unix)]
        Host::UnixSocket(path) => assert!(path.to_string_lossy().contains("core-1")),
        other => panic!("expected local IPC controller: {other:?}"),
    }
    manager.shutdown().await.unwrap();
}

#[tokio::test]
async fn safe_mihomo_switch_uses_the_graceful_overlap_path() {
    let temp = tempfile::tempdir().unwrap();
    let root = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();
    let initial = root.join("initial.yaml");
    let desired = root.join("desired.yaml");
    tokio::fs::write(
        &initial,
        format!(
            "external-controller: {}\nmixed-port: 17890\nallow-lan: false\n",
            free_controller_address()
        ),
    )
    .await
    .unwrap();
    tokio::fs::write(
        &desired,
        format!(
            "external-controller: {}\nmixed-port: 17891\nallow-lan: true\n",
            free_controller_address()
        ),
    )
    .await
    .unwrap();

    let manager = manager(&root).await;
    manager.start(spec(&root, initial)).await.unwrap();
    let old_revision = manager.status().revision.unwrap();
    let old_pid = match manager.status().state {
        CoreState::Running { pid, .. } => pid,
        other => panic!("expected running state: {other:?}"),
    };

    let outcome = manager.switch(spec(&root, desired)).await.unwrap();
    assert_eq!(outcome, SwitchOutcome::Graceful);
    let status = manager.status();
    let CoreState::Running { epoch, pid } = status.state else {
        panic!("expected running state after graceful switch: {status:?}")
    };
    assert_eq!(epoch, 2);
    assert_ne!(pid, old_pid);
    let revision = status.revision.unwrap();
    assert_eq!(revision.epoch, 2);
    assert!(!old_revision.runtime_path.exists());
    assert!(revision.runtime_path.exists());
    manager.shutdown().await.unwrap();
}
