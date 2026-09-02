use std::{
    borrow::Cow,
    pin::Pin,
    sync::OnceLock,
    task::{Context, Poll},
};

use axum::body::Body;
use futures_util::{Stream, StreamExt};
use hyper::{
    Method, Request,
    header::{
        CONNECTION, CONTENT_TYPE, SEC_WEBSOCKET_KEY, SEC_WEBSOCKET_VERSION, UPGRADE,
    },
};
use tokio_tungstenite::tungstenite::{Message, handshake::client::generate_key};

use crate::{
    SERVICE_PLACEHOLDER,
    api::{
        self,
        contract::{
            CoreApply, CoreCheck, CoreRecover, CoreRestart, CoreStart, CoreStop, CoreV2Operation,
            CoreV2Status, CoreV2Submit, IpcOperation, LogsInspect, LogsRetrieve, NetworkSetDns,
            OpResponse, Status,
        },
        ws::events::{EVENT_URI, Event},
    },
    client::{open_websocket, send_request},
};

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

    async fn call<Op>(&self, body: Option<&Op::Req<'_>>) -> Result<'_, OpResponse<Op>>
    where
        Op: IpcOperation,
    {
        let mut request = Request::builder().method(Op::METHOD).uri(Op::PATH);
        let body = match body {
            Some(body) => {
                request = request.header(CONTENT_TYPE, "application/json");
                Body::from(simd_json::serde::to_string(body)?)
            }
            None => Body::empty(),
        };
        send_request(&self.0, request.body(body)?)
            .await?
            .cast_body::<OpResponse<Op>>()
            .await
    }

    pub async fn status(&self) -> Result<'_, api::status::StatusResBody<'static>> {
        self.call::<Status>(None)
            .await?
            .ok()?
            .data
            .ok_or(ClientError::EmptyData {
                operation: Status::PATH,
            })
    }

    pub async fn start_core(&self, payload: &api::core::start::CoreStartReq<'_>) -> Result<'_, ()> {
        self.call::<CoreStart>(Some(payload)).await?.ok()?;
        Ok(())
    }

    pub async fn stop_core(&self) -> Result<'_, ()> {
        self.call::<CoreStop>(None).await?.ok()?;
        Ok(())
    }

    pub async fn restart_core(&self) -> Result<'_, ()> {
        self.call::<CoreRestart>(None).await?.ok()?;
        Ok(())
    }

    pub async fn check_core(&self, payload: &api::core::check::CoreCheckReq<'_>) -> Result<'_, ()> {
        self.call::<CoreCheck>(Some(payload)).await?.ok()?;
        Ok(())
    }

    pub async fn apply_core(
        &self,
        payload: &api::core::apply::CoreApplyReq<'_>,
    ) -> Result<'_, api::core::apply::CoreApplyData> {
        self.call::<CoreApply>(Some(payload))
            .await?
            .ok()?
            .data
            .ok_or(ClientError::EmptyData {
                operation: CoreApply::PATH,
            })
    }

    pub async fn submit_core(
        &self,
        payload: &api::core::v2::CoreSubmitReq<'_>,
    ) -> Result<'_, api::core::v2::OperationInfo> {
        self.call::<CoreV2Submit>(Some(payload))
            .await?
            .ok()?
            .data
            .ok_or(ClientError::EmptyData {
                operation: CoreV2Submit::PATH,
            })
    }

    pub async fn core_operation(
        &self,
        payload: &api::core::v2::CoreOperationReq<'_>,
    ) -> Result<'_, api::core::v2::OperationInfo> {
        self.call::<CoreV2Operation>(Some(payload))
            .await?
            .ok()?
            .data
            .ok_or(ClientError::EmptyData {
                operation: CoreV2Operation::PATH,
            })
    }

    pub async fn core_status_v2(&self) -> Result<'_, api::status::CoreInfos> {
        self.call::<CoreV2Status>(None)
            .await?
            .ok()?
            .data
            .ok_or(ClientError::EmptyData {
                operation: CoreV2Status::PATH,
            })
    }

    pub async fn recover_core(&self) -> Result<'_, ()> {
        self.call::<CoreRecover>(None).await?.ok()?;
        Ok(())
    }

    pub async fn inspect_logs(&self) -> Result<'_, api::log::LogsResBody<'static>> {
        self.call::<LogsInspect>(None)
            .await?
            .ok()?
            .data
            .ok_or(ClientError::EmptyData {
                operation: LogsInspect::PATH,
            })
    }

    pub async fn retrieve_logs(&self) -> Result<'_, api::log::LogsResBody<'static>> {
        self.call::<LogsRetrieve>(None)
            .await?
            .ok()?
            .data
            .ok_or(ClientError::EmptyData {
                operation: LogsRetrieve::PATH,
            })
    }

    pub async fn set_dns(
        &self,
        payload: &api::network::set_dns::NetworkSetDnsReq<'_>,
    ) -> Result<'_, ()> {
        self.call::<NetworkSetDns>(Some(payload)).await?.ok()?;
        Ok(())
    }

    /// Subscribe to the unversioned event stream. The first frame is a full
    /// `CoreStatusChanged` snapshot, and another snapshot follows lag recovery.
    pub async fn events(&self) -> Result<'_, EventStream> {
        let request = Request::builder()
            .method(Method::GET)
            .uri(EVENT_URI)
            .header(CONNECTION, "upgrade")
            .header(UPGRADE, "websocket")
            .header(SEC_WEBSOCKET_VERSION, "13")
            .header(SEC_WEBSOCKET_KEY, generate_key())
            .body(Body::empty())?;
        let websocket = open_websocket(&self.0, request).await?;
        let stream = websocket.filter_map(|message| async move {
            let mut bytes = match message {
                Ok(Message::Binary(bytes)) => bytes.to_vec(),
                Ok(Message::Text(text)) => text.as_bytes().to_vec(),
                Ok(_) => return None,
                Err(source) => {
                    return Some(Err(ClientError::Other(anyhow::Error::new(source))));
                }
            };
            Some(
                simd_json::serde::from_slice(&mut bytes).map_err(ClientError::ParseFailed),
            )
        });
        Ok(EventStream {
            inner: Box::pin(stream),
        })
    }
}

/// A stream of typed events pushed by the service.
pub struct EventStream {
    inner: Pin<Box<dyn Stream<Item = StdResult<Event, ClientError<'static>>> + Send>>,
}

impl Stream for EventStream {
    type Item = StdResult<Event, ClientError<'static>>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.inner.poll_next_unpin(cx)
    }
}

impl std::fmt::Debug for EventStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EventStream").finish_non_exhaustive()
    }
}
