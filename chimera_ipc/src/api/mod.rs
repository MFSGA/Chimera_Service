pub mod status;

use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::{borrow::Cow, fmt::Debug, io::Error as IoError};

#[derive(Debug, Serialize, Deserialize, Clone, Copy, Default, PartialEq)]
pub enum ResponseCode {
    #[default]
    Ok = 0,
    OtherError = -1,
}

impl ResponseCode {
    pub const fn msg(&self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::OtherError => "other error",
        }
    }
}
