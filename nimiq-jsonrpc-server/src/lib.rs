//! This crate implements a JSON-RPC HTTP server using [warp](https://crates.io/crates/warp). It accepts POST requests
//! at `/` and requests over websocket at `/ws`.

#![warn(missing_docs)]
#![warn(missing_doc_code_examples)]


use std::{
    net::SocketAddr,
    sync::{atomic::AtomicUsize, Arc},
    future::Future,
    fmt::Debug,
};

use futures::{stream::{StreamExt, FuturesUnordered}, sink::SinkExt, Stream, pin_mut};
use tokio::sync::{RwLock, mpsc};
use async_trait::async_trait;
use serde_json::Value;
use warp::Filter;
use bytes::Bytes;
use serde::{
    ser::Serialize,
    de::Deserialize,
};
use thiserror::Error;

use nimiq_jsonrpc_core::{SingleOrBatch, Request, Response, RpcError, SubscriptionId, SubscriptionMessage};
use std::sync::atomic::Ordering;


/// A server error.
#[derive(Debug, Error)]
pub enum Error {
    /// Error returned by warp
    #[error("HTTP error: {0}")]
    Warp(#[from] warp::Error),

    /// Error from the message queues, that are used internally.
    #[error("Queue error: {0}")]
    Mpsc(#[from] tokio::sync::mpsc::error::SendError<Vec<u8>>),

    /// JSON error
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    /// JSON RPC error (from [`nimiq_jsonrpc_core`])
    #[error("JSON RPC error: {0}")]
    JsonRpc(#[from] nimiq_jsonrpc_core::Error),
}


/// The server configuration
///
/// #TODO
///
/// - CORS header
/// - allowed methods
///
#[derive(Clone, Debug)]
pub struct Config {
    /// Bind server to specified hostname and port.
    pub bind_to: SocketAddr,

    /// Enable JSON-RPC over websocket at `/ws`.
    pub enable_websocket: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            bind_to: ([127, 0, 0, 1], 8000).into(),
            enable_websocket: true,
        }
    }
}


struct Inner<D: Dispatcher> {
    config: Config,
    dispatcher: RwLock<D>,
    next_id: AtomicUsize,
}


/// A JSON-RPC server.
pub struct Server<D: Dispatcher> {
    inner: Arc<Inner<D>>,
}


impl<D: Dispatcher> Server<D> {
    /// Creates a new JSON-RPC server.
    ///
    /// # Arguments
    ///
    ///  - `config`: The server configuration.
    ///  - `dispatcher`: The dispatcher that takes a request and executes the requested method. This can be derived
    ///    using the `nimiq_jsonrpc_derive::service` macro.
    pub fn new(config: Config, dispatcher: D) -> Self {
        Self {
            inner: Arc::new(Inner {
                config,
                dispatcher: RwLock::new(dispatcher),
                next_id: AtomicUsize::new(1),
            })
        }
    }

    /// Runs the server forever.
    pub async fn run(&self) {
        // Route to use JSON-RPC over websocket
        let inner = Arc::clone(&self.inner);
        let ws_route = warp::path("ws")
            .and(warp::path::end())
            .and(warp::ws())
            .map(move |ws| Self::upgrade_to_ws(Arc::clone(&inner), ws));

        // Route for backwards-compatiblity to use JSON-RPC over HTTP at /
        let inner = Arc::clone(&self.inner);
        let post_route = warp::path::end()
            .and(warp::post())
            .and(warp::body::bytes())
            .and_then(move |body: Bytes| {
                let inner = Arc::clone(&inner);
                async move {
                    let data = Self::handle_raw_request(inner, &body, None).await
                        .unwrap_or_default();

                    let response = http::response::Builder::new()
                        .status(200)
                        .header("Content-Type", "application/json-rpc")
                        .body(data)
                        .unwrap(); // As long as the hard-coded status code and content-type is correct, this won't fail.

                    Ok::<_, warp::Rejection>(response)
                }
            });

        let routes = ws_route.or(post_route);

        warp::serve(routes).run(self.inner.config.bind_to.clone()).await;
    }

