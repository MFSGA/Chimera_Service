#[cfg(feature = "test-hooks")]
use std::time::Duration;

use camino::Utf8PathBuf;
use nyanpasu_core_manager::{CoreKind, CoreSpec, Error, InstanceOptions, InstanceSpec};

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
        options: InstanceOptions::default(),
    }
}

#[tokio::test]
async fn config_check_passes_and_surfaces_core_rejection() {
    let temp = tempfile::tempdir().unwrap();
    let root = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();
    let valid = root.join("valid.yaml");
    let invalid = root.join("invalid.yaml");
    tokio::fs::write(&valid, "external-controller: 127.0.0.1:1\n")
        .await
        .unwrap();
    tokio::fs::write(&invalid, "external-controller: 127.0.0.1:1\nreject: true\n")
        .await
        .unwrap();

    nyanpasu_core_manager::kind::check_config(&spec(&root, valid))
        .await
        .unwrap();
    let error = nyanpasu_core_manager::kind::check_config(&spec(&root, invalid))
        .await
        .unwrap_err();
    assert!(matches!(error, Error::ConfigCheckFailed(_)));
    assert!(error.to_string().contains("fake core rejected config"));
}

#[cfg(feature = "test-hooks")]
#[tokio::test]
async fn explicit_check_deadline_kills_a_hung_check() {
    let temp = tempfile::tempdir().unwrap();
    let root = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();
    let config = root.join("slow.yaml");
    tokio::fs::write(
        &config,
        "external-controller: 127.0.0.1:1\ncheck-delay-ms: 500\n",
    )
    .await
    .unwrap();

    let started = std::time::Instant::now();
    let error = nyanpasu_core_manager::kind::check_config_within(
        &spec(&root, config),
        Duration::from_millis(50),
    )
    .await
    .unwrap_err();
    assert!(matches!(error, Error::ConfigCheckFailed(_)));
    assert!(started.elapsed() < Duration::from_secs(2));
    assert!(error.to_string().contains("timed out"));
}
