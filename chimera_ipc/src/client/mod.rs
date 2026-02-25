use http_body_util::BodyExt;
use hyper::{
    Response as HyperResponse,
    body::{Body, Incoming},
    http::Request,
};
use hyper_util::rt::TokioIo;
use std::error::Error as StdError;
use tokio::io::AsyncReadExt;

use interprocess::local_socket::tokio::{Stream, prelude::*};

pub mod shortcuts;

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("An IO error occurred: {0}")]
    Io(#[from] std::io::Error),
    #[error("A network error occurred: {0}")]
    Hyper(#[from] hyper::Error),
    #[error("An error occurred while perform HTTP: {0}")]
    Http(#[from] hyper::http::Error),
    #[error("An error occurred: {0}")]
    ParseFailed(#[from] serde_json::Error),
    #[error("An error occurred: {0}")]
    Other(#[from] anyhow::Error),
}

pub struct Response {
    response: HyperResponse<Incoming>,
}
