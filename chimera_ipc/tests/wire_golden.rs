use std::{borrow::Cow, sync::Arc};

use chimera_ipc::api::{
    R, ResponseCode,
    core::apply::{ApplyOutcomeKind, CoreApplyData, CoreApplyReq},
    status::{
        ConfigRevisionInfo, CoreInfos, CoreState, CoreStateDetail, RevisionIdInfo, StatusResBody,
    },
    ws::events::{ClashCoreKind, Event, LogField, LogFrame, LogLevel, LogStream, LogTimestamp},
};
use chimera_utils::core::{ClashCoreType, CoreType};
use serde_json::json;

#[test]
fn legacy_success_envelope_omits_error_kind() {
    let envelope = R {
        code: ResponseCode::Ok,
        msg: Cow::Borrowed("ok"),
        data: Some(()),
        ts: 42,
        error_kind: None,
        retryable: None,
    };
    assert_eq!(
        serde_json::to_value(envelope).unwrap(),
        json!({"code":"Ok","msg":"ok","data":null,"ts":42})
    );
}

#[test]
fn classified_error_envelope_appends_error_kind() {
    let envelope: R<'_, ()> = R {
        code: ResponseCode::OtherError,
        msg: Cow::Borrowed("missing config"),
        data: None,
        ts: 42,
        error_kind: Some(Cow::Borrowed("config_not_found")),
        retryable: None,
    };
    assert_eq!(
        serde_json::to_value(envelope).unwrap(),
        json!({
            "code":"OtherError",
            "msg":"missing config",
            "data":null,
            "ts":42,
            "error_kind":"config_not_found"
        })
    );
}

#[test]
fn legacy_core_infos_decode_without_s7_fields() {
    let infos: CoreInfos = serde_json::from_value(json!({
        "type": null,
        "state": {"Stopped": null},
        "state_changed_at": 42,
        "config_path": null
    }))
    .unwrap();
    assert!(infos.controller.is_none());
    assert!(infos.health.is_none());
    assert!(infos.revision.is_none());
    assert!(infos.detail.is_none());
}

#[test]
fn absent_optional_status_fields_stay_absent_on_the_wire() {
    let infos = CoreInfos {
        r#type: None,
        state: CoreState::Stopped(None),
        state_changed_at: 42,
        config_path: None,
        controller: None,
        health: None,
        revision: None,
        detail: None,
    };
    assert_eq!(
        serde_json::to_value(infos).unwrap(),
        json!({
            "type":null,
            "state":{"Stopped":null},
            "state_changed_at":42,
            "config_path":null
        })
    );
}

#[test]
fn full_status_snapshot_event_has_the_target_variant_name() {
    let event = Event::new_core_status_changed(CoreInfos {
        r#type: Some(CoreType::Clash(ClashCoreType::Mihomo)),
        state: CoreState::Running,
        state_changed_at: 42,
        config_path: Some("config.yaml".into()),
        controller: None,
        health: None,
        revision: Some(ConfigRevisionInfo {
            epoch: 3,
            generation: 7,
            source_hash: "source".into(),
            effective_hash: "effective".into(),
        }),
        detail: Some(CoreStateDetail::Running { epoch: 3, pid: 9 }),
    });
    assert_eq!(
        serde_json::to_value(event).unwrap(),
        json!({
            "CoreStatusChanged": {
                "type":{"clash":"mihomo"},
                "state":"Running",
                "state_changed_at":42,
                "config_path":"config.yaml",
                "revision":{
                    "epoch":3,
                    "generation":7,
                    "source_hash":"source",
                    "effective_hash":"effective"
                },
                "detail":{"Running":{"epoch":3,"pid":9}}
            }
        })
    );
}

#[test]
fn core_log_event_serializes_the_shared_frame_without_arc_artifacts() {
    let event = Event::new_core_log(Arc::new(LogFrame {
        at: 1_700_000_000_000,
        epoch: 7,
        kind: ClashCoreKind::Mihomo,
        stream: LogStream::Stderr,
        level: LogLevel::Error,
        timestamp: Some(LogTimestamp {
            raw: "2026-08-16T10:00:00Z".into(),
            unix_ms: Some(1_700_000_000_001),
            inferred: false,
        }),
        target: Some("config".into()),
        message: "bad config".into(),
        fields: vec![LogField {
            key: "line".into(),
            value: "3".into(),
        }],
        raw: "bad config line=3".into(),
        truncated: false,
    }));
    assert_eq!(
        serde_json::to_value(event).unwrap(),
        json!({
            "CoreLog": {
                "at":1_700_000_000_000_i64,
                "epoch":7,
                "kind":"mihomo",
                "stream":"stderr",
                "level":"error",
                "timestamp":{
                    "raw":"2026-08-16T10:00:00Z",
                    "unix_ms":1_700_000_000_001_i64,
                    "inferred":false
                },
                "target":"config",
                "message":"bad config",
                "fields":[{"key":"line","value":"3"}],
                "raw":"bad config line=3",
                "truncated":false
            }
        })
    );
}

#[test]
fn pre_log_paths_status_payload_still_decodes() {
    let body: StatusResBody<'static> = serde_json::from_value(json!({
        "version":"1.8.1",
        "core_infos":{
            "type":null,
            "state":{"Stopped":null},
            "state_changed_at":42,
            "config_path":null
        },
        "runtime_infos":{
            "service_data_dir":"service-data",
            "service_config_dir":"service-config",
            "nyanpasu_config_dir":"nyanpasu-config",
            "nyanpasu_data_dir":"nyanpasu-data"
        }
    }))
    .unwrap();
    assert!(body.logs.is_none());
}

#[test]
fn apply_request_carries_the_optional_revision_triple() {
    let core_type = CoreType::Clash(ClashCoreType::Mihomo);
    let config_file = std::path::PathBuf::from("config.yaml");
    let request = CoreApplyReq {
        core_type: Cow::Borrowed(&core_type),
        config_file: Cow::Borrowed(&config_file),
        expected_revision: Some(RevisionIdInfo {
            epoch: 3,
            generation: 7,
            effective_hash: "effective".into(),
        }),
    };
    assert_eq!(
        serde_json::to_value(request).unwrap(),
        json!({
            "core_type":{"clash":"mihomo"},
            "config_file":"config.yaml",
            "expected_revision":{
                "epoch":3,
                "generation":7,
                "effective_hash":"effective"
            }
        })
    );
}

#[test]
fn rolled_back_apply_is_a_success_payload_not_a_transport_error() {
    let data = CoreApplyData {
        outcome: ApplyOutcomeKind::RolledBack,
        revision: ConfigRevisionInfo {
            epoch: 2,
            generation: 4,
            source_hash: "old-source".into(),
            effective_hash: "old-effective".into(),
        },
        warning: Some("restored previous process".into()),
        failed_apply: Some("desired config failed".into()),
    };
    assert_eq!(
        serde_json::to_value(data).unwrap(),
        json!({
            "outcome":"rolled_back",
            "revision":{
                "epoch":2,
                "generation":4,
                "source_hash":"old-source",
                "effective_hash":"old-effective"
            },
            "warning":"restored previous process",
            "failed_apply":"desired config failed"
        })
    );
}
