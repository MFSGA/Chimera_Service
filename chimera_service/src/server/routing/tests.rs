use std::{borrow::Cow, sync::Arc};

use axum::{
    body::{Body, to_bytes},
    http::{Method, Request, StatusCode, header::CONTENT_TYPE},
    response::Response,
};
use chimera_ipc::api::{
    ResponseCode,
    contract::{
        CoreApply, CoreCheck, CoreRecover, CoreRestart, CoreStart, CoreStop, CoreV2Operation,
        CoreV2Status, CoreV2Submit, IpcOperation, LogsInspect, LogsRetrieve, NetworkSetDns,
        Status as StatusOp,
    },
    core::{
        apply::{CoreApplyReq, CoreApplyRes},
        check::{CoreCheckReq, CoreCheckRes},
        recover::CoreRecoverRes,
        stop::CoreStopRes,
        v2::{
            CoreCommandInfo, CoreOperationReq, CoreOperationRes, CoreSubmitReq, CoreSubmitRes,
            OperationPhase,
        },
    },
    status::{CoreState, CoreStateDetail, StatusRes},
};
use chimera_utils::core::{ClashCoreType, CoreType};
use nyanpasu_core_manager::LocalIpcPolicy;
use serde::de::DeserializeOwned;
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;

use super::{AppState, create_router};
use crate::server::{CoreManager, Logger, consts::RuntimeInfos, events::EventHub};

struct TestEnv {
    state: AppState,
    _dir: TempDir,
}

impl TestEnv {
    async fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        for name in [
            "service-data",
            "service-config",
            "nyanpasu-config",
            "nyanpasu-data",
            "nyanpasu-app",
        ] {
            std::fs::create_dir_all(root.join(name)).unwrap();
        }
        let runtime = Arc::new(RuntimeInfos {
            service_data_dir: root.join("service-data"),
            service_config_dir: root.join("service-config"),
            nyanpasu_config_dir: root.join("nyanpasu-config"),
            nyanpasu_data_dir: root.join("nyanpasu-data"),
            nyanpasu_app_dir: root.join("nyanpasu-app"),
        });
        let core_manager = CoreManager::new(
            runtime.clone(),
            LocalIpcPolicy::Disable,
            CancellationToken::new(),
        )
        .await
        .unwrap();
        Self {
            state: AppState {
                core_manager,
                hub: EventHub::new(),
                runtime,
                logger: Logger::new(),
            },
            _dir: dir,
        }
    }
}

async fn body_of<T: DeserializeOwned>(response: Response) -> T {
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    serde_json::from_slice(&body).unwrap()
}

