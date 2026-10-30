#![cfg(windows)]

//! Windows process integration contract: a core launched by the shared process
//! manager can handle a targeted Ctrl+Break, another process in the test
//! runner's console remains alive, and helper failure preserves the hard-kill fallback.
//! The boundary is a real Windows process group and Job Object, not a fixture.
//! Full contract: ../../../../../docs/testing/contracts/windows-core-graceful-shutdown.md

use std::{
    path::PathBuf,
    time::{Duration, Instant},
};

use chimera_utils::process::{Command, Containment, ProcessEvent};
use tokio::sync::mpsc;

fn child_exe() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_chimera-test-child"))
}

async fn wait_until_ready(events: &mut mpsc::Receiver<ProcessEvent>) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while let Some(event) = events.recv().await {
            match event {
                ProcessEvent::Stdout(line) if line == "ready" => return,
                ProcessEvent::Terminated(payload) => {
                    panic!("child exited before ready: {payload:?}")
                }
                ProcessEvent::Error(error) => panic!("child output failed before ready: {error}"),
                _ => {}
            }
        }
        panic!("child output stream closed before ready");
    })
    .await
    .expect("child did not reach ready state");
}

#[tokio::test]
async fn graceful_shutdown_targets_its_process_group_and_not_a_sibling() {
    let temp = tempfile::tempdir().unwrap();
    let core_marker = temp.path().join("core-stopped.txt");
    let sibling_marker = temp.path().join("sibling-stopped.txt");
    let exe = child_exe();

    let (core, mut core_events) = Command::new(&exe)
        .args(["wait-for-ctrl-break", core_marker.to_str().unwrap()])
        .kill_grace(Duration::from_secs(2))
        .windows_shutdown_helper(&exe)
        .spawn()
        .await
        .unwrap();
    assert_eq!(core.containment(), Containment::JobObject);
    wait_until_ready(&mut core_events).await;

    let (sibling, mut sibling_events) = Command::new(&exe)
        .args(["wait-for-ctrl-break", sibling_marker.to_str().unwrap()])
        .kill_grace(Duration::from_secs(2))
        .windows_shutdown_helper(&exe)
        .spawn()
        .await
        .unwrap();
    wait_until_ready(&mut sibling_events).await;

    core.graceful_kill().await.unwrap();
    assert_eq!(core.wait().await.unwrap().code, Some(0));

    assert_eq!(
        std::fs::read_to_string(&core_marker).unwrap(),
        "ctrl-break-received"
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(250), sibling.wait())
            .await
            .is_err(),
        "a sibling sharing the console must not receive the target's Ctrl+Break"
    );
    assert!(
        !sibling_marker.exists(),
        "the sibling's Ctrl+Break handler must not run"
    );

    sibling.kill().await.unwrap();
}

#[tokio::test]
async fn missing_shutdown_helper_falls_back_to_job_object_kill() {
    let exe = child_exe();
    let missing_helper = exe.with_file_name("missing-chimera-shutdown-helper.exe");
    let (core, mut events) = Command::new(&exe)
        .arg("sleep-forever")
        .kill_grace(Duration::from_secs(5))
        .windows_shutdown_helper(missing_helper)
        .spawn()
        .await
        .unwrap();
    assert_eq!(core.containment(), Containment::JobObject);
    wait_until_ready(&mut events).await;

    let started = Instant::now();
    core.graceful_kill().await.unwrap();
    assert_ne!(
        core.wait().await.unwrap().code,
        Some(0),
        "missing helper must terminate the core through Job Object fallback"
    );
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "helper spawn failure must trigger immediate contained termination"
    );
}
