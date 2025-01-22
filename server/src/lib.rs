//! This crate implements a JSON-RPC HTTP server using [warp](https://crates.io/crates/warp). It accepts POST requests
//! at `/` and requests over websocket at `/ws`.

#![warn(missing_docs)]
#![warn(rustdoc::missing_doc_code_examples)]

use std::{
    collections::{HashMap, HashSet},
    error,
    fmt::{self, Debug},
    future::Future,
    net::{IpAddr, SocketAddr},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
};

use async_trait::async_trait;
use axum::{
    body::{Body, Bytes},
    extract::{DefaultBodyLimit, Query, State, WebSocketUpgrade},
    http::{header::CONTENT_TYPE, response::Builder, HeaderValue, Method, StatusCode},
    middleware::Next,
    response::{IntoResponse as _, Response as HttpResponse},
    routing::{any, post},
    Router,
};
use axum_extra::{
    headers::{authorization::Basic, Authorization},
    TypedHeader,
};
use blake2::{digest::consts::U32, Blake2b, Digest};
use futures::{
    pin_mut,
    sink::SinkExt,
    stream::{FuturesUnordered, StreamExt},
    Stream,
};
use serde::{de::Deserialize, ser::Serialize};
use serde_json::Value;
use subtle::ConstantTimeEq;
use thiserror::Error;
use tokio::{
    net::TcpListener,
    sync::{mpsc, RwLock, RwLockReadGuard, RwLockWriteGuard},
};

use nimiq_jsonrpc_core::{
    FrameType, Request, Response, RpcError, Sensitive, SingleOrBatch, SubscriptionId,
    SubscriptionMessage,
};

pub use axum::extract::ws::Message;
pub use tokio::sync::Notify;
use tower_http::cors::{Any, CorsLayer};

/// Type defining a response and a possible notify handle used to terminate a subscription stream
pub type ResponseAndSubScriptionNotifier = (Response, Option<Arc<Notify>>);

/// A server error.
#[derive(Debug, Error)]
pub enum Error {
    /// Error returned by axum
    #[error("HTTP error: {0}")]
    Axum(#[from] axum::Error),

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

    /// Cross-Origin Resource Sharing configuration
    pub cors: Option<Cors>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            bind_to: ([127, 0, 0, 1], 8000).into(),
            enable_websocket: true,
            ip_whitelist: None,
            basic_auth: None,
            cors: None,
        }
    }
}

fn blake2b(bytes: &[u8]) -> [u8; 32] {
    *Blake2b::<U32>::digest(bytes).as_ref()
}

async fn basic_auth_middleware<D: Dispatcher>(
    State(state): State<Arc<Inner<D>>>,
    basic_auth_header: Option<TypedHeader<Authorization<Basic>>>,
    request: axum::extract::Request,
    next: Next,
) -> HttpResponse {
    let auth_config = if let Some(auth_config) = &state.config.basic_auth {
        auth_config
    } else {
        // No basic auth is configured
        return next.run(request).await;
    };

    let auth_header = if let Some(auth_header) = basic_auth_header {
        auth_header
    } else {
        // Basic auth is configured but Authorization header is not (correctly) provided
        return StatusCode::UNAUTHORIZED.into_response();
    };

    if auth_config
        .verify(auth_header.username(), auth_header.password())
        .is_ok()
    {
        // Everything checks out, access granted
        next.run(request).await
    } else {
        // Invalid username and/or password provided
        StatusCode::UNAUTHORIZED.into_response()
    }
}

#[derive(Clone, Debug)]
/// CORS configuration
pub struct Cors(CorsLayer);

impl Cors {
    /// Create a new instance with `Content-Type` as mandatory header and `POST` as mandatory method.
    pub fn new() -> Self {
        Self(
            CorsLayer::new()
                .allow_headers([CONTENT_TYPE])
                .allow_methods([Method::POST]),
        )
    }

    /// Configure CORS to only allow specific origins.
    /// Note that multiple calls to this method will override any previous origin-related calls.
    pub fn with_origins(mut self, origins: Vec<&str>) -> Self {
        self.0 = self.0.allow_origin::<Vec<HeaderValue>>(
            origins
                .iter()
                .map(|o| o.parse::<HeaderValue>().unwrap())
                .collect(),
        );
        self
    }