async fn post_json<T: serde::Serialize>(state: AppState, path: &str, payload: &T) -> Response {
    create_router(state)
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri(path)
                .header(CONTENT_TYPE, "application/json")
                .body(Body::from(serde_json::to_vec(payload).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap()
}

async fn probe(state: AppState, method: Method, path: &str) -> StatusCode {
    create_router(state)
        .oneshot(
            Request::builder()
                .method(method)
                .uri(path)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap()
        .status()
}

#[tokio::test]
async fn status_reports_injected_runtime_and_log_paths() {
    let env = TestEnv::new().await;
    let response = create_router(env.state.clone())
        .oneshot(
            Request::builder()
                .uri(StatusOp::PATH)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let envelope: StatusRes<'static> = body_of(response).await;
    assert_eq!(envelope.code, ResponseCode::Ok);
    let body = envelope.data.unwrap();
    assert!(matches!(body.core_infos.state, CoreState::Stopped(None)));
    assert_eq!(
        body.runtime_infos.service_data_dir.as_ref(),
        &env.state.runtime.service_data_dir
    );
    assert_eq!(
        body.core_infos.detail,
        Some(CoreStateDetail::Stopped { reason: None })
    );
    let logs = body.logs.expect("status reports log paths");
    assert_eq!(logs.service_dir, crate::utils::dirs::service_logs_dir());
    assert!(logs.core_dir.is_some_and(|path| path.ends_with("logs")));
}

#[tokio::test]
async fn two_states_keep_runtime_and_log_buffers_independent() {
    use std::io::Write;

    let first = TestEnv::new().await;
    let second = TestEnv::new().await;
    assert_ne!(
        first.state.runtime.service_data_dir,
        second.state.runtime.service_data_dir
    );

    let mut first_logger = first.state.logger.clone();
    writeln!(&mut first_logger, "first-only").unwrap();
    assert_eq!(first.state.logger.inspect_logs().len(), 1);
    assert!(second.state.logger.inspect_logs().is_empty());

    let first_status: StatusRes<'static> = body_of(
        create_router(first.state.clone())
            .oneshot(
                Request::builder()
                    .uri(StatusOp::PATH)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap(),
    )
    .await;
    let second_status: StatusRes<'static> = body_of(
        create_router(second.state.clone())
            .oneshot(
                Request::builder()
                    .uri(StatusOp::PATH)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap(),
    )
    .await;
    assert_ne!(
        first_status.data.unwrap().runtime_infos.service_data_dir,
        second_status.data.unwrap().runtime_infos.service_data_dir
    );
}

#[tokio::test]
async fn every_operation_is_mounted_at_its_contract_address() {
    let env = TestEnv::new().await;
    for (method, path) in [
        (StatusOp::METHOD, StatusOp::PATH),
        (CoreStart::METHOD, CoreStart::PATH),
        (CoreStop::METHOD, CoreStop::PATH),
        (CoreRestart::METHOD, CoreRestart::PATH),
        (CoreApply::METHOD, CoreApply::PATH),
        (CoreCheck::METHOD, CoreCheck::PATH),
        (CoreRecover::METHOD, CoreRecover::PATH),
        (CoreV2Submit::METHOD, CoreV2Submit::PATH),
        (CoreV2Operation::METHOD, CoreV2Operation::PATH),
        (CoreV2Status::METHOD, CoreV2Status::PATH),
        (LogsRetrieve::METHOD, LogsRetrieve::PATH),
        (LogsInspect::METHOD, LogsInspect::PATH),
        (NetworkSetDns::METHOD, NetworkSetDns::PATH),
    ] {
        let status = probe(env.state.clone(), method, path).await;
        assert_ne!(status, StatusCode::NOT_FOUND, "{path} not mounted");
        assert_ne!(status, StatusCode::METHOD_NOT_ALLOWED, "wrong method for {path}");
    }
}

#[tokio::test]
async fn unknown_path_and_wrong_method_keep_error_envelopes() {
    let env = TestEnv::new().await;
    let missing = create_router(env.state.clone())
        .oneshot(
            Request::builder()
                .uri("/does/not/exist")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(missing.status(), StatusCode::NOT_FOUND);
    let envelope: CoreStopRes<'static> = body_of(missing).await;
    assert_eq!(envelope.code, ResponseCode::OtherError);
    assert_eq!(envelope.msg, "not found");

    let wrong = create_router(env.state.clone())
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri(StatusOp::PATH)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(wrong.status(), StatusCode::METHOD_NOT_ALLOWED);
    let envelope: CoreStopRes<'static> = body_of(wrong).await;
    assert_eq!(envelope.code, ResponseCode::OtherError);
    assert_eq!(envelope.msg, "method not allowed");
}

#[tokio::test]
async fn stopped_apply_reports_typed_not_started() {
    let env = TestEnv::new().await;
    let core_type = CoreType::Clash(ClashCoreType::Mihomo);
    let data_dir = &env.state.runtime.nyanpasu_data_dir;
    std::fs::write(data_dir.join(core_type.get_executable_name()), b"").unwrap();
    let config = data_dir.join("config.yaml");
    std::fs::write(&config, b"mixed-port: 7890\n").unwrap();

    let response = post_json(
        env.state.clone(),
        CoreApply::PATH,
        &CoreApplyReq {
            core_type: Cow::Borrowed(&core_type),
            config_file: Cow::Borrowed(&config),
            expected_revision: None,
        },
    )
    .await;
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    let envelope: CoreApplyRes<'static> = body_of(response).await;
    assert_eq!(envelope.error_kind.as_deref(), Some("not_started"));
}

#[tokio::test]
async fn missing_config_and_missing_binary_have_distinct_kinds() {
    let env = TestEnv::new().await;
    let core_type = CoreType::Clash(ClashCoreType::Mihomo);
    let missing = env.state.runtime.nyanpasu_data_dir.join("missing.yaml");
    let response = post_json(
        env.state.clone(),
        CoreCheck::PATH,
        &CoreCheckReq {
            core_type: Cow::Borrowed(&core_type),
            config_file: Cow::Borrowed(&missing),
        },
    )
    .await;
    let envelope: CoreCheckRes<'static> = body_of(response).await;
    assert_eq!(envelope.error_kind.as_deref(), Some("config_not_found"));

    let config = env.state.runtime.nyanpasu_data_dir.join("config.yaml");
    std::fs::write(&config, b"mixed-port: 7890\n").unwrap();
    let response = post_json(
        env.state.clone(),
        CoreApply::PATH,
        &CoreApplyReq {
            core_type: Cow::Borrowed(&core_type),
            config_file: Cow::Borrowed(&config),
            expected_revision: None,
        },
    )
    .await;
    let envelope: CoreApplyRes<'static> = body_of(response).await;
    assert_eq!(envelope.error_kind.as_deref(), Some("binary_not_found"));
}

#[tokio::test]
async fn v2_submit_and_long_poll_observe_the_same_operation() {
    let env = TestEnv::new().await;
    let id = "0011223344556677-8899aabb-ccddeeff";
    let submit = post_json(
        env.state.clone(),
        CoreV2Submit::PATH,
        &CoreSubmitReq {
            operation_id: Cow::Borrowed(id),
            command: CoreCommandInfo::Stop,
        },
    )
    .await;
    assert_eq!(submit.status(), StatusCode::OK);
    let envelope: CoreSubmitRes<'static> = body_of(submit).await;
    let admitted = envelope.data.expect("submit returns operation info");
    assert_eq!(admitted.id, id);
    assert!(matches!(
        admitted.phase,
        OperationPhase::Queued | OperationPhase::Running | OperationPhase::Failed
    ));

    let operation = post_json(
        env.state.clone(),
        CoreV2Operation::PATH,
        &CoreOperationReq {
            operation_id: Cow::Borrowed(id),
            wait_ms: Some(2_000),
        },
    )
    .await;
    assert_eq!(operation.status(), StatusCode::OK);
    let envelope: CoreOperationRes<'static> = body_of(operation).await;
    let terminal = envelope.data.expect("operation query returns registry state");
    assert_eq!(terminal.id, id);
    assert_eq!(terminal.phase, OperationPhase::Failed);
    assert_eq!(
        terminal.error.as_ref().and_then(|error| error.kind.as_deref()),
        Some("not_started")
    );
}

#[tokio::test]
async fn v2_rejects_the_ref_test_only_contiguous_operation_id_format() {
    let env = TestEnv::new().await;
    let response = post_json(
        env.state.clone(),
        CoreV2Submit::PATH,
        &CoreSubmitReq {
            operation_id: Cow::Borrowed("00112233445566778899aabbccddeeff"),
            command: CoreCommandInfo::Stop,
        },
    )
    .await;
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    let envelope: CoreSubmitRes<'static> = body_of(response).await;
    assert_eq!(envelope.code, ResponseCode::OtherError);
    assert!(envelope.msg.contains("malformed operation id"));
}

#[tokio::test]
async fn v2_status_is_additive_and_reports_the_same_snapshot() {
    let env = TestEnv::new().await;
    let response = create_router(env.state.clone())
        .oneshot(
            Request::builder()
                .uri(CoreV2Status::PATH)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let envelope: chimera_ipc::api::R<'static, chimera_ipc::api::status::CoreInfos> =
        body_of(response).await;
    assert!(matches!(
        envelope.data.unwrap().state,
        CoreState::Stopped(None)
    ));
}

#[tokio::test]
async fn recovery_without_quarantine_is_idempotent() {
    let env = TestEnv::new().await;
    let response = create_router(env.state.clone())
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri(CoreRecover::PATH)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let envelope: CoreRecoverRes<'static> = body_of(response).await;
    assert_eq!(envelope.code, ResponseCode::Ok);
    assert!(envelope.error_kind.is_none());
}
