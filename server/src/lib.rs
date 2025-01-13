//! This crate implements a JSON-RPC HTTP server using [warp](https://crates.io/crates/warp). It accepts POST requests
//! at `/` and requests over websocket at `/ws`.

#![warn(missing_docs)]
#![warn(rustdoc::missing_doc_code_examples)]

mod auth_filter;

use std::{
    collections::HashSet,
    error,
    fmt::{self, Debug},
    future::{self, Future},
    net::{IpAddr, SocketAddr},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
};

use async_trait::async_trait;
use blake2::{digest::consts::U32, Blake2b, Digest};
use bytes::Bytes;
use futures::{
    pin_mut,
    sink::SinkExt,
    stream::{FuturesUnordered, StreamExt},
    Stream,
};
use headers::{authorization::Basic, Authorization};
use serde::{de::Deserialize, ser::Serialize};
use serde_json::Value;
use subtle::ConstantTimeEq;
use thiserror::Error;
use tokio::sync::{mpsc, RwLock, RwLockReadGuard, RwLockWriteGuard};
pub use warp::filters::ws::Message;
use warp::Filter;

use nimiq_jsonrpc_core::{
    Request, Response, RpcError, Sensitive, SingleOrBatch, SubscriptionId, SubscriptionMessage,
};

/// A server error.
#[derive(Debug, Error)]
pub enum Error {
    /// Error returned by warp
    #[error("HTTP error: {0}")]
    Warp(#[from] warp::Error),

    /// Error from the message queues, that are used internally.
    #[error("Queue error: {0}")]
    Mpsc(#[from] tokio::sync::mpsc::error::SendError<Message>),

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

    /// Allowed IPs. If `None`, all source IPs are allowed.
    pub ip_whitelist: Option<HashSet<IpAddr>>,

    /// Username and password for HTTP basic authentication.
    pub basic_auth: Option<Credentials>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            bind_to: ([127, 0, 0, 1], 8000).into(),
            enable_websocket: true,
            ip_whitelist: None,
            basic_auth: None,
        }
    }
}

fn blake2b(bytes: &[u8]) -> [u8; 32] {
    *Blake2b::<U32>::digest(bytes).as_ref()
}

/// Basic auth credentials, containing username and password.
#[derive(Clone, Debug)]
pub struct Credentials {
    username: String,
    password_blake2b: Sensitive<[u8; 32]>,
}

/// Invalid username or password was passed to [`Credentials::verify`].
#[derive(Clone, Debug)]
pub struct CredentialsVerificationError(());

impl error::Error for CredentialsVerificationError {}
impl fmt::Display for CredentialsVerificationError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        fmt::Display::fmt("invalid username or password", f)
    }
}

impl Credentials {
    /// Create basic auth credentials from username and password.
    pub fn new<T: Into<String>, U: AsRef<str>>(username: T, password: U) -> Credentials {
        Credentials::new_from_blake2b(username, blake2b(password.as_ref().as_bytes()))
    }
    /// Create basic auth credentials from username and Blake2b hash of the password.
    pub fn new_from_blake2b<T: Into<String>>(
        username: T,
        password_blake2b: [u8; 32],
    ) -> Credentials {
        Credentials {
            username: username.into(),
            password_blake2b: Sensitive(password_blake2b),
        }
    }
    /// Verifies basic auth credentials against username and password in constant time.
    pub fn verify<T: AsRef<str>, U: AsRef<str>>(
        &self,
        username: T,
        password: U,
    ) -> Result<(), CredentialsVerificationError> {
        if (self.username.as_bytes().ct_eq(username.as_ref().as_bytes())
            & self
                .password_blake2b
                .ct_eq(&blake2b(password.as_ref().as_bytes())))
        .into()
        {
            Ok(())
        } else {
            Err(CredentialsVerificationError(()))
        }
    }
}

