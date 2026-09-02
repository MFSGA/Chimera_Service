use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use camino::Utf8PathBuf;
use nyanpasu_core_manager::{
    ApplyOutcome, CoreKind, CoreManager, CoreSpec, CoreState, Error, InstanceOptions, InstanceSpec,
    InstanceState, InstanceStatus, ManagerOptions, ProbePhase, ProbeResult, ResolvedController,
    RuntimeBackend, RuntimeInstance, RuntimeLaunchRequest, runtime::BoxFuture,
};
use tokio::sync::watch;

struct FakeInstance {
    spec: InstanceSpec,
    epoch: u64,
    controller: ResolvedController,
    pid: u32,
    state_tx: watch::Sender<InstanceStatus>,
}

impl FakeInstance {
    fn new(spec: InstanceSpec, epoch: u64, controller: ResolvedController, pid: u32) -> Self {
        let (state_tx, _) = watch::channel(InstanceStatus {
            state: InstanceState::Running { pid },
            health: None,
        });
        Self {
            spec,
            epoch,
            controller,
            pid,
            state_tx,
        }
    }
}

impl RuntimeInstance for FakeInstance {
    fn epoch(&self) -> u64 {
        self.epoch
    }

    fn spec(&self) -> &InstanceSpec {
        &self.spec
    }

    fn controller(&self) -> &ResolvedController {
        &self.controller
    }

    fn pid(&self) -> Option<u32> {
        Some(self.pid)
    }

    fn state(&self) -> watch::Receiver<InstanceStatus> {
        self.state_tx.subscribe()
    }

    fn wait_ready<'a>(&'a self) -> BoxFuture<'a, Result<(), Error>> {
        Box::pin(async { Ok(()) })
    }

    fn probe_now<'a>(&'a self, _phase: ProbePhase) -> BoxFuture<'a, ProbeResult> {
        Box::pin(async { ProbeResult::Healthy })
    }

    fn stop_and_confirm_dead(
        self: Box<Self>,
        _timeout: Duration,
    ) -> BoxFuture<'static, Result<(), Error>> {
        Box::pin(async { Ok(()) })
    }
}

#[derive(Default)]
struct SequencedBackend {
    launches: AtomicUsize,
}

impl RuntimeBackend for SequencedBackend {
    fn launch(
        &self,
        request: RuntimeLaunchRequest,
    ) -> BoxFuture<'_, Result<Box<dyn RuntimeInstance>, Error>> {
        let index = self.launches.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            if index == 1 {
                return Err(Error::ApplyFailed(
                    "injected desired replacement failure".into(),
                ));
            }
            let pid = if index == 0 { 100 } else { 200 };
            Ok(Box::new(FakeInstance::new(
                request.effective_spec,
                request.epoch,
                request.controller,
                pid,
            )) as Box<dyn RuntimeInstance>)
        })
    }

    fn check_config<'a>(&'a self, _spec: &'a InstanceSpec) -> BoxFuture<'a, Result<(), Error>> {
        Box::pin(async { Ok(()) })
    }
}

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
            // The custom backend never executes this path. An explicit version
            // also keeps capability resolution from probing it.
            binary_path: root.join("fake-mihomo"),
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
async fn failed_same_epoch_replacement_rolls_back_the_original_revision() {
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

    let backend = Arc::new(SequencedBackend::default());
    let manager = CoreManager::builder(ManagerOptions {
        runtime_dir: Some(root.join("runtime")),
        control_timeout: Duration::from_millis(100),
        ..ManagerOptions::default()
    })
    .runtime_backend(backend.clone())
    .build()
    .await
    .unwrap();

    manager.start(spec(&root, initial)).await.unwrap();
    let before = manager.status();
    let before_revision = before.revision.unwrap();
    assert!(matches!(
        before.state,
        CoreState::Running { epoch: 1, pid: 100 }
    ));

    let outcome = manager
        .apply_config(spec(&root, desired), Some(before_revision.id()))
        .await
        .unwrap();
    let ApplyOutcome::RolledBack {
        revision,
        failed_apply,
    } = outcome
    else {
        panic!("expected rollback, got {outcome:?}");
    };

    assert_eq!(revision, before_revision);
    assert!(failed_apply.contains("injected desired replacement failure"));
    let after = manager.status();
    assert_eq!(after.revision, Some(before_revision.clone()));
    assert!(matches!(
        after.state,
        CoreState::Running { epoch: 1, pid: 200 }
    ));
    assert_eq!(backend.launches.load(Ordering::SeqCst), 3);

    manager.shutdown().await.unwrap();
}
