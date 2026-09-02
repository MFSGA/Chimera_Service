use std::time::Duration;

use camino::{Utf8Path, Utf8PathBuf};
use nyanpasu_core_manager::{
    ApplyOutcome, CheckRequest, ConfigInput, ControlOptions, CoreCommand, CoreCommandEnvelope,
    CoreControl, CoreErrorKind, CoreKind, CoreManager, CoreSpec, CoreState, ExecutorExit,
    InstanceOptions, ManagerOptions, OperationId, OperationOutput, OperationState, ReconcileRequest,
};

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn fast_options() -> InstanceOptions {
    InstanceOptions {
        startup_timeout: Duration::from_secs(5),
        ..InstanceOptions::default()
    }
}

fn core_spec(_dir: &Utf8Path) -> CoreSpec {
    CoreSpec {
        kind: CoreKind::Mihomo,
        binary_path: Utf8PathBuf::from(env!("CARGO_BIN_EXE_nyanpasu-fake-core")),
        version: Some("v1.18.9".into()),
        features: Vec::new(),
    }
}

fn config_body(port: u16, extra: &str) -> String {
    format!("external-controller: 127.0.0.1:{port}\n{extra}")
}

async fn control_with(
    dir: &Utf8Path,
    tweak: impl FnOnce(ControlOptions) -> ControlOptions,
) -> CoreControl {
    let manager = CoreManager::new(ManagerOptions {
        runtime_dir: Some(dir.join("runtime")),
        ..ManagerOptions::default()
    })
    .await
    .unwrap();
    CoreControl::spawn(
        manager,
        tweak(ControlOptions::new(dir.join("sources"), dir.to_owned())),
    )
}

async fn control(dir: &Utf8Path) -> CoreControl {
    control_with(dir, |options| options).await
}

fn reconcile_envelope(id: OperationId, dir: &Utf8Path, body: &str) -> CoreCommandEnvelope {
    CoreCommandEnvelope {
        operation_id: id,
        command: CoreCommand::Reconcile(Box::new(ReconcileRequest {
            core: core_spec(dir),
            config: ConfigInput::inline(body.as_bytes().to_vec()),
            options: fast_options(),
            expected_applied: None,
        })),
    }
}

fn command(command: CoreCommand) -> CoreCommandEnvelope {
    CoreCommandEnvelope {
        operation_id: OperationId::generate(),
        command,
    }
}

async fn shutdown(control: &CoreControl) {
    control.shutdown().await.unwrap();
    assert_eq!(control.until_closed().await, ExecutorExit::Clean);
}

#[tokio::test]
async fn submitted_reconcile_runs_to_started_and_stop_completes() {
    let temp = tempfile::tempdir().unwrap();
    let dir = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();
    let control = control(&dir).await;
    let id = OperationId::generate();

    let output = control
        .submit(reconcile_envelope(id, &dir, &config_body(free_port(), "")))
        .unwrap()
        .wait()
        .await
        .unwrap();
    assert!(matches!(
        output,
        OperationOutput::Reconciled(ApplyOutcome::Started { .. })
    ));
    assert!(matches!(control.status().state, CoreState::Running { .. }));
    assert!(matches!(
        control.operation(id),
        Some(OperationState::Succeeded(_))
    ));

    assert!(matches!(
        control
            .submit(command(CoreCommand::Stop))
            .unwrap()
            .wait()
            .await
            .unwrap(),
        OperationOutput::Stopped
    ));
    let error = control
        .submit(command(CoreCommand::Stop))
        .unwrap()
        .wait()
        .await
        .unwrap_err();
    assert_eq!(error.kind, Some(CoreErrorKind::NotStarted));
    shutdown(&control).await;
}

#[tokio::test]
async fn idempotent_resubmit_attaches_to_original_operation() {
    let temp = tempfile::tempdir().unwrap();
    let dir = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();
    let control = control(&dir).await;
    let body = config_body(free_port(), "");
    let id = OperationId::generate();

    let first = control.submit(reconcile_envelope(id, &dir, &body)).unwrap();
    let second = control.submit(reconcile_envelope(id, &dir, &body)).unwrap();
    assert!(first.newly_admitted());
    assert!(!second.newly_admitted());
    assert_eq!(first.sequence(), second.sequence());

    let first = first.wait().await.unwrap();
    let second = second.wait().await.unwrap();
    assert_eq!(first, second);
    shutdown(&control).await;
}

#[tokio::test]
async fn same_id_with_different_payload_conflicts_at_submit() {
    let temp = tempfile::tempdir().unwrap();
    let dir = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();
    let control = control(&dir).await;
    let port = free_port();
    let id = OperationId::generate();

    let first = control
        .submit(reconcile_envelope(id, &dir, &config_body(port, "")))
        .unwrap();
    let error = control
        .submit(reconcile_envelope(
            id,
            &dir,
            &config_body(port, "mixed-port: 7899\n"),
        ))
        .unwrap_err();
    assert_eq!(error.kind, Some(CoreErrorKind::OperationConflict));
    assert!(!error.retryable);

    first.wait().await.unwrap();
    shutdown(&control).await;
}

