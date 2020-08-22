use std::ops::RangeInclusive;

use serde::{Serialize, Deserialize};
use serde_json::Value;
use thiserror::Error;


pub const JSONRPC_VERSION: &'static str = "2.0";
pub const JSONRPC_RESERVED_ERROR_CODES: RangeInclusive<i64> = -32768 ..= -32000;


#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub enum SingleOrBatch<T> {
    Single(T),
    Batch(Vec<T>)
}


#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub jsonrpc: String,

    pub method: String,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<Value>,
}

impl Request {
    pub fn new(method: String, params: Option<Value>) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION.to_owned(),
            method,
            params,
            id: None // This will be set by the client
        }
    }

    pub fn verify(&self) -> Result<(), Error> {
        // TODO: verify correctness of request
        Ok(())
    }
}


#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Response {
    pub jsonrpc: String,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<Error>,

    pub id: Value,
}

impl Response {
    pub fn new_success(id: Value, result: Value) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION.to_owned(),
            result: Some(result),
            error: None,
            id
        }
    }

    pub fn new_error(id: Value, error: Error) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION.to_owned(),
            result: None,
            error: Some(error),
            id,
        }
    }
}


pub type ErrorCode = i64;


#[derive(Clone, Debug, Error, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Error {
    pub code: i64,

    pub message: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> Result<(), std::fmt::Error> {
        write!(f, "JSON-RPC error: code={}", self.code)?;
        if let Some(message) = &self.message {
            write!(f, ": {}", message)?;
        }
        Ok(())
    }
}

impl Error {
    fn new_reserved(code: i64, message: &'static str) -> Self {
        Self {
            code,
            message: Some(message.to_owned()),
            data: None,
        }
    }

    pub fn parse_error() -> Self {
        Self::new_reserved(-32700, "Parse error")
    }

    pub fn invalid_request() -> Self {
        Self::new_reserved(-32600, "Invalid Request")
    }

    pub fn method_not_found() -> Self {
        Self::new_reserved(-32601, "Method not found")
    }

    pub fn invalid_params() -> Self {
        Self::new_reserved(-32602, "Invalid params")
    }

    pub fn internal_error() -> Self {
        Self::new_reserved(-32603, "Internal error")
    }
}

impl From<()> for Error {
    fn from(_: ()) -> Self {
        Error::internal_error()
    }
}