    /// Configure CORS to allow every origin. Also known as the `*` wildcard.
    /// Note that multiple calls to this method will override any previous origin-related calls.
    pub fn with_any_origin(mut self) -> Self {
        self.0 = self.0.allow_origin(Any);
        self
    }

    pub(crate) fn into_layer(self) -> CorsLayer {
        self.0
    }
}

impl Default for Cors {
    fn default() -> Self {
        Self::new()
    }
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
    subscription_notifiers: RwLock<HashMap<SubscriptionId, Arc<Notify>>>,
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
                subscription_notifiers: RwLock::new(HashMap::new()),
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
        let inner = Arc::clone(&self.inner);
        let http_router = Router::new().route(
            "/",
            post(|body: Bytes| async move {
                let data = Self::handle_raw_request(inner, &Message::binary(body), None, None)
                    .await
                    .unwrap_or(Message::Binary(Bytes::new()));

                Builder::new()
                    .status(StatusCode::OK)
                    .header(CONTENT_TYPE, "application/json")
                    .body(Body::from(data.into_data().to_owned()))
                    .unwrap() // As long as the hard-coded status code and content-type is correct, this won't fail.
            }),
        );

        let inner = Arc::clone(&self.inner);
        let ws_router = Router::new().route(
            "/ws",
            any(
                |Query(params): Query<HashMap<String, String>>, ws: WebSocketUpgrade| async move {
                    Self::upgrade_to_ws(inner, ws, params)
                },
            ),
        );

        let app = Router::new()
            .merge(http_router)
            .merge(ws_router)
            .route_layer(axum::middleware::from_fn_with_state(
                Arc::clone(&self.inner),
                basic_auth_middleware,
            ))
            .layer(DefaultBodyLimit::max(1024 * 1024 /* 1MB */))
            .layer(
                self.inner
                    .config
                    .cors
                    .clone()
                    .unwrap_or_default()
                    .into_layer(),
            )
            .with_state(Arc::clone(&self.inner));