struct Inner<D: Dispatcher> {
    config: Config,
    dispatcher: RwLock<D>,
    next_id: AtomicU64,
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
                next_id: AtomicU64::new(1),
            }),
        }
    }

    /// Returns a borrow to the server config.
    pub async fn config(&self) -> &Config {
        &self.inner.config
    }

    /// Returns a borrow to the server's dispatcher.
    pub async fn dispatcher(&self) -> RwLockReadGuard<'_, D> {
        self.inner.dispatcher.read().await
    }

    /// Returns a mutable borrow to the server's dispatcher.
    pub async fn dispatcher_mut(&self) -> RwLockWriteGuard<'_, D> {
        self.inner.dispatcher.write().await
    }

    /// Runs the server forever.
    pub async fn run(&self) {
        // Route to use JSON-RPC over websocket
        let inner = Arc::clone(&self.inner);
        let ws_route = warp::path("ws")
            .and(warp::path::end())
            .and(warp::ws())
            .map(move |ws| Self::upgrade_to_ws(Arc::clone(&inner), ws));

        // Route for backwards-compatibility to use JSON-RPC over HTTP at /
        let inner = Arc::clone(&self.inner);
        let post_route = warp::path::end()
            .and(warp::post())
            .and(warp::body::content_length_limit(1024 * 1024))
            .and(warp::body::bytes())
            .and_then(move |body: Bytes| {
                let inner = Arc::clone(&inner);
                async move {
                    let data = Self::handle_raw_request(inner, &Message::text(body), None)
                        .await
                        .unwrap_or(Message::text(""));

                    let response = http::response::Builder::new()
                        .status(200)
                        .header("Content-Type", "application/json")
                        .body(data.as_bytes().to_owned())
                        .unwrap(); // As long as the hard-coded status code and content-type is correct, this won't fail.

                    Ok::<_, warp::Rejection>(response)
                }
            });

        let json_rpc_route = ws_route.or(post_route);

        let root = if self.inner.config.basic_auth.is_some() {
            let inner = Arc::clone(&self.inner);
            let realm = "JSON-RPC";
            auth_filter::basic_auth_filter(realm)
                .and_then(move |auth_header: Authorization<Basic>| {
                    let inner = Arc::clone(&inner);

                    let basic_auth = inner.config.basic_auth.as_ref().unwrap();
                    future::ready(
                        basic_auth
                            .verify(auth_header.0.username(), auth_header.0.password())
                            .map_err(|CredentialsVerificationError(())| {
                                warp::reject::custom(auth_filter::Unauthorized {
                                    realm: realm.to_string(),
                                })
                            }),
                    )
                })
                .untuple_one()
                .boxed()
        } else {
            warp::any().boxed()
        };

        warp::serve(
            root.and(json_rpc_route)
                .recover(auth_filter::handle_auth_rejection),
        )
        .run(self.inner.config.bind_to)
        .await;
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

            let (multiplex_tx, mut multiplex_rx) = mpsc::channel::<Message>(16); // TODO: What size?

            // Forwards multiplexer queue output to websocket
            let forward_fut = async move {
                while let Some(data) = multiplex_rx.recv().await {
                    // Close the sink if we get a close message (don't echo the message since this is not permitted)
                    if data.is_close() {
                        tx.close().await?;
                    } else {
                        tx.send(data).await?;
                    }
                }
                Ok::<(), Error>(())
            };

            // Handles requests received from websocket
            let handle_fut = {
                async move {
                    while let Some(message) = rx.next().await.transpose()? {
                        if message.is_ping() || message.is_pong() {
                            // Do nothing - these messages are handled automatically
                        } else if message.is_close() {
                            // We received the close message, so we need to send a close message to close the sink
                            multiplex_tx.send(warp::ws::Message::close()).await?;
                            // Then we exit the loop which closes the connection
                            break;
                        } else if let Some(response) = Self::handle_raw_request(
                            Arc::clone(&inner),
                            &message,
                            Some(&multiplex_tx),
                        )
                        .await
                        {
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
    async fn handle_raw_request(
        inner: Arc<Inner<D>>,
        request: &Message,
        tx: Option<&mpsc::Sender<Message>>,
    ) -> Option<Message> {
        match serde_json::from_slice(request.as_bytes()) {
            Ok(request) => Self::handle_request(inner, request, tx).await,
            Err(_e) => {
                log::error!("Received invalid JSON from client");
                Some(SingleOrBatch::Single(Response::new_error(
                    Value::Null,
                    RpcError::invalid_request(Some("Received invalid JSON".to_owned())),
                )))
            }
        }
        .map(|response| {
            if request.is_text() {
                Message::text(
                    serde_json::to_string(&response)
                        .expect("Failed to serialize JSON RPC response"),
                )
            } else {
                Message::text(
                    serde_json::to_vec(&response).expect("Failed to serialize JSON RPC response"),
                )
            }
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
    async fn handle_request(
        inner: Arc<Inner<D>>,
        request: SingleOrBatch<Request>,
        tx: Option<&mpsc::Sender<Message>>,
    ) -> Option<SingleOrBatch<Response>> {
        match request {
            SingleOrBatch::Single(request) => Self::handle_single_request(inner, request, tx)
                .await
                .map(SingleOrBatch::Single),

            SingleOrBatch::Batch(requests) => {
                let futures = requests
                    .into_iter()
                    .map(|request| Self::handle_single_request(Arc::clone(&inner), request, tx))
                    .collect::<FuturesUnordered<_>>();

                let responses = futures
                    .filter_map(|response_opt| async { response_opt })
                    .collect::<Vec<Response>>()
                    .await;

                Some(SingleOrBatch::Batch(responses))
            }
        }
    }

    /// Handles a single JSON RPC request
    ///
    /// # TODO
    ///
    /// - Handle subscriptions
    async fn handle_single_request(
        inner: Arc<Inner<D>>,
        request: Request,
        tx: Option<&mpsc::Sender<Message>>,
    ) -> Option<Response> {
        let mut dispatcher = inner.dispatcher.write().await;
        // This ID is only used for streams
        let id = inner.next_id.fetch_add(1, Ordering::SeqCst);

        log::debug!("request: {:#?}", request);

        let response = dispatcher.dispatch(request, tx, id).await;

        log::debug!("response: {:#?}", response);

        response
    }
}

/// A method dispatcher. These take a request and handle the method execution. Can be generated from an `impl` block
/// using `nimiq_jsonrpc_derive::service`.
#[async_trait]
pub trait Dispatcher: Send + Sync + 'static {
    /// Calls the requested method with the request parameters and returns it's return value (or error) as a response.
    async fn dispatch(
        &mut self,
        request: Request,
        tx: Option<&mpsc::Sender<Message>>,
        id: u64,
    ) -> Option<Response>;

    /// Returns whether a method should be dispatched with this dispatcher.
    ///
    /// # Arguments
    ///
    ///  - `name`: The name of the method to be dispatched.
    ///
    /// # Returns
    ///
    /// `true` if this dispatcher can handle the method, `false` otherwise.
    ///
    fn match_method(&self, _name: &str) -> bool {
        true
    }

    /// Returns the names of all methods matched by this dispatcher.
    fn method_names(&self) -> Vec<&str>;
}

/// A dispatcher, that can be composed from other dispatchers.
#[derive(Default)]
pub struct ModularDispatcher {
    dispatchers: Vec<Box<dyn Dispatcher>>,
}

impl ModularDispatcher {
    /// Adds a dispatcher.
    pub fn add<D: Dispatcher>(&mut self, dispatcher: D) {
        self.dispatchers.push(Box::new(dispatcher));
    }
}

#[async_trait]
impl Dispatcher for ModularDispatcher {
    async fn dispatch(
        &mut self,
        request: Request,
        tx: Option<&mpsc::Sender<Message>>,
        id: u64,
    ) -> Option<Response> {
        for dispatcher in &mut self.dispatchers {
            let m = dispatcher.match_method(&request.method);
            log::debug!("Matching '{}' against dispatcher -> {}", request.method, m);
            log::debug!("Methods: {:?}", dispatcher.method_names());
            if m {
                return dispatcher.dispatch(request, tx, id).await;
            }
        }

        method_not_found(request)
    }

    fn method_names(&self) -> Vec<&str> {
        self.dispatchers
            .iter()
            .flat_map(|dispatcher| dispatcher.method_names())
            .collect()
    }
}

/// Dispatcher that only allows specified methods.
pub struct AllowListDispatcher<D>
where
    D: Dispatcher,
{
    /// The underlying dispatcher.
    pub inner: D,

    /// Allowed methods. If `None`, all methods are allowed.
    pub method_allowlist: Option<HashSet<String>>,
}

impl<D> AllowListDispatcher<D>
where
    D: Dispatcher,
{
    /// Creates a new `AllowListDispatcher`.
    ///
    /// # Arguments
    ///
    ///  - `inner`: The underlying dispatcher, which will handle allowed method calls.
    ///  - `method_allowlist`: Names of allowed methods. If `None`, allows all methods.
    ///
    pub fn new(inner: D, method_allowlist: Option<HashSet<String>>) -> Self {
        Self {
            inner,
            method_allowlist,
        }
    }

    fn is_allowed(&self, method: &str) -> bool {
        self.method_allowlist
            .as_ref()
            .map(|method_allowlist| method_allowlist.contains(method))
            .unwrap_or(true)
    }
}

#[async_trait]
impl<D> Dispatcher for AllowListDispatcher<D>
where
    D: Dispatcher,
{
    async fn dispatch(
        &mut self,
        request: Request,
        tx: Option<&mpsc::Sender<Message>>,
        id: u64,
    ) -> Option<Response> {
        if self.is_allowed(&request.method) {
            log::debug!("Dispatching method: {}", request.method);
            self.inner.dispatch(request, tx, id).await
        } else {
            log::debug!("Method not allowed: {}", request.method);
            // If the method is not white-listed, pretend it doesn't exist.
            method_not_found(request)
        }
    }

    fn match_method(&self, name: &str) -> bool {
        if !self.is_allowed(name) {
            log::debug!("Method not allowed: {}", name);
            false
        } else {
            true
        }
    }

    fn method_names(&self) -> Vec<&str> {
        self.inner
            .method_names()
            .into_iter()
            .filter(|method_name| self.is_allowed(method_name))
            .collect()
    }
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
where
    P: for<'de> Deserialize<'de> + Send,
    R: Serialize,
    RpcError: From<E>,
    F: FnOnce(P) -> Fut + Send,
    Fut: Future<Output = Result<R, E>> + Send,
{
    let params = match request.params {
        Some(params) => params,
        None => {
            return error_response(request.id, || {
                RpcError::invalid_params(Some("Missing request parameters.".to_owned()))
            })
        }
    };

    let params = match serde_json::from_value(params) {
        Ok(params) => params,
        Err(e) => {
            log::error!("{}", e);
            return error_response(request.id, || RpcError::invalid_params(Some(e.to_string())));
        }
    };

    let result = f(params).await;

    response(request.id, result)
}

/// Read the request and call a handler function if possible. This variant accepts calls without arguments.
///
/// This is a helper function used by implementations of `Dispatcher`.
///
pub async fn dispatch_method_without_args<R, E, F, Fut>(request: Request, f: F) -> Option<Response>
where
    R: Serialize,
    RpcError: From<E>,
    F: FnOnce() -> Fut + Send,
    Fut: Future<Output = Result<R, E>> + Send,
{
    let result = f().await;

    match request.params {
        Some(Value::Null) | None => {}
        Some(Value::Array(a)) if a.is_empty() => {}
        Some(Value::Object(o)) if o.is_empty() => {}
        _ => {
            return error_response(request.id, || {
                RpcError::invalid_params(Some("Didn't expect any request parameters".to_owned()))
            })
        }
    }

    response(request.id, result)
}

/// Constructs a [`Response`] if necessary (i.e., if the request ID was set).
fn response<R, E>(id_opt: Option<Value>, result: Result<R, E>) -> Option<Response>
where
    R: Serialize,
    RpcError: From<E>,
{
    let response = match (id_opt, result) {
        (Some(id), Ok(retval)) => {
            let retval = serde_json::to_value(retval).expect("Failed to serialize return value");
            Some(Response::new_success(id, retval))
        }
        (Some(id), Err(e)) => Some(Response::new_error(id, RpcError::from(e))),
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
        let e = e();
        log::error!("Error response: {:?}", e);
        Some(Response::new_error(id, e))
    } else {
        None
    }
}

/// Returns an error response for a method that was not found. This returns `None`, if the request doesn't expect a
/// response.
pub fn method_not_found(request: Request) -> Option<Response> {
    let ::nimiq_jsonrpc_core::Request { id, method, .. } = request;

    error_response(id, || {
        RpcError::method_not_found(Some(format!("Method does not exist: {}", method)))
    })
}

async fn forward_notification<T>(
    item: T,
    tx: &mut mpsc::Sender<Message>,
    id: &SubscriptionId,
    method: &str,
) -> Result<(), Error>
where
    T: Serialize + Debug + Send + Sync,
{
    let message = SubscriptionMessage {
        subscription: id.clone(),
        result: item,
    };

    let notification = Request::build::<_, ()>(method.to_owned(), Some(&message), None)?;

    log::debug!("Sending notification: {:?}", notification);

    tx.send(Message::text(serde_json::to_vec(&notification)?))
        .await?;

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
pub fn connect_stream<T, S>(
    stream: S,
    tx: &mpsc::Sender<Message>,
    stream_id: u64,
    method: String,
) -> SubscriptionId
where
    T: Serialize + Debug + Send + Sync,
    S: Stream<Item = T> + Send + 'static,
{
    let mut tx = tx.clone();
    let id: SubscriptionId = stream_id.into();

    {
        let id = id.clone();
        tokio::spawn(async move {
            pin_mut!(stream);

            while let Some(item) = stream.next().await {
                if let Err(e) = forward_notification(item, &mut tx, &id, &method).await {
                    // Break the loop when the channel is closed
                    if let Error::Mpsc(_) = e {
                        break;
                    }

                    log::error!("{}", e);
                }
            }
        });
    }

    id
}
