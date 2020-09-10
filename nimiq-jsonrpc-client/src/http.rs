use serde::{
    ser::Serialize,
    de::Deserialize,
};
use serde_json::Value;
use async_trait::async_trait;
use thiserror::Error;

use nimiq_jsonrpc_core::{Request, Response};

use crate::Client;


/// Error that might be returned by the http client.
#[derive(Debug, Error)]
pub enum Error {
    /// The HTTP request failed.
    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),

    /// The server replied with an error object.
    #[error("{0}")]
    JsonRpc(#[from] nimiq_jsonrpc_core::Error),

    /// Request and response ID mismatched.
    #[error("Response ID doesn't match request ID: expected {expected}, but got {got:?}")]
    IdMismatch {
        /// The expected ID that was expected.
        expected: usize,

        /// The ID that the server replied with.
        got: Value,
    }
}


/// A JSON-HTTP client that sends the request via HTTP POST to an URL.
pub struct HttpClient {
    next_id: usize,
    client: reqwest::Client,
    url: String,
}

impl HttpClient {
    /// Creates a new HTTP client.
    ///
    /// # Arguments
    ///
    ///  - `url`: The URL to which the requests are send.
    ///
    pub fn new(url: &str) -> Self {
        Self {
            next_id: 1,
            client: reqwest::Client::new(),
            url: url.to_owned(),
        }
    }
}

#[async_trait]
impl Client for HttpClient {
    type Error = Error;

    async fn send_request<P, R>(&mut self, method: &str, params: &P) -> Result<R, Error>
        where P: Serialize + std::fmt::Debug + Send + Sync,
              R: for<'de> Deserialize<'de> + std::fmt::Debug + Send + Sync,
    {
        let request_id = self.next_id;
        self.next_id += 1;

        let request = Request::build(method.to_owned(), Some(params), Some(&request_id))
            .expect("Failed to serialize JSON-RPC request.");

        log::debug!("Sending request: {:?}", request);

        let response: Response = self.client.post(&self.url)
            .json(&request)
            .send()
            .await?
            .json()
            .await?;

        log::debug!("Received response: {:?}", response);

        if response.id != Value::Number(request_id.into()) {
            Err(Error::IdMismatch { expected: request_id, got: response.id })
        }
        else {
            Ok(response.into_result()?)
        }
    }
}