        let listener = TcpListener::bind(self.inner.config.bind_to).await.unwrap();
        axum::serve(listener, app).await.unwrap();
    }

    /// Upgrades a connection to websocket. This creates message queues and tasks to forward messages between them.
    ///
    /// We need a MPSC queue to be able to pass sender halves to called functions. The called functions then can keep
    /// the sender for sending notifications to the client.
    ///
    /// # TODO:
    ///
    ///  - Make the queue size configurable
    ///
    fn upgrade_to_ws(
        inner: Arc<Inner<D>>,
        ws: WebSocketUpgrade,
        query_params: HashMap<String, String>,
    ) -> HttpResponse<Body> {
        let frame_type: Option<FrameType> = query_params
            .get("frame")
            .map(|frame_type| Some(frame_type.into()))
            .unwrap_or_default();

        ws.on_upgrade(move |websocket| {
            let (mut tx, mut rx) = websocket.split();

            let (multiplex_tx, mut multiplex_rx) = mpsc::channel::<Message>(16); // TODO: What size?

            // Forwards multiplexer queue output to websocket
            let forward_fut = async move {
                while let Some(data) = multiplex_rx.recv().await {
                    // Close the sink if we get a close message (don't echo the message since this is not permitted)
                    if matches!(data, Message::Close(_)) {
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
                        if matches!(message, Message::Ping(_))
                            || matches!(message, Message::Pong(_))
                        {
                            // Do nothing - these messages are handled automatically
                        } else if matches!(message, Message::Close(_)) {
                            // We received the close message, so we need to send a close message to close the sink
                            multiplex_tx.send(Message::Close(None)).await?;
                            // Then we exit the loop which closes the connection
                            break;
                        } else if let Some(response) = Self::handle_raw_request(
                            Arc::clone(&inner),
                            &message,
                            Some(&multiplex_tx),
                            frame_type,
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
    ///  - `frame_type`: If the request was received over websocket, indicate whether notifications are send back as Text or Binary frames.
    ///
    async fn handle_raw_request(
        inner: Arc<Inner<D>>,
        request: &Message,
        tx: Option<&mpsc::Sender<Message>>,
        frame_type: Option<FrameType>,
    ) -> Option<Message> {
        match serde_json::from_slice(request.clone().into_data().as_ref()) {
            Ok(request) => Self::handle_request(inner, request, tx, frame_type).await,
            Err(_e) => {
                log::error!("Received invalid JSON from client");
                Some(SingleOrBatch::Single(Response::new_error(
                    Value::Null,
                    RpcError::invalid_request(Some("Received invalid JSON".to_owned())),
                )))
            }
        }
        .map(|response| {
            if matches!(&request, Message::Text(_)) {
                Message::text(
                    serde_json::to_string(&response)
                        .expect("Failed to serialize JSON RPC response"),
                )
            } else {
                Message::binary(
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
    ///  - `frame_type`: If the request was received over websocket, indicate whether notifications are send back as Text or Binary frames.
    ///
    async fn handle_request(
        inner: Arc<Inner<D>>,
        request: SingleOrBatch<Request>,
        tx: Option<&mpsc::Sender<Message>>,
        frame_type: Option<FrameType>,
    ) -> Option<SingleOrBatch<Response>> {
        match request {
            SingleOrBatch::Single(request) => {
                Self::handle_single_request(inner, request, tx, frame_type)
                    .await
                    .map(|(response, _)| SingleOrBatch::Single(response))
            }

            SingleOrBatch::Batch(requests) => {
                let futures = requests
                    .into_iter()
                    .map(|request| {
                        Self::handle_single_request(Arc::clone(&inner), request, tx, frame_type)
                    })
                    .collect::<FuturesUnordered<_>>();

                let responses = futures
                    .filter_map(|response_opt| async { response_opt.map(|(response, _)| response) })
                    .collect::<Vec<Response>>()
                    .await;

                Some(SingleOrBatch::Batch(responses))
            }
        }
    }

    /// Handles a single JSON RPC request
    async fn handle_single_request(
        inner: Arc<Inner<D>>,
        request: Request,
        tx: Option<&mpsc::Sender<Message>>,
        frame_type: Option<FrameType>,
    ) -> Option<ResponseAndSubScriptionNotifier> {
        if request.method == "unsubscribe" {
            return Self::handle_unsubscribe_stream(request, inner).await;
        }

        let mut dispatcher = inner.dispatcher.write().await;
        // This ID is only used for streams
        let id = inner.next_id.fetch_add(1, Ordering::SeqCst);

        log::debug!("request: {:#?}", request);

        let response = dispatcher.dispatch(request, tx, id, frame_type).await;

        log::debug!("response: {:#?}", response);

        if let Some((_, Some(ref handler))) = response {
            inner
                .subscription_notifiers
                .write()
                .await
                .insert(SubscriptionId::Number(id), handler.clone());
        }

        response
    }

    async fn handle_unsubscribe_stream(
        request: Request,
        inner: Arc<Inner<D>>,
    ) -> Option<ResponseAndSubScriptionNotifier> {
        let params = if let Some(params) = request.params {
            params
        } else {
            return error_response(request.id, || {
                RpcError::invalid_request(Some(
                    "Missing request parameter containing a list of subscription ids".to_owned(),
                ))
            });
        };

        let subscription_ids =
            if let Ok(ids) = serde_json::from_value::<Vec<SubscriptionId>>(params) {
                ids
            } else {
                return error_response(request.id, || {
                    RpcError::invalid_params(Some(
                        "A list of subscription ids is not provided".to_owned(),
                    ))
                });
            };

        if subscription_ids.is_empty() {
            return error_response(request.id, || {
                RpcError::invalid_params(Some("Empty list of subscription ids provided".to_owned()))
            });
        }

        let mut terminated_streams = vec![];
        let mut subscription_notifiers = inner.subscription_notifiers.write().await;
        for id in subscription_ids.iter() {
            if let Some(notifier) = subscription_notifiers.remove(id) {
                notifier.notify_one();
                terminated_streams.push(id);
            }
        }

        Some((
            Response::new_success(
                serde_json::to_value(request.id.unwrap_or_default()).unwrap(),
                serde_json::to_value(terminated_streams).unwrap(),
            ),
            None,
        ))
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
        frame_type: Option<FrameType>,
    ) -> Option<ResponseAndSubScriptionNotifier>;

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
        frame_type: Option<FrameType>,
    ) -> Option<ResponseAndSubScriptionNotifier> {
        for dispatcher in &mut self.dispatchers {
            let m = dispatcher.match_method(&request.method);
            log::debug!("Matching '{}' against dispatcher -> {}", request.method, m);
            log::debug!("Methods: {:?}", dispatcher.method_names());
            if m {
                return dispatcher.dispatch(request, tx, id, frame_type).await;
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
        frame_type: Option<FrameType>,
    ) -> Option<ResponseAndSubScriptionNotifier> {
        if self.is_allowed(&request.method) {
            log::debug!("Dispatching method: {}", request.method);
            self.inner.dispatch(request, tx, id, frame_type).await
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
pub async fn dispatch_method_with_args<P, R, E, F, Fut>(
    request: Request,
    f: F,
) -> Option<ResponseAndSubScriptionNotifier>
where
    P: for<'de> Deserialize<'de> + Send,
    R: Serialize,
    RpcError: From<E>,
    F: FnOnce(P) -> Fut + Send,
    Fut: Future<Output = Result<(R, Option<Arc<Notify>>), E>> + Send,
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
pub async fn dispatch_method_without_args<R, E, F, Fut>(
    request: Request,
    f: F,
) -> Option<ResponseAndSubScriptionNotifier>
where
    R: Serialize,
    RpcError: From<E>,
    F: FnOnce() -> Fut + Send,
    Fut: Future<Output = Result<(R, Option<Arc<Notify>>), E>> + Send,
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
fn response<R, E>(
    id_opt: Option<Value>,
    result: Result<(R, Option<Arc<Notify>>), E>,
) -> Option<ResponseAndSubScriptionNotifier>
where
    R: Serialize,
    RpcError: From<E>,
{
    let response = match (id_opt, result) {
        (Some(id), Ok((value, subscription))) => {
            let retval = serde_json::to_value(value).expect("Failed to serialize return value");
            Some((Response::new_success(id, retval), subscription))
        }
        (Some(id), Err(e)) => Some((Response::new_error(id, RpcError::from(e)), None)),
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
pub fn error_response<E>(id_opt: Option<Value>, e: E) -> Option<ResponseAndSubScriptionNotifier>
where
    E: FnOnce() -> RpcError,
{
    if let Some(id) = id_opt {
        let e = e();
        log::error!("Error response: {:?}", e);
        Some((Response::new_error(id, e), None))
    } else {
        None
    }
}

/// Returns an error response for a method that was not found. This returns `None`, if the request doesn't expect a
/// response.
pub fn method_not_found(request: Request) -> Option<ResponseAndSubScriptionNotifier> {
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
    frame_type: Option<FrameType>,
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

    let message = match frame_type {
        Some(FrameType::Text) => Message::text(serde_json::to_string(&notification)?),
        Some(FrameType::Binary) | None => Message::binary(serde_json::to_vec(&notification)?),
    };

    tx.send(message).await?;

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
    notify_handler: Arc<Notify>,
    frame_type: Option<FrameType>,
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

            let notify_future = notify_handler.notified();
            pin_mut!(notify_future);

            loop {
                tokio::select! {
                    item = stream.next() => {
                        match item {
                            Some(notification) => {
                                if let Err(error) = forward_notification(notification, &mut tx, &id, &method, frame_type).await {
                                    // Break the loop when the channel is closed
                                    if let Error::Mpsc(_) = error {
                                        break;
                                    }

                                    log::error!("{}", error);
                                }
                            },
                            None => break,
                        }
                    }
                    _ = &mut notify_future => {
                        // Break the loop when an unsubscribe notification is received
                        break;
                    }
                }
            }
        });
    }

    id
}
