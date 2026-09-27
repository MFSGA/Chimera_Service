use std::{borrow::Cow, sync::OnceLock};

use axum::body::Body;
use bytes::Bytes;
use http_body_util::Empty;
use hyper::{Request, header::CONTENT_TYPE};

use crate::{SERVICE_PLACEHOLDER, api, client::send_request};

use super::ClientError;

use std::result::Result as StdResult;

pub struct Client<'a>(Cow<'a, str>);

type Result<'a, T, E = ClientError<'a>> = StdResult<T, E>;

impl<'a> Client<'a> {
    pub fn new(placeholder: &'a str) -> Self {
        Self(Cow::Borrowed(placeholder))
    }

    pub fn service_default() -> &'static Client<'static> {
        static CLIENT: OnceLock<Client<'static>> = OnceLock::new();
        CLIENT.get_or_init(|| Client::new(SERVICE_PLACEHOLDER))
    }

    pub async fn status(&self) -> Result<'_, api::status::StatusResBody<'_>> {
        let request = Request::get(api::status::STATUS_ENDPOINT).body(Empty::<Bytes>::new())?;
        let response = send_request(&self.0, request)
            .await?
            .cast_body::<api::status::StatusRes<'_>>()
            .await?
            .ok()?;
        let data = response.data.unwrap();
        Ok(data)
    }

    pub async fn start_core(&self, payload: &api::core::start::CoreStartReq<'_>) -> Result<'_, ()> {
        let payload = simd_json::serde::to_string(payload)?;
        let request = Request::post(api::core::start::CORE_START_ENDPOINT)
            .header(CONTENT_TYPE, "application/json")
            .body(Body::from(payload))?;
        let response = send_request(&self.0, request)
            .await?
            .cast_body::<api::core::start::CoreStartRes>()
            .await?;
        response.ok()?;
        Ok(())
    }

    pub async fn stop_core(&self) -> Result<'_, ()> {
        let request =
            Request::post(api::core::stop::CORE_STOP_ENDPOINT).body(Empty::<Bytes>::new())?;
        let response = send_request(&self.0, request)
            .await?
            .cast_body::<api::core::stop::CoreStopRes>()
            .await?;
        response.ok()?;
        Ok(())
    }

    pub async fn restart_core(&self) -> Result<'_, ()> {
        let request =
            Request::post(api::core::restart::CORE_RESTART_ENDPOINT).body(Empty::<Bytes>::new())?;
        let response = send_request(&self.0, request)
            .await?
            .cast_body::<api::core::restart::CoreRestartRes>()
            .await?;
        response.ok()?;
        Ok(())
    }

    pub async fn submit_core(
        &self,
        payload: &api::core::v2::CoreSubmitReq<'_>,
    ) -> Result<'_, api::core::v2::OperationInfo> {
        let payload = simd_json::serde::to_string(payload)?;
        let request = Request::post(api::core::v2::CORE_V2_SUBMIT_ENDPOINT)
            .header(CONTENT_TYPE, "application/json")
            .body(Body::from(payload))?;
        let response = send_request(&self.0, request)
            .await?
            .cast_body::<api::core::v2::CoreSubmitRes<'static>>()
            .await?;
        Ok(response.ok()?.data.unwrap())
    }

    pub async fn core_operation(
        &self,
        payload: &api::core::v2::CoreOperationReq<'_>,
    ) -> Result<'_, api::core::v2::OperationInfo> {
        let payload = simd_json::serde::to_string(payload)?;
        let request = Request::post(api::core::v2::CORE_V2_OPERATION_ENDPOINT)
            .header(CONTENT_TYPE, "application/json")
            .body(Body::from(payload))?;
        let response = send_request(&self.0, request)
            .await?
            .cast_body::<api::core::v2::CoreOperationRes<'static>>()
            .await?;
        Ok(response.ok()?.data.unwrap())
    }

    pub async fn core_status_v2(&self) -> Result<'_, api::status::CoreInfos> {
        let request =
            Request::get(api::core::v2::CORE_V2_STATUS_ENDPOINT).body(Empty::<Bytes>::new())?;
        let response = send_request(&self.0, request)
            .await?
            .cast_body::<api::core::v2::CoreStatusRes<'static>>()
            .await?;
        Ok(response.ok()?.data.unwrap())
    }

    pub async fn core_api_connection(
        &self,
    ) -> Result<'_, Option<api::core::v2::CoreApiConnection>> {
        let request = Request::get(api::core::v2::CORE_V2_API_CONNECTION_ENDPOINT)
            .body(Empty::<Bytes>::new())?;
        let response = send_request(&self.0, request)
            .await?
            .cast_body::<api::core::v2::CoreApiConnectionRes<'static>>()
            .await?;
        Ok(response.ok()?.data.flatten())
    }

    pub async fn effective_config_v2(
        &self,
    ) -> Result<'_, Option<api::core::v2::CoreEffectiveConfig>> {
        let request = Request::get(api::core::v2::CORE_V2_EFFECTIVE_CONFIG_ENDPOINT)
            .body(Empty::<Bytes>::new())?;
        let response = send_request(&self.0, request)
            .await?
            .cast_body::<api::core::v2::CoreEffectiveConfigRes<'static>>()
            .await?;
        Ok(response.ok()?.data.flatten())
    }

    pub async fn inspect_logs(&self) -> Result<'_, api::log::LogsResBody<'_>> {
        let request = Request::get(api::log::LOGS_INSPECT_ENDPOINT).body(Empty::<Bytes>::new())?;
        let response = send_request(&self.0, request)
            .await?
            .cast_body::<api::log::LogsRes<'_>>()
            .await?
            .ok()?;
        let data = response.data.unwrap();
        Ok(data)
    }

    pub async fn retrieve_logs(&self) -> Result<'_, api::log::LogsResBody<'_>> {
        let request = Request::get(api::log::LOGS_RETRIEVE_ENDPOINT).body(Empty::<Bytes>::new())?;
        let response = send_request(&self.0, request)
            .await?
            .cast_body::<api::log::LogsRes<'_>>()
            .await?
            .ok()?;
        let data = response.data.unwrap();
        Ok(data)
    }

    pub async fn set_dns(
        &self,
        payload: &api::network::set_dns::NetworkSetDnsReq<'_>,
    ) -> Result<'_, ()> {
        let payload = simd_json::serde::to_string(payload)?;
        let request = Request::post(api::network::set_dns::NETWORK_SET_DNS_ENDPOINT)
            .header(CONTENT_TYPE, "application/json")
            .body(Body::from(payload))?;
        let response = send_request(&self.0, request)
            .await?
            .cast_body::<api::network::set_dns::NetworkSetDnsRes>()
            .await?;
        response.ok()?;
        Ok(())
    }
}
