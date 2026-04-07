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
}
