use http_body_util::BodyExt;
use hyper::{
    Response as HyperResponse,
    body::{Body, Incoming},
    http::Request,
};
use hyper_util::rt::TokioIo;
use simd_json::Buffers;
use std::error::Error as StdError;
use tokio::io::AsyncReadExt;

use interprocess::local_socket::tokio::{Stream, prelude::*};

pub mod shortcuts;
mod wrapper;
use wrapper::BodyDataStreamExt;

use crate::api::R;

#[derive(Debug, thiserror::Error)]
pub enum ClientError<'a> {
    #[error("An IO error occurred: {0}")]
    Io(#[from] std::io::Error),
    #[error("A network error occurred: {0}")]
    Hyper(#[from] hyper::Error),
    #[error("An error occurred while perform HTTP: {0}")]
    Http(#[from] hyper::http::Error),
    #[error("An error occurred: {0}")]
    ParseFailed(#[from] simd_json::Error),
    #[error("An server error respond: {0:?}")]
    ServerResponseFailed(R<'a, Option<()>>),
    #[error("IPC request `{operation}` succeeded but carried no data")]
    EmptyData { operation: &'static str },
    #[error("An error occurred: {0}")]
    Other(#[from] anyhow::Error),
}

pub struct Response {
    response: HyperResponse<Incoming>,
}

pub async fn send_request<R>(
    placeholder: &str,
    request: Request<R>,
) -> Result<Response, ClientError<'_>>
where
    R: Body + 'static + Send,
    R::Data: Send,
    R::Error: Into<Box<dyn StdError + Send + Sync>>,
{
    let name = crate::utils::get_name(placeholder)?;
    let conn = Stream::connect(name).await?;
    let io = TokioIo::new(conn);
    let (mut sender, conn) =
        hyper::client::conn::http1::handshake::<TokioIo<Stream>, R>(io).await?;
    tokio::task::spawn(async move {
        if let Err(err) = conn.with_upgrades().await {
            tracing::error!("An error occurred: {:#?}", err);
        }
    });

    let response = sender.send_request(request).await?;

    if response.status().is_client_error() || response.status().is_server_error() {
        let status = response.status();
        let res = Response { response };
        return match res.cast_body::<crate::api::R<Option<()>>>().await {
            Ok(envelope) => Err(ClientError::ServerResponseFailed(envelope)),
            Err(error) => Err(ClientError::Other(anyhow::anyhow!(
                "Received HTTP {status}, but failed to decode its error envelope: {error}"
            ))),
        };
    }
    Ok(Response { response })
}

pub(crate) async fn open_websocket<'a>(
    placeholder: &'a str,
    request: Request<axum::body::Body>,
) -> Result<
    tokio_tungstenite::WebSocketStream<TokioIo<hyper::upgrade::Upgraded>>,
    ClientError<'a>,
> {
    let mut response = send_request(placeholder, request).await?.response;
    if response.status() != hyper::StatusCode::SWITCHING_PROTOCOLS {
        return Err(ClientError::Other(anyhow::anyhow!(
            "WebSocket upgrade returned HTTP {}",
            response.status()
        )));
    }
    let upgraded = hyper::upgrade::on(&mut response).await?;
    Ok(tokio_tungstenite::WebSocketStream::from_raw_socket(
        TokioIo::new(upgraded),
        tokio_tungstenite::tungstenite::protocol::Role::Client,
        None,
    )
    .await)
}

impl Response {
    pub fn get_ref(&self) -> &HyperResponse<Incoming> {
        &self.response
    }
    /// use simd_json to cast the body of the response to a specific type
    pub async fn cast_body<'a, T>(self) -> Result<T, ClientError<'a>>
    where
        T: for<'de> serde::Deserialize<'de>,
    {
        let content_length = self.response.headers().get(hyper::header::CONTENT_LENGTH);
        let content_length = content_length
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(0);
        if content_length == 0 {
            return Err(ClientError::Other(anyhow::anyhow!(
                "No content in response"
            )));
        }
        let mut buf = Vec::with_capacity(content_length);
        let stream = self.response.into_data_stream().into_stream_wrapper();
        let mut reader = tokio_util::io::StreamReader::new(stream);
        let n = reader.read_to_end(&mut buf).await?;
        if n != content_length {
            return Err(ClientError::Other(anyhow::anyhow!(
                "Failed to read the entire response"
            )));
        }
        let mut buffers = Buffers::default();
        Ok(simd_json::serde::from_slice_with_buffers(
            &mut buf,
            &mut buffers,
        )?)
    }
}
