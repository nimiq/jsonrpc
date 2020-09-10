use std::ops::RangeInclusive;

use serde::{Serialize, Deserialize};
use serde_json::Value;
use thiserror::Error;


pub const JSONRPC_VERSION: &'static str = "2.0";
pub const JSONRPC_RESERVED_ERROR_CODES: RangeInclusive<i64> = -32768 ..= -32000;


/// An error of this JSON-RPC implementation. This can be either an error object returned by the server, or
/// any other error that might be triggered in the server or client (e.g. a network error).
#[derive(Debug, Error)]
pub enum Error {
    /// An error object sent by the server.
    #[error("The server responded with an error: {0}")]
    JsonRpc(#[from] RpcError),

    /// JSON parsing error.
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("Received invalid response")]
    InvalidResponse,
}

impl Error {
    /// Tries to convert this error to an error after the JSON-RPC specification.
    pub fn into_rpc_error(self) -> Option<RpcError> {
        match self {
            Error::JsonRpc(e) => Some(e),
            Error::Json(e) => Some(RpcError::parse_error(Some(e.to_string()))),
            _ => None,
        }
    }
}


/// A JSON-RPC request or response can either be a single request or response, or a list of the former. This `enum`
/// matches either for serialization and deserialization.
///
/// [1] https://www.jsonrpc.org/specification#batch
///
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub enum SingleOrBatch<T> {
    Single(T),
    Batch(Vec<T>)
}


/// A single JSON-RPC request object
///
/// [1] https://www.jsonrpc.org/specification#request_object
///
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    /// The version of the protocol. Must be `"2.0"`. See [[`JSONRPC_VERSION`]].
    pub jsonrpc: String,

    /// Name of the method to be called.
    pub method: String,

    /// The parameters for the function call.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,

    /// Identifier sent by client to match to responses. If send by the client, the server will include this, in their
    /// responses.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<Value>,
}

impl Request {
    /// Creates a new request object.
    ///
    pub fn new(method: String, params: Option<Value>, id: Option<Value>) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION.to_owned(),
            method,
            params,
            id,
        }
    }

    pub fn build<P, I>(method: String, params_opt: Option<&P>, id_opt: Option<&I>) -> Result<Self, Error>
        where
            P: Serialize,
            I: Serialize,
    {
        let params = params_opt
            .map(|params| serde_json::to_value(params))
            .transpose()?;
        let id = id_opt
            .map(|id| serde_json::to_value(id))
            .transpose()?;

        Ok(Self::new(method, params, id))
    }

    /// Verifies the correctness of the request and either returns nothing if the request was correct, or with a
    /// `RpcError` object with the appropriate error to reply with.
    pub fn verify(&self) -> Result<(), RpcError> {
        if self.jsonrpc != JSONRPC_VERSION {
            return Err(RpcError::invalid_request(Some(format!("Field 'jsonrpc' must be '2.0', but was '{}'", self.jsonrpc))))
        }

        match &self.id {
            Some(Value::String(_)) => {}, // Ok
            Some(Value::Number(n)) => {
                if n.is_f64() {
                    return Err(RpcError::invalid_request(Some(format!("Field 'id' is a number, but should not be fractional: {:?}", self.id))))
                }
                // otherwise numbers are ok.
            },
            _ => {
                return Err(RpcError::invalid_request(Some(format!("Invalid type in field 'id': {:?}", self.id))))
            },
        }

        Ok(())
    }
}

/// A single JSON-RPC response object.
///
/// [1] https://www.jsonrpc.org/specification#response_object
///
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Response {
    /// The version of the protocol. Must be `"2.0"`. See [[`JSONRPC_VERSION`]].
    pub jsonrpc: String,

    /// The result of the function call that is returned to the client. This is `None`, if there was an error. This
    /// might also be `None`, if the call at the server returned `null`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,

    /// The error triggered by a request, if present.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<RpcError>,

    /// The ID to identify the to which request this response belongs. If the client sent an ID in the request, this
    /// will be replied here. This is `null`, if the response is an error that was triggered when trying to parse the
    /// request ID
    pub id: Value,
}

impl Response {
    /// Creates a new successful response.
    pub fn new_success(id: Value, result: Value) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION.to_owned(),
            result: Some(result),
            error: None,
            id,
        }
    }

    /// Creates a new error response.
    pub fn new_error(id: Value, error: RpcError) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION.to_owned(),
            result: None,
            error: Some(error),
            id,
        }
    }

    /// Creates a response object from a [[std::result::Result]].
    pub fn from_result<R>(id: Value, result: Result<R, RpcError>) -> Result<Self, Error>
        where
            R: Serialize,
    {
        Ok(match result {
            Ok(result) => Self::new_success(id, serde_json::to_value(&result)?),
            Err(e) => Self::new_error(id, e),
        })
    }

    /// Converts the response object to a [[std::result::Result]]
    pub fn into_result<R>(self) -> Result<R, Error>
        where
            R: for<'de> Deserialize<'de>,
    {
        match (self.result, self.error) {
            (Some(result), None) => {
                Ok(serde_json::from_value(result)?)
            },
            (None, Some(error)) => {
                Err(error.into())
            }
            _ => {
                Err(Error::InvalidResponse)
            }
        }
    }
}


/// Numeric error code used in error objects.
pub type ErrorCode = i64;


/// An error object that can be returned by the server.
///
/// [1] https://www.jsonrpc.org/specification#error_object
///
#[derive(Clone, Debug, Error, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RpcError {
    /// The error code as defined by the JSON-RPC specification.
    pub code: i64,

    /// An optional error message.
    pub message: Option<String>,

    /// Optional data attached to the error object by the server.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl std::fmt::Display for RpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> Result<(), std::fmt::Error> {
        write!(f, "JSON-RPC error: code={}", self.code)?;
        if let Some(message) = &self.message {
            write!(f, ": {}", message)?;
        }
        Ok(())
    }
}

impl RpcError {
    fn new_reserved(code: i64, message: &'static str, description: Option<String>) -> Self {
        Self {
            code,
            message: Some(message.to_owned()),
            data: description.map(|s| Value::String(s)),
        }
    }

    pub fn parse_error(description: Option<String>) -> Self {
        Self::new_reserved(-32700, "Parse error", description)
    }

    pub fn invalid_request(description: Option<String>) -> Self {
        Self::new_reserved(-32600, "Invalid Request", description)
    }

    pub fn method_not_found(description: Option<String>) -> Self {
        Self::new_reserved(-32601, "Method not found", description)
    }

    pub fn invalid_params(description: Option<String>) -> Self {
        Self::new_reserved(-32602, "Invalid params", description)
    }

    pub fn internal_error(description: Option<String>) -> Self {
        Self::new_reserved(-32603, "Internal error", description)
    }
}

impl Default for RpcError {
    fn default() -> Self {
        Self::internal_error(None)
    }
}

impl From<()> for RpcError {
    fn from(_: ()) -> Self {
        Self::default()
    }
}