    /// Upgrades a connection to websocket. This creates message queues and tasks to forward messages between them.
    ///
    /// We need a MPSC queue to be able to pass sender halves to called functions. The called functions then can keep
    /// the sender for sending notifications to the client.
    ///
    /// # TODO:
    ///
    ///  - This sends stuff as binary websocket frames. It should really use text frames.
    ///  - Make the queue size configurable
    ///
    fn upgrade_to_ws(inner: Arc<Inner<D>>, ws: warp::ws::Ws) -> impl warp::Reply {
        ws.on_upgrade(move |websocket| {
            let (mut tx, mut rx) = websocket.split();

            let (mut multiplex_tx, mut multiplex_rx) = mpsc::channel(16); // TODO: What size?

            // Forwards multiplexer queue output to websocket
            let forward_fut = async move {
                while let Some(request) = multiplex_rx.recv().await {
                    if let Ok(message) = serde_json::to_vec(&request) {
                        tx.send(warp::ws::Message::binary(message)).await?;
                    }
                }
                Ok::<(), Error>(())
            };

            // Handles requests received from websocket
            let handle_fut = {
                async move {
                    while let Some(message) = rx.next().await.transpose()? {
                        if let Some(response) = Self::handle_raw_request(Arc::clone(&inner), message.as_bytes(), Some(&multiplex_tx)).await {
                            multiplex_tx.send(response).await?;
                        }
                    }
                    Ok::<(), Error>(())
                }
            };

            async {
                if let Err(e) = futures::future::try_join(forward_fut, handle_fut).await {
                    log::error!("Websocket error: {}", e);
                }
            }
        })
    }

    /// Handles a raw request received as POST request, or websocket message.
    ///
    /// # Arguments
    ///
    ///  - `inner`: Server state
    ///  - `request`: The raw request data.
    ///  - `tx`: If the request was received over websocket, this the message queue over which the called function can
    ///          send notifications to the client (used for subscriptions).
    ///
    async fn handle_raw_request(inner: Arc<Inner<D>>, request: &[u8], tx: Option<&mpsc::Sender<Vec<u8>>>) -> Option<Vec<u8>> {
        match serde_json::from_slice(request) {
            Ok(request) => Self::handle_request(inner, request, tx).await,
            Err(_e) => {
                log::error!("Received invalid JSON from client");
                Some(SingleOrBatch::Single(Response::new_error(Value::Null, RpcError::invalid_request(Some("Received invalid JSON".to_owned())))))
            }
        }.map(|response| {
            serde_json::to_vec(&response)
                .expect("Failed to serialize JSON RPC response")
        })
    }

    /// Handles an JSON RPC request. This can either be a single or batch request.
    ///
    /// # Arguments
    ///
    ///  - `inner`: Server state
    ///  - `request`: The request that was received.
    ///  - `tx`: If the request was received over websocket, this the message queue over which the called function can
    ///          send notifications to the client (used for subscriptions).
    ///
    async fn handle_request(inner: Arc<Inner<D>>, request: SingleOrBatch<Request>, tx: Option<&mpsc::Sender<Vec<u8>>>) -> Option<SingleOrBatch<Response>> {
        match request {
            SingleOrBatch::Single(request) => {
                Self::handle_single_request(inner, request, tx).await
                    .map(|response| SingleOrBatch::Single(response))
            },

            SingleOrBatch::Batch(requests) => {
                let futures = requests
                    .into_iter()
                    .map(|request| Self::handle_single_request(Arc::clone(&inner), request, tx.clone()))
                    .collect::<FuturesUnordered<_>>();

                let responses = futures.filter_map(|response_opt| async { response_opt })
                    .collect::<Vec<Response>>().await;

                Some(SingleOrBatch::Batch(responses))
            }
        }
    }

    /// Handles a single JSON RPC request
    ///
    /// # TODO
    ///
    /// - Handle subscriptions
    async fn handle_single_request(inner: Arc<Inner<D>>, request: Request, tx: Option<&mpsc::Sender<Vec<u8>>>) -> Option<Response> {
        let mut dispatcher = inner.dispatcher.write().await;
        // This ID is only used for streams
        let id = inner.next_id.fetch_add(1, Ordering::SeqCst);
        dispatcher.dispatch(request, tx, id).await
    }
}

/// A method dispatcher. These take a request and handle the method execution. Can be generated from an `impl` block
/// using `nimiq_jsonrpc_derive::service`.
#[async_trait]
pub trait Dispatcher: Send + Sync + 'static {
    /// Calls the requested method with the request parameters and returns it's return value (or error) as a resposne.
    async fn dispatch(&mut self, request: Request, tx: Option<&mpsc::Sender<Vec<u8>>>, id: usize) -> Option<Response>;
}


