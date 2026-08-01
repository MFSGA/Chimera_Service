//! The single source of truth for the IPC operation set.
//!
//! Each operation names its HTTP method, endpoint path, request body type and
//! response payload type. Both clients and servers can consume this contract,
//! preventing route definitions from drifting apart.

use std::fmt::Debug;

use http::Method;
use serde::{Serialize, de::DeserializeOwned};

use super::{
    R,
    core::{
        apply::{CORE_APPLY_ENDPOINT, CoreApplyData},
        check::CORE_CHECK_ENDPOINT,
        recover::CORE_RECOVER_ENDPOINT,
        restart::CORE_RESTART_ENDPOINT,
        start::CORE_START_ENDPOINT,
        stop::CORE_STOP_ENDPOINT,
    },
    log::{LOGS_INSPECT_ENDPOINT, LOGS_RETRIEVE_ENDPOINT, LogsResBody},
    network::set_dns::{NETWORK_SET_DNS_ENDPOINT, NetworkSetDnsReq},
    status::{STATUS_ENDPOINT, StatusResBody},
};

/// One request/response IPC operation and its wire-level body types.
pub trait IpcOperation {
    const METHOD: Method;
    const PATH: &'static str;
    type Req<'a>: Serialize;
    type Data: Serialize + DeserializeOwned + Debug;
}

/// The response envelope decoded for an operation.
pub type OpResponse<Op> = R<'static, <Op as IpcOperation>::Data>;

/// `GET /status`
pub struct Status;

impl IpcOperation for Status {
    const METHOD: Method = Method::GET;
    const PATH: &'static str = STATUS_ENDPOINT;
    type Req<'a> = ();
    type Data = StatusResBody<'static>;
}

/// `POST /core/start`
pub struct CoreStart;

impl IpcOperation for CoreStart {
    const METHOD: Method = Method::POST;
    const PATH: &'static str = CORE_START_ENDPOINT;
    type Req<'a> = super::core::start::CoreStartReq<'a>;
    type Data = ();
}

/// `POST /core/stop`
pub struct CoreStop;

impl IpcOperation for CoreStop {
    const METHOD: Method = Method::POST;
    const PATH: &'static str = CORE_STOP_ENDPOINT;
    type Req<'a> = ();
    type Data = ();
}

/// `POST /core/restart`
pub struct CoreRestart;

impl IpcOperation for CoreRestart {
    const METHOD: Method = Method::POST;
    const PATH: &'static str = CORE_RESTART_ENDPOINT;
    type Req<'a> = ();
    type Data = ();
}

/// `POST /core/apply`
pub struct CoreApply;

impl IpcOperation for CoreApply {
    const METHOD: Method = Method::POST;
    const PATH: &'static str = CORE_APPLY_ENDPOINT;
    type Req<'a> = super::core::apply::CoreApplyReq<'a>;
    type Data = CoreApplyData;
}

/// `POST /core/check`
pub struct CoreCheck;

impl IpcOperation for CoreCheck {
    const METHOD: Method = Method::POST;
    const PATH: &'static str = CORE_CHECK_ENDPOINT;
    type Req<'a> = super::core::check::CoreCheckReq<'a>;
    type Data = ();
}

/// `POST /core/recover`
pub struct CoreRecover;

impl IpcOperation for CoreRecover {
    const METHOD: Method = Method::POST;
    const PATH: &'static str = CORE_RECOVER_ENDPOINT;
    type Req<'a> = ();
    type Data = ();
}

/// `GET /logs/retrieve`
pub struct LogsRetrieve;

impl IpcOperation for LogsRetrieve {
    const METHOD: Method = Method::GET;
    const PATH: &'static str = LOGS_RETRIEVE_ENDPOINT;
    type Req<'a> = ();
    type Data = LogsResBody<'static>;
}

/// `GET /logs/inspect`
pub struct LogsInspect;

impl IpcOperation for LogsInspect {
    const METHOD: Method = Method::GET;
    const PATH: &'static str = LOGS_INSPECT_ENDPOINT;
    type Req<'a> = ();
    type Data = LogsResBody<'static>;
}

/// `POST /network/set_dns`
pub struct NetworkSetDns;

impl IpcOperation for NetworkSetDns {
    const METHOD: Method = Method::POST;
    const PATH: &'static str = NETWORK_SET_DNS_ENDPOINT;
    type Req<'a> = NetworkSetDnsReq<'a>;
    type Data = ();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_operation_keeps_its_legacy_path_and_method() {
        assert_eq!((Status::METHOD, Status::PATH), (Method::GET, "/status"));
        assert_eq!(
            (CoreStart::METHOD, CoreStart::PATH),
            (Method::POST, "/core/start")
        );
        assert_eq!(
            (CoreStop::METHOD, CoreStop::PATH),
            (Method::POST, "/core/stop")
        );
        assert_eq!(
            (CoreRestart::METHOD, CoreRestart::PATH),
            (Method::POST, "/core/restart")
        );
        assert_eq!(
            (LogsRetrieve::METHOD, LogsRetrieve::PATH),
            (Method::GET, "/logs/retrieve")
        );
        assert_eq!(
            (LogsInspect::METHOD, LogsInspect::PATH),
            (Method::GET, "/logs/inspect")
        );
        assert_eq!(
            (NetworkSetDns::METHOD, NetworkSetDns::PATH),
            (Method::POST, "/network/set_dns")
        );
        assert_eq!(
            (CoreApply::METHOD, CoreApply::PATH),
            (Method::POST, "/core/apply")
        );
        assert_eq!(
            (CoreCheck::METHOD, CoreCheck::PATH),
            (Method::POST, "/core/check")
        );
        assert_eq!(
            (CoreRecover::METHOD, CoreRecover::PATH),
            (Method::POST, "/core/recover")
        );
    }
}
