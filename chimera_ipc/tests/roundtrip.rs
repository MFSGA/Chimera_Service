use std::{borrow::Cow, net::IpAddr, path::PathBuf, sync::Arc};

use chimera_ipc::api::{
    R, ResponseCode,
    core::{
        apply::{ApplyOutcomeKind, CoreApplyData, CoreApplyReq},
        check::CoreCheckReq,
        start::CoreStartReq,
    },
    network::set_dns::NetworkSetDnsReq,
    status::{
        ConfigRevisionInfo, CoreControllerInfo, CoreHealthInfo, CoreHealthState, CoreInfos,
        CoreState, CoreStateDetail, RevisionIdInfo,
    },
    ws::events::{ClashCoreKind, Event, LogField, LogFrame, LogLevel, LogStream, LogTimestamp},
};
use chimera_utils::core::{ClashCoreType, CoreType};
use serde::{Serialize, de::DeserializeOwned};

fn assert_json_roundtrip<T>(value: &T)
where
    T: Serialize + DeserializeOwned,
{
    let encoded = serde_json::to_vec(value).unwrap();
    let decoded: T = serde_json::from_slice(&encoded).unwrap();
    assert_eq!(
        serde_json::to_value(value).unwrap(),
        serde_json::to_value(decoded).unwrap()
    );
}

fn mihomo() -> CoreType {
    CoreType::Clash(ClashCoreType::Mihomo)
}

#[test]
fn response_envelopes_roundtrip_with_and_without_classification() {
    assert_json_roundtrip(&R {
        code: ResponseCode::Ok,
        msg: Cow::Borrowed("ok"),
        data: Some("payload".to_string()),
        ts: 42,
        error_kind: None,
        retryable: None,
    });
    assert_json_roundtrip(&R::<()> {
        code: ResponseCode::OtherError,
        msg: Cow::Borrowed("missing binary"),
        data: None,
        ts: 43,
        error_kind: Some(Cow::Borrowed("binary_not_found")),
        retryable: Some(false),
    });
}

#[test]
fn all_core_control_requests_roundtrip() {
    let core_type = mihomo();
    let config_file = PathBuf::from("config.yaml");
    assert_json_roundtrip(&CoreStartReq {
        core_type: Cow::Borrowed(&core_type),
        config_file: Cow::Borrowed(&config_file),
    });
    assert_json_roundtrip(&CoreCheckReq {
        core_type: Cow::Borrowed(&core_type),
        config_file: Cow::Borrowed(&config_file),
    });
    assert_json_roundtrip(&CoreApplyReq {
        core_type: Cow::Borrowed(&core_type),
        config_file: Cow::Borrowed(&config_file),
        expected_revision: Some(RevisionIdInfo {
            epoch: 3,
            generation: 7,
            effective_hash: "effective".into(),
        }),
    });
}

#[test]
fn apply_outcomes_roundtrip() {
    for outcome in [
        ApplyOutcomeKind::Noop,
        ApplyOutcomeKind::Patched,
        ApplyOutcomeKind::Reloaded,
        ApplyOutcomeKind::Restarted,
        ApplyOutcomeKind::Switched,
        ApplyOutcomeKind::RolledBack,
    ] {
        assert_json_roundtrip(&CoreApplyData {
            outcome,
            revision: ConfigRevisionInfo {
                epoch: 3,
                generation: 7,
                source_hash: "source".into(),
                effective_hash: "effective".into(),
            },
            warning: Some("warning".into()),
            failed_apply: Some("failure".into()),
        });
    }
}

#[test]
fn enriched_status_roundtrips_every_optional_field() {
    assert_json_roundtrip(&CoreInfos {
        r#type: Some(mihomo()),
        state: CoreState::Running,
        state_changed_at: 42,
        config_path: Some("config.yaml".into()),
        controller: Some(CoreControllerInfo::NamedPipe("core.pipe".into())),
        health: Some(CoreHealthInfo {
            state: CoreHealthState::Healthy,
            changed_at: 40,
            consecutive_failures: 0,
            last_error: None,
            last_success_at: Some(41),
        }),
        revision: Some(ConfigRevisionInfo {
            epoch: 3,
            generation: 7,
            source_hash: "source".into(),
            effective_hash: "effective".into(),
        }),
        detail: Some(CoreStateDetail::Running { epoch: 3, pid: 9 }),
    });
}

#[test]
fn every_lifecycle_detail_roundtrips() {
    for detail in [
        CoreStateDetail::Stopped {
            reason: Some("user".into()),
        },
        CoreStateDetail::Starting { epoch: 1 },
        CoreStateDetail::Running { epoch: 1, pid: 9 },
        CoreStateDetail::Restarting {
            epoch: 1,
            attempt: 2,
        },
        CoreStateDetail::Switching {
            from: Some(1),
            to: 2,
        },
        CoreStateDetail::Stopping { epoch: 2 },
    ] {
        assert_json_roundtrip(&detail);
    }
}

#[test]
fn every_event_variant_roundtrips() {
    assert_json_roundtrip(&Event::new_core_log(Arc::new(LogFrame {
        at: 1_753_719_382_646,
        epoch: 3,
        kind: ClashCoreKind::Mihomo,
        stream: LogStream::Stdout,
        level: LogLevel::Info,
        timestamp: Some(LogTimestamp {
            raw: "2026-08-01T00:00:00Z".into(),
            unix_ms: Some(1_753_987_200_000),
            inferred: false,
        }),
        target: Some("mihomo".into()),
        message: "started".into(),
        fields: vec![LogField {
            key: "request_id".into(),
            value: "abc".into(),
        }],
        raw: "started".into(),
        truncated: false,
    })));
    assert_json_roundtrip(&Event::new_core_state_changed(CoreState::Running));
    assert_json_roundtrip(&Event::new_core_status_changed(CoreInfos {
        r#type: None,
        state: CoreState::Stopped(None),
        state_changed_at: 42,
        config_path: None,
        controller: None,
        health: None,
        revision: None,
        detail: Some(CoreStateDetail::Stopped { reason: None }),
    }));
}

#[test]
fn dns_request_roundtrips_ipv4_ipv6_and_clear() {
    assert_json_roundtrip(&NetworkSetDnsReq {
        dns_servers: Some(vec![
            Cow::Owned("1.1.1.1".parse::<IpAddr>().unwrap()),
            Cow::Owned("2606:4700:4700::1111".parse::<IpAddr>().unwrap()),
        ]),
    });
    assert_json_roundtrip(&NetworkSetDnsReq { dns_servers: None });
}