/// Read the request and call a handler function if possible. This variant accepts calls with arguments.
///
/// This is a helper function used by implementations of `Dispatcher`.
///
/// # TODO
///
///  - Currently this always expects an object with named parameters. Do we want to accept a list too?
///  - Merge with it's other variant, as a function call without arguments is just one with `()` as request parameter.
///
pub async fn dispatch_method_with_args<P, R, E, F, Fut>(request: Request, f: F) -> Option<Response>
    where P: for<'de> Deserialize<'de> + Send,
          R: Serialize,
          RpcError: From<E>,
          F: FnOnce(P) -> Fut + Send,
          Fut: Future<Output=Result<R, E>> + Send
{
    let params = match request.params {
        Some(params) => params,
        None => return error_response(request.id, || RpcError::invalid_params(Some("Missing request parameters.".to_owned()))),
    };

    let params = match serde_json::from_value(params) {
        Ok(params) => params,
        Err(_e) => return error_response(request.id, || RpcError::invalid_params(Some("Expected an object for the request parameters.".to_owned()))),
    };

    let result = f(params).await;

    response(request.id, result)
}

/// Read the request and call a handler function if possible. This variant accepts calls without arguments.
///
/// This is a helper function used by implementations of `Dispatcher`.
///
pub async fn dispatch_method_without_args<R, E, F, Fut>(request: Request, f: F) -> Option<Response>
    where R: Serialize,
          RpcError: From<E>,
          F: FnOnce() -> Fut + Send,
          Fut: Future<Output=Result<R, E>> + Send
{
    let result = f().await;

    match request.params {
        Some(Value::Null) | None => {},
        Some(Value::Array(a)) if a.is_empty() => {},
        Some(Value::Object(o)) if o.is_empty() => {},
        _ => return error_response(request.id, || RpcError::invalid_params(Some("Didn't expect any request parameters".to_owned()))),
    }

    response(request.id, result)
}

/// Constructs a [`Response`] if necessary (i.e., if the request ID was set).
fn response<R, E>(id_opt: Option<Value>, result: Result<R, E>) -> Option<Response>
    where R: Serialize,
          RpcError: From<E>,
{
    let response = match (id_opt, result) {
        (Some(id), Ok(retval)) => {
            let retval = serde_json::to_value(retval).expect("Failed to serialize return value");
            Some(Response::new_success(id, retval))
        },
        (Some(id), Err(e)) => {
            Some(Response::new_error(id, RpcError::from(e)))
        },
        (None, _) => None,
    };

    log::debug!("Sending response: {:?}", response);

    response
}

/// Constructs an error response if necessary (i.e., if the request ID was set).
///
/// # Arguments
///
///  - `id_opt`: The ID field from the request.
///  - `e`: A function that returns the error. This is only called, if we actually can respond with an error.
///
pub fn error_response<E>(id_opt: Option<Value>, e: E) -> Option<Response>
    where
        E: FnOnce() -> RpcError,
{
    if let Some(id) = id_opt {
        Some(Response::new_error(id, e()))
    }
    else {
        None
    }
}


async fn forward_notification<T>(item: T, tx: &mut mpsc::Sender<Vec<u8>>, id: &SubscriptionId, method: &str) -> Result<(), Error>
    where
        T: Serialize + Debug + Send + Sync,
{
    let message = SubscriptionMessage {
        subscription: id.clone(),
        result: item
    };

    let notification = Request::build::<_, ()>(method.to_owned(), Some(&message), None)?;

    log::debug!("Sending notification: {:?}", notification);

    tx.send(serde_json::to_vec(&notification)?).await?;

    Ok(())
}

/// Connects a stream such that its items are sent to the client as notifications.
///
/// # Arguments
///
///  - `stream`: The stream that should be forwarded to the client
///  - `tx`: The tx queue from the client connection.
///  - `stream_id`: An unique ID that can be assigned to the stream.
///  - `method`: The method name set in the notifications.
///
/// # Returns
///
/// Returns the subscription ID.
///
pub fn connect_stream<T, S>(stream: S, tx: &mpsc::Sender<Vec<u8>>, stream_id: usize, method: String) -> SubscriptionId
    where
        T: Serialize + Debug + Send + Sync,
        S: Stream<Item=T> + Send + 'static,
{
    let mut tx = tx.clone();
    let id = Value::Number(stream_id.into());

    {
        let id = id.clone();
        tokio::spawn(async move {
            pin_mut!(stream);

            while let Some(item) = stream.next().await {
                if let Err(e) = forward_notification(item, &mut tx, &id, &method).await {
                    log::error!("{}", e);
                }
            }
        });
    }

    id
}
