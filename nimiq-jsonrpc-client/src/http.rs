use std::fmt::Debug;

use serde::{
    ser::Serialize,
    de::Deserialize,
};
use serde_json::Value;
use async_trait::async_trait;
use thiserror::Error;
use futures::stream::BoxStream;
use url::Url;

use nimiq_jsonrpc_core::{Request, Response, SubscriptionId, Credentials};

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
    url: Url,
    basic_auth: Option<Credentials>,
}

impl HttpClient {
    /// Creates a new HTTP client.
    ///
    /// # Arguments
    ///
    ///  - `client`: The [`reqwest::Client`] to use for the HTTP requests.
    ///  - `url`: The URL to which the requests are send.
    ///  - `basic_auth`: Credentials used for HTTP Basic Authentication (optional).
    ///
    pub fn new(client: reqwest::Client, url: Url, basic_auth: Option<Credentials>) -> Self {
        Self {
            next_id: 1,
            client,
            url,
            basic_auth,
        }
    }

    /// Creates a new HTTP client - with default client and without authentication
    ///
    /// # Arguments
    ///
    ///  - `url`: The URL to which the requests are send.
    ///
    pub fn with_url(url: Url) -> Self {
        Self::new(reqwest::Client::new(), url, None)
    }
}

#[async_trait]
impl Client for HttpClient {
    type Error = Error;

    async fn send_request<P, R>(&mut self, method: &str, params: &P) -> Result<R, Error>
        where P: Serialize + Debug + Send + Sync,
              R: for<'de> Deserialize<'de> + Debug + Send + Sync,
    {
        let request_id = self.next_id;
        self.next_id += 1;

        let request = Request::build(method.to_owned(), Some(params), Some(&request_id))
            .expect("Failed to serialize JSON-RPC request.");

        log::debug!("Sending request: {:?}", request);

        let mut request_builder = self.client.post(self.url.clone());

        if let Some(basic_auth) = &self.basic_auth {
            request_builder = request_builder.basic_auth(&basic_auth.username, Some(&basic_auth.password));
        }

        let response: Response = request_builder
            .json(&request)
            .send()
            .await?
            .error_for_status()?
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

    async fn connect_stream<T>(&mut self, _id: SubscriptionId) -> BoxStream<'static, T> {
        panic!("Streams are not supported by the HTTP client.");
    }
}
