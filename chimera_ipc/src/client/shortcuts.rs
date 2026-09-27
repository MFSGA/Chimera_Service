use std::{
    borrow::Cow,
    pin::Pin,
    sync::OnceLock,
    task::{Context, Poll},
    time::Duration,
};

use axum::body::Body;
use backon::{BackoffBuilder, ExponentialBuilder};
use bytes::Bytes;
use futures_util::{SinkExt, Stream, StreamExt};
use http_body_util::Empty;
use hyper::{Request, header::CONTENT_TYPE};
use interprocess::local_socket::tokio::{Stream as LocalSocketStream, prelude::*};
use tokio_tungstenite::{WebSocketStream, client_async, tungstenite::Message};

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

    /// Subscribe to events pushed by the Service over `/ws/events`.
    ///
    /// The connection uses the same local socket as HTTP requests. Windows
    /// named-pipe busy errors are retried before the WebSocket handshake starts;
    /// an established stream or a handshake failure is never replayed.
    pub async fn events(&self) -> Result<'_, EventStream> {
        const EVENT_URL: &str = "ws://chimera-service.localipc/ws/events";

        let websocket = self.connect_event_stream(EVENT_URL).await?;
        let events = futures_util::stream::unfold(
            (websocket, false),
            |(mut websocket, closed)| async move {
                if closed {
                    return None;
                }
                loop {
                    match websocket.next().await {
                        Some(Ok(Message::Text(text))) => {
                            return Some((decode_event(text.as_bytes()), (websocket, false)));
                        }
                        Some(Ok(Message::Binary(bytes))) => {
                            return Some((decode_event(&bytes), (websocket, false)));
                        }
                        Some(Ok(Message::Ping(payload))) => {
                            if let Err(source) = websocket.send(Message::Pong(payload)).await {
                                return Some((
                                    Err(ClientError::WebSocket {
                                        operation: api::ws::events::EVENT_URI,
                                        source,
                                    }),
                                    (websocket, true),
                                ));
                            }
                        }
                        Some(Ok(Message::Pong(_))) => {}
                        Some(Ok(Message::Close(_))) | None => return None,
                        Some(Ok(_)) => {}
                        Some(Err(source)) => {
                            return Some((
                                Err(ClientError::WebSocket {
                                    operation: api::ws::events::EVENT_URI,
                                    source,
                                }),
                                (websocket, true),
                            ));
                        }
                    }
                }
            },
        );
        Ok(EventStream {
            inner: Box::pin(events),
        })
    }

    async fn connect_event_stream(
        &self,
        uri: &'static str,
    ) -> Result<'_, WebSocketStream<LocalSocketStream>> {
        let started = tokio::time::Instant::now();
        let mut backoff = ExponentialBuilder::default()
            .with_min_delay(Duration::from_millis(50))
            .with_max_delay(Duration::from_millis(200))
            .with_jitter()
            .without_max_times()
            .build();

        loop {
            let name = crate::utils::get_name(&self.0)?;
            match LocalSocketStream::connect(name).await {
                Ok(socket) => {
                    return client_async(uri, socket)
                        .await
                        .map(|(websocket, _response)| websocket)
                        .map_err(|source| ClientError::WebSocket {
                            operation: api::ws::events::EVENT_URI,
                            source,
                        });
                }
                Err(error) if cfg!(windows) && error.raw_os_error() == Some(231) => {
                    let Some(delay) = backoff.next() else {
                        return Err(error.into());
                    };
                    if started.elapsed() + delay > Duration::from_secs(1) {
                        return Err(error.into());
                    }
                    tokio::time::sleep(delay).await;
                }
                Err(error) => return Err(error.into()),
            }
        }
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

    pub async fn check_config(
        &self,
        payload: &api::core::check::CoreCheckReq<'_>,
    ) -> Result<'_, ()> {
        let payload = simd_json::serde::to_string(payload)?;
        let request = Request::post(api::core::check::CORE_CHECK_ENDPOINT)
            .header(CONTENT_TYPE, "application/json")
            .body(Body::from(payload))?;
        let response = send_request(&self.0, request)
            .await?
            .cast_body::<api::core::check::CoreCheckRes>()
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

fn decode_event(bytes: &[u8]) -> Result<'static, api::ws::events::Event> {
    serde_json::from_slice(bytes).map_err(|source| ClientError::EventDecode {
        operation: api::ws::events::EVENT_URI,
        source,
    })
}

/// A decoded event stream from the local IPC Service endpoint.
pub struct EventStream {
    inner: Pin<Box<dyn Stream<Item = Result<'static, api::ws::events::Event>> + Send>>,
}

impl Stream for EventStream {
    type Item = Result<'static, api::ws::events::Event>;

    fn poll_next(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.inner.poll_next_unpin(context)
    }
}

impl std::fmt::Debug for EventStream {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("EventStream")
            .finish_non_exhaustive()
    }
}