#[tokio::test]
async fn full_queue_returns_retryable_queue_full() {
    let temp = tempfile::tempdir().unwrap();
    let dir = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();
    let control = control_with(&dir, |mut options| {
        options.queue_capacity = 1;
        options
    })
    .await;
    let port = free_port();
    let slow = config_body(port, "x-fake-core:\n  check-delay-ms: 1200\n");

    let first_id = OperationId::generate();
    let first = control
        .submit(reconcile_envelope(first_id, &dir, &slow))
        .unwrap();

    tokio::time::timeout(Duration::from_secs(5), async {
        while !matches!(control.operation(first_id), Some(OperationState::Running)) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();

    let second = control
        .submit(reconcile_envelope(OperationId::generate(), &dir, &slow))
        .unwrap();
    let rejected_id = OperationId::generate();
    let error = control
        .submit(reconcile_envelope(rejected_id, &dir, &slow))
        .unwrap_err();
    assert_eq!(error.kind, Some(CoreErrorKind::QueueFull));
    assert!(error.retryable);
    assert!(control.operation(rejected_id).is_none());

    first.wait().await.unwrap();
    let _ = second.wait().await;
    shutdown(&control).await;
}

#[tokio::test]
async fn dropping_handle_does_not_cancel_the_transaction() {
    let temp = tempfile::tempdir().unwrap();
    let dir = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();
    let control = control(&dir).await;
    let id = OperationId::generate();

    let handle = control
        .submit(reconcile_envelope(id, &dir, &config_body(free_port(), "")))
        .unwrap();
    drop(handle);

    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if matches!(
                control.operation(id),
                Some(OperationState::Succeeded(_)) | Some(OperationState::Failed(_))
            ) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert!(matches!(control.status().state, CoreState::Running { .. }));
    shutdown(&control).await;
}

#[tokio::test]
async fn shutdown_latches_admission() {
    let temp = tempfile::tempdir().unwrap();
    let dir = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();
    let control = control(&dir).await;

    let shutdown = control.submit(command(CoreCommand::Shutdown)).unwrap();
    let error = control
        .submit(reconcile_envelope(
            OperationId::generate(),
            &dir,
            &config_body(free_port(), ""),
        ))
        .unwrap_err();
    assert_eq!(error.kind, Some(CoreErrorKind::ShuttingDown));
    assert!(matches!(
        shutdown.wait().await.unwrap(),
        OperationOutput::ShutDown
    ));
    assert_eq!(control.until_closed().await, ExecutorExit::Clean);
}

#[tokio::test]
async fn advisory_check_does_not_mutate_runtime() {
    let temp = tempfile::tempdir().unwrap();
    let dir = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();
    let control = control(&dir).await;
    let port = free_port();

    let started = control
        .submit(reconcile_envelope(
            OperationId::generate(),
            &dir,
            &config_body(port, ""),
        ))
        .unwrap()
        .wait()
        .await
        .unwrap();
    let OperationOutput::Reconciled(ApplyOutcome::Started { revision }) = started else {
        panic!("expected cold start");
    };

    control
        .check(CheckRequest {
            core: core_spec(&dir),
            config: ConfigInput::inline(config_body(port, "mixed-port: 7899\n").into_bytes()),
        })
        .await
        .unwrap();

    let error = control
        .check(CheckRequest {
            core: core_spec(&dir),
            config: ConfigInput::inline(
                config_body(port, "x-fake-core:\n  check-fail: rejected\n").into_bytes(),
            ),
        })
        .await
        .unwrap_err();
    assert_eq!(error.kind, Some(CoreErrorKind::ConfigCheckFailed));

    let status = control.status();
    assert!(matches!(status.state, CoreState::Running { .. }));
    assert_eq!(
        status.revision.as_ref().map(|current| current.id()),
        Some(revision.id())
    );
    shutdown(&control).await;
}

#[tokio::test]
async fn declared_digest_mismatch_aborts_without_starting() {
    let temp = tempfile::tempdir().unwrap();
    let dir = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();
    let control = control(&dir).await;

    let error = control
        .submit(CoreCommandEnvelope {
            operation_id: OperationId::generate(),
            command: CoreCommand::Reconcile(Box::new(ReconcileRequest {
                core: core_spec(&dir),
                config: ConfigInput::Inline {
                    bytes: config_body(free_port(), "").into_bytes(),
                    expected_digest: Some("0000000000000000".into()),
                },
                options: fast_options(),
                expected_applied: None,
            })),
        })
        .unwrap()
        .wait()
        .await
        .unwrap_err();

    assert_eq!(error.kind, Some(CoreErrorKind::InvalidConfig));
    assert!(matches!(control.status().state, CoreState::Stopped { .. }));
    shutdown(&control).await;
}
