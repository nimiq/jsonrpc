//! This crate implements multiple JSON-RPC clients. Currently available are: `http` and `websocket`. Only `websocket`
//! supports *PubSub*.
//!
//! Instead of using a [`Client`] implementation and calling [`Client::send_request`] directly, you can derive a proxy
//! struct that implements methods for egonomic RPC calling:
//!
//! ```rust
//! #[nimiq_jsonrpc_derive::proxy]
//! #[async_trait]
//! trait Foobar {
//!     async fn hello(&mut self, name: String) -> String;
//!     async fn bye(&mut self) -> Goodbye;
//!     //async fn may_fail(&self) -> Result<String, ()>;
//! }
//!```
//!
//! # TODO
//!
//!  - Implement PubSub
//!  - Proper error handling
//!

#![warn(missing_docs)]
#![warn(missing_doc_code_examples)]

/// An implementation of JSON-RPC over HTTP post requests. Feature `http` must be enabled:
///
/// ```toml
/// [dependencies]
/// nimiq-jsonrpc-client = { version = "...", features = ["http"] }
/// ```
///
#[cfg(feature = "http-client")]
pub mod http;

/// An implementation of JSON-RPC over websockets. Feature `websocket` must be enabled:
///
/// ```toml
/// [dependencies]
/// nimiq-jsonrpc-client = { version = "...", features = ["websocket"] }
/// ```
///
#[cfg(feature = "websocket-client")]
pub mod websocket;

#[cfg(feature = "wasm-websocket-client")]
pub mod wasm_websocket;

use std::{fmt::Debug, sync::Arc};

use async_trait::async_trait;
use futures::{lock::Mutex, stream::BoxStream};
use serde::{de::Deserialize, ser::Serialize};

use nimiq_jsonrpc_core::SubscriptionId;

#[async_trait]
/// This trait must be implemented by the client's transport. It is responsible to send the request and return the
/// server's response.
///
/// # TODO
///
///  - Support sending notifications (i.e. don't set the request ID and always a `Result<(), Self::Error>`.
///
pub trait Client {
    /// Error type that this client returns.
    type Error: Debug;

    /// Sends a JSON-HTTP request
    ///
    /// # Arguments
    ///
    ///  - `method`: The name of the method to call.
    ///  - `params`: The request parameters. This can be anything that implements [`serde::ser::Serialize`], but
    ///              should serialize to a struct containing the named method arguments.
    ///
    /// # Returns
    ///
    /// Returns either the result that was responded with by the server, or an error. The error can be either a
    /// client-side error (e.g. a network error), or an error object sent by the server.
    ///
    async fn send_request<P, R>(&mut self, method: &str, params: &P) -> Result<R, Self::Error>
    where
        P: Serialize + Debug + Send + Sync,
        R: for<'de> Deserialize<'de> + Debug + Send + Sync;

    /// If the client supports streams (i.e. receiving notifications), this should return a stream for the specific
    /// subscription ID.
    ///
    /// # Arguments
    ///
    ///  - `id`: The subscription ID
    ///
    /// # Returns
    ///
    /// Returns a stream of items of type `T` that are received as notifications with the specific subscription ID.
    ///
    /// # Panics
    ///
    /// If the client doesn't support receiving notifications, this method is allowed to panic.
    ///
    async fn connect_stream<T: Unpin + 'static>(
        &mut self,
        id: SubscriptionId,
    ) -> BoxStream<'static, T>
    where
        T: for<'de> Deserialize<'de> + Debug + Send + Sync;
}

/// Wraps a client into an `Arc<Mutex<_>>`, so that it can be cloned.
pub struct ArcClient<C> {
    inner: Arc<Mutex<C>>,
}

#[async_trait]
impl<C: Client + Send> Client for ArcClient<C> {
    type Error = <C as Client>::Error;

    async fn send_request<P, R>(&mut self, method: &str, params: &P) -> Result<R, Self::Error>
    where
        P: Serialize + Debug + Send + Sync,
        R: for<'de> Deserialize<'de> + Debug + Send + Sync,
    {
        self.inner.lock().await.send_request(method, params).await
    }

    async fn connect_stream<T: Unpin + 'static>(
        &mut self,
        id: SubscriptionId,
    ) -> BoxStream<'static, T>
    where
        T: for<'de> Deserialize<'de> + Debug + Send + Sync,
    {
        self.inner.lock().await.connect_stream(id).await
    }
}

impl<C: Client> ArcClient<C> {
    /// Creates a new `ArcClient` from the inner client.
    pub fn new(inner: C) -> Self {
        Self {
            inner: Arc::new(Mutex::new(inner)),
        }
    }
}

impl<C> Clone for ArcClient<C> {
    fn clone(&self) -> Self {
        ArcClient {
            inner: Arc::clone(&self.inner),
        }
    }
}
