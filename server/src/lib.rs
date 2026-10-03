//! This crate implements a JSON-RPC HTTP server using [axum](https://crates.io/crates/axum). It accepts POST requests
//! at `/` and requests over websocket at `/ws`. Access from web pages is limited to the origins configured in
//! [`Config::cors`].

#![warn(missing_docs)]
#![warn(rustdoc::missing_doc_code_examples)]

use std::{
    collections::{HashMap, HashSet},
    error,
    fmt::{self, Debug},
    future::Future,
    net::{IpAddr, Ipv6Addr, SocketAddr},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
};

use async_trait::async_trait;
use axum::{
    body::{Body, Bytes},
    extract::{ConnectInfo, DefaultBodyLimit, Query, State, WebSocketUpgrade},
    http::{
        header::{AUTHORIZATION, CONTENT_TYPE, ORIGIN},
        response::Builder,
        HeaderValue, Method, StatusCode,
    },
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
use tower_http::cors::{AllowOrigin, CorsLayer};

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
    ///
    /// This is matched against the address of the peer that opened the connection, so it offers no
    /// protection when the server sits behind a reverse proxy: every request then originates from
    /// the proxy itself. IPv4-mapped IPv6 addresses are canonicalized before being matched, so an
    /// IPv4 entry also matches a client that reaches a dual-stack socket.
    pub ip_whitelist: Option<HashSet<IpAddr>>,

    /// Username and password for HTTP basic authentication.
    pub basic_auth: Option<Credentials>,

    /// Cross-Origin Resource Sharing configuration.
    ///
    /// The configured origins do more than fill in the CORS response headers: they are enforced on
    /// the request path. A request that carries an `Origin` header not in the list is rejected with
    /// `403 Forbidden`, on both the HTTP endpoint and the websocket endpoint. Browsers open
    /// websockets and send simple cross-origin POSTs without waiting for a CORS decision, so the
    /// headers alone do not stop a web page from talking to the server. Preflight `OPTIONS`
    /// requests are answered by the CORS layer itself and never reach the endpoints.
    ///
    /// Requests without an `Origin` header, which is what non-browser clients typically send, are
    /// not affected. The header is trivially forged outside a browser, so this only keeps web pages
    /// out; it is no substitute for [`Config::basic_auth`].
    ///
    /// `None` behaves like [`Cors::default`]: no browser origin is allowed.
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

async fn ip_whitelist_middleware<D: Dispatcher>(
    State(state): State<Arc<Inner<D>>>,
    request: axum::extract::Request,
    next: Next,
) -> HttpResponse {
    let ip_whitelist = if let Some(ip_whitelist) = &state.config.ip_whitelist {
        ip_whitelist
    } else {
        // No IP whitelist is configured, so every source IP is allowed
        return next.run(request).await;
    };

    // The whitelist cannot be enforced without knowing where the request came from, so a missing
    // source address is rejected instead of being let through
    let connect_info = request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ConnectInfo(source_address)| *source_address);
    let source_address = if let Some(source_address) = connect_info {
        source_address
    } else {
        log::error!("Rejecting request with an unknown source address");
        return (StatusCode::FORBIDDEN, "source address unknown").into_response();
    };

    // A client connecting over IPv4 to a dual-stack socket is reported as an IPv4-mapped IPv6
    // address, which would never match an IPv4 whitelist entry
    if ip_whitelist.contains(&source_address.ip().to_canonical()) {
        next.run(request).await
    } else {
        log::debug!("Rejecting request from {}: not whitelisted", source_address);
        (StatusCode::FORBIDDEN, "source address not allowed").into_response()
    }
}

async fn origin_middleware<D: Dispatcher>(
    State(state): State<Arc<Inner<D>>>,
    request: axum::extract::Request,
    next: Next,
) -> HttpResponse {
    // Only browsers send an `Origin` header, so a request without one has nothing to check
    let origin = if let Some(origin) = request.headers().get(ORIGIN) {
        origin
    } else {
        return next.run(request).await;
    };

    // The CORS headers alone do not stop a browser from opening a websocket or sending a simple
    // POST, so the configured origins are enforced here as well. An absent CORS configuration
    // allows no origin, matching what the default CORS headers tell the browser.
    let allowed = state
        .config
        .cors
        .as_ref()
        .is_some_and(|cors| cors.allows_origin(origin));

    if allowed {
        next.run(request).await
    } else {
        log::debug!("Rejecting request from origin {:?}: not allowed", origin);
        (StatusCode::FORBIDDEN, "origin not allowed").into_response()
    }
}

/// The origins a browser may talk to the server from
#[derive(Clone, Debug)]
enum AllowedOrigins {
    /// Every origin is allowed
    Any,
    /// Only the listed origins are allowed. An empty list allows no browser origin at all.
    List(HashSet<HeaderValue>),
}

impl Default for AllowedOrigins {
    fn default() -> Self {
        Self::List(HashSet::new())
    }
}

/// CORS configuration
///
/// The origins configured here are used both for the CORS response headers and to reject requests
/// from other origins, see [`Config::cors`].
#[derive(Clone, Debug, Default)]
pub struct Cors {
    allowed_origins: AllowedOrigins,
}

impl Cors {
    /// Create a new instance that allows the `Authorization` and `Content-Type` request headers and
    /// the `POST` method, and no origin at all.
    ///
    /// No origin is allowed until [`Cors::with_origins`] or [`Cors::with_any_origin`] is called.
    pub fn new() -> Self {
        Self::default()
    }

    /// Configure CORS to only allow specific origins. An empty list allows no origin.
    ///
    /// An entry is the origin of a web page, that is its `http(s)://` address without any path,
    /// not the URL of this server. Entries are brought into the form browsers put in the `Origin`
    /// header before they are compared: surrounding whitespace and trailing slashes are removed,
    /// scheme and host are lowercased, IPv6 literals are compressed and the default port of the
    /// scheme is dropped, so `HTTPS://Example.com:443/` matches `https://example.com`. An entry
    /// that does not have the shape `scheme://host[:port]` is rejected, as are wildcard hosts such
    /// as `https://*.example.com` and the opaque origin `null`, which browsers send for sandboxed
    /// frames and `file://` pages and which would therefore let any web page in.
    ///
    /// A `*` entry allows every origin, like [`Cors::with_any_origin`].
    ///
    /// Note that multiple calls to this method will override any previous origin-related calls.
    pub fn with_origins<I, S>(mut self, origins: I) -> Result<Self, InvalidOrigin>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut any = false;
        let mut list = HashSet::new();
        for entry in origins {
            let entry = entry.as_ref();
            if entry.trim() == "*" {
                any = true;
            } else {
                list.insert(canonical_origin(entry)?);
            }
        }

        self.allowed_origins = if any {
            AllowedOrigins::Any
        } else {
            AllowedOrigins::List(list)
        };
        Ok(self)
    }

    /// Configure CORS to allow every origin. Also known as the `*` wildcard.
    ///
    /// This lets any web page talk to the server, including pages the operator merely has open in
    /// a browser on the same machine, which is exactly what the origin check exists to prevent.
    /// Only use it for a server that is protected by [`Config::basic_auth`] or that serves nothing
    /// a stranger could misuse. [`Server::run`] logs a warning when it is in effect.
    ///
    /// Note that multiple calls to this method will override any previous origin-related calls.
    pub fn with_any_origin(mut self) -> Self {
        self.allowed_origins = AllowedOrigins::Any;
        self
    }

    /// Returns whether a request carrying the given `Origin` header may be served.
    pub(crate) fn allows_origin(&self, origin: &HeaderValue) -> bool {
        match &self.allowed_origins {
            AllowedOrigins::Any => true,
            AllowedOrigins::List(origins) => origins.contains(origin),
        }
    }

    /// Returns whether every origin is allowed.
    pub(crate) fn allows_any_origin(&self) -> bool {
        matches!(self.allowed_origins, AllowedOrigins::Any)
    }

    /// Builds the CORS layer from the same origin list that the request path enforces, so the
    /// headers the browser sees can never disagree with what the server accepts.
    pub(crate) fn into_layer(self) -> CorsLayer {
        let allow_origin = match self.allowed_origins {
            AllowedOrigins::Any => AllowOrigin::any(),
            AllowedOrigins::List(origins) => AllowOrigin::list(origins),
        };

        CorsLayer::new()
            .allow_headers([AUTHORIZATION, CONTENT_TYPE])
            .allow_methods([Method::POST])
            .allow_origin(allow_origin)
    }
}

/// An entry passed to [`Cors::with_origins`] that cannot be used as an origin.
#[derive(Clone, Debug, Error)]
#[error("invalid origin {entry:?}: {reason}")]
pub struct InvalidOrigin {
    entry: String,
    reason: &'static str,
}

impl InvalidOrigin {
    /// The offending entry, as it was passed in.
    pub fn entry(&self) -> &str {
        &self.entry
    }
}

/// Brings a configured origin into the form a browser puts in the `Origin` header, so that an
/// operator's `HTTPS://Example.com/` still matches `https://example.com`, or explains why the entry
/// can never match anything.
fn canonical_origin(entry: &str) -> Result<HeaderValue, InvalidOrigin> {
    const SHAPE: &str = "expected `scheme://host[:port]`";
    const PORT: &str = "port must be a number between 0 and 65535";
    let invalid = |reason| InvalidOrigin {
        entry: entry.to_owned(),
        reason,
    };

    let origin = entry.trim().trim_end_matches('/').to_ascii_lowercase();
    if origin == "null" {
        return Err(invalid(
            "the opaque origin `null` is sent by sandboxed frames and would let any web page in",
        ));
    }
    if !origin.is_ascii() {
        return Err(invalid(
            "must be ASCII, use punycode for international host names",
        ));
    }
    if origin.contains('*') {
        return Err(invalid(
            "wildcard hosts are not supported, list each origin",
        ));
    }

    let (scheme, authority) = origin.split_once("://").ok_or_else(|| invalid(SHAPE))?;
    let scheme_is_valid = scheme.starts_with(|c: char| c.is_ascii_alphabetic())
        && scheme
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'));
    // A host cannot contain URL delimiters, userinfo, percent-encoding (browsers decode it) or the
    // other code points the URL standard forbids in hosts
    let authority_is_valid = !authority.is_empty()
        && !authority.contains(|c: char| {
            c.is_whitespace()
                || matches!(
                    c,
                    '/' | '?' | '#' | '@' | '%' | '<' | '>' | '\\' | '^' | '|'
                )
        });
    if !scheme_is_valid || !authority_is_valid {
        return Err(invalid(SHAPE));
    }
    if matches!(scheme, "ws" | "wss") {
        return Err(invalid(
            "an origin is the page's `http(s)://` address, not the server's websocket URL",
        ));
    }

    // Split off the port. An IPv6 literal is bracketed, so the colons inside it are not a port.
    let (host, port) = if let Some(rest) = authority.strip_prefix('[') {
        let (literal, rest) = rest.split_once(']').ok_or_else(|| invalid(SHAPE))?;
        let address: Ipv6Addr = literal
            .parse()
            .map_err(|_| invalid("not a valid IPv6 address"))?;
        let port = match rest.strip_prefix(':') {
            Some(port) => Some(port),
            None if rest.is_empty() => None,
            None => return Err(invalid(SHAPE)),
        };
        (ipv6_host(address), port)
    } else {
        let (host, port) = match authority.rsplit_once(':') {
            Some((host, port)) => (host, Some(port)),
            None => (authority, None),
        };
        if host.is_empty() || host.contains([':', '[', ']']) {
            return Err(invalid(SHAPE));
        }
        (host.to_owned(), port)
    };

    // Browsers leave out the default port of the scheme, so it is dropped here as well
    let port = match port {
        Some(port) if !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()) => {
            Some(port.parse::<u16>().map_err(|_| invalid(PORT))?)
        }
        Some(_) => return Err(invalid(PORT)),
        None => None,
    };
    let default_port = match scheme {
        "http" => Some(80),
        "https" => Some(443),
        _ => None,
    };
    let origin = match port {
        Some(port) if Some(port) != default_port => format!("{scheme}://{host}:{port}"),
        _ => format!("{scheme}://{host}"),
    };

    origin
        .parse::<HeaderValue>()
        .map_err(|_| invalid("not a valid header value"))
}

/// Serializes an IPv6 address the way the URL standard does: compressed, lowercase and, for an
/// IPv4-mapped address, in hex groups rather than the dotted form Rust prints.
fn ipv6_host(address: Ipv6Addr) -> String {
    match address.to_ipv4_mapped() {
        Some(mapped) => {
            let [a, b, c, d] = mapped.octets();
            format!(
                "[::ffff:{:x}:{:x}]",
                u16::from_be_bytes([a, b]),
                u16::from_be_bytes([c, d])
            )
        }
        None => format!("[{address}]"),
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
        if self
            .inner
            .config
            .cors
            .as_ref()
            .is_some_and(Cors::allows_any_origin)
        {
            log::warn!("CORS allows every origin: any web page can talk to this server");
        }

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

        let mut app = Router::new().merge(http_router);

        // The `/ws` route is only mounted when websocket support is enabled, so that disabling it
        // actually removes the endpoint instead of just being advisory
        if self.inner.config.enable_websocket {
            let inner = Arc::clone(&self.inner);
            let ws_router = Router::new().route(
                "/ws",
                any(
                    |Query(params): Query<HashMap<String, String>>,
                     ws: WebSocketUpgrade| async move {
                        Self::upgrade_to_ws(inner, ws, params)
                    },
                ),
            );

            app = app.merge(ws_router);
        }

        let app = app
            .route_layer(axum::middleware::from_fn_with_state(
                Arc::clone(&self.inner),
                basic_auth_middleware,
            ))
            // Each layer wraps the ones added before it, so the checks run in reverse order: IP
            // whitelist first, then origin, then basic auth
            .route_layer(axum::middleware::from_fn_with_state(
                Arc::clone(&self.inner),
                origin_middleware,
            ))
            .route_layer(axum::middleware::from_fn_with_state(
                Arc::clone(&self.inner),
                ip_whitelist_middleware,
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
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
        .unwrap();
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
    ///    send notifications to the client (used for subscriptions).
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
    ///    send notifications to the client (used for subscriptions).
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
        match request.method.as_str() {
            "unsubscribe" => return Self::handle_unsubscribe_stream(request, inner).await,

            // Built-in introspection methods (the `rpc.` prefix is reserved by the JSON-RPC spec
            // for these kinds of system extensions). They let any client discover which methods
            // exist and, in particular, which ones are deprecated, so the client can report
            // deprecated usage.
            "rpc.methods" => {
                let dispatcher = inner.dispatcher.read().await;
                return introspection_response(request.id, &dispatcher.method_names());
            }
            "rpc.deprecatedMethods" => {
                let dispatcher = inner.dispatcher.read().await;
                return introspection_response(
                    request.id,
                    &deprecation_map(&dispatcher.deprecated_methods()),
                );
            }
            _ => {}
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

/// Builds the response for one of the built-in introspection requests. Returns `None` if the
/// request doesn't expect a response (i.e. it is a notification).
fn introspection_response<T: Serialize>(
    id: Option<Value>,
    payload: &T,
) -> Option<ResponseAndSubScriptionNotifier> {
    id.map(|id| {
        let result =
            serde_json::to_value(payload).expect("Failed to serialize introspection response");
        (Response::new_success(id, result), None)
    })
}

/// Builds the `rpc.deprecatedMethods` response payload: an object keyed by method name, with the
/// optional `note` and `since` of each method's `#[deprecated]` attribute.
fn deprecation_map(deprecations: &[MethodDeprecation<'_>]) -> Value {
    Value::Object(
        deprecations
            .iter()
            .map(|deprecation| {
                let mut entry = serde_json::Map::new();
                if let Some(note) = deprecation.note {
                    entry.insert("note".to_owned(), note.into());
                }
                if let Some(since) = deprecation.since {
                    entry.insert("since".to_owned(), since.into());
                }
                (deprecation.method.to_owned(), Value::Object(entry))
            })
            .collect(),
    )
}

/// Deprecation metadata of a single RPC method, extracted from its `#[deprecated]` attribute by
/// the `nimiq_jsonrpc_derive::service` macro and reported by [`Dispatcher::deprecated_methods`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MethodDeprecation<'a> {
    /// The JSON-RPC method name (i.e. after any `rename_all`).
    pub method: &'a str,
    /// The optional note of the `#[deprecated]` attribute.
    pub note: Option<&'a str>,
    /// The optional version of the `#[deprecated(since = "...")]` attribute.
    pub since: Option<&'a str>,
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

    /// Returns deprecation metadata for the methods matched by this dispatcher that are marked
    /// `#[deprecated]`.
    ///
    /// The default implementation returns an empty list. Dispatchers generated by the
    /// `nimiq_jsonrpc_derive::service` macro override this with the methods that carry a
    /// `#[deprecated]` attribute. Clients can query this list through the built-in
    /// `rpc.deprecatedMethods` method.
    fn deprecated_methods(&self) -> Vec<MethodDeprecation<'_>> {
        vec![]
    }
}

/// Logs a warning that a deprecated JSON-RPC method was called, including the note and `since`
/// version of its `#[deprecated]` attribute, if given. This is invoked by dispatchers generated by
/// the `nimiq_jsonrpc_derive::service` macro and is exposed so that the generated code doesn't
/// have to depend on the `log` crate itself.
pub fn log_deprecated(deprecation: &MethodDeprecation<'_>) {
    let since = deprecation
        .since
        .map_or_else(String::new, |since| format!(" (since {})", since));
    match deprecation.note {
        Some(note) => log::warn!(
            "Call to deprecated JSON-RPC method '{}'{}: {}",
            deprecation.method,
            since,
            note
        ),
        None => log::warn!(
            "Call to deprecated JSON-RPC method '{}'{}",
            deprecation.method,
            since
        ),
    }
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

    fn deprecated_methods(&self) -> Vec<MethodDeprecation<'_>> {
        self.dispatchers
            .iter()
            .flat_map(|dispatcher| dispatcher.deprecated_methods())
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

    fn deprecated_methods(&self) -> Vec<MethodDeprecation<'_>> {
        self.inner
            .deprecated_methods()
            .into_iter()
            .filter(|deprecation| self.is_allowed(deprecation.method))
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
        None => Value::Array(Vec::new()),
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

#[cfg(test)]
mod tests {
    use super::*;

    struct StubDispatcher;

    #[async_trait]
    impl Dispatcher for StubDispatcher {
        async fn dispatch(
            &mut self,
            request: Request,
            _tx: Option<&mpsc::Sender<Message>>,
            _id: u64,
            _frame_type: Option<FrameType>,
        ) -> Option<ResponseAndSubScriptionNotifier> {
            method_not_found(request)
        }

        fn method_names(&self) -> Vec<&str> {
            vec!["foo", "oldFoo"]
        }

        fn deprecated_methods(&self) -> Vec<MethodDeprecation<'_>> {
            vec![MethodDeprecation {
                method: "oldFoo",
                note: Some("use foo instead"),
                since: Some("1.0"),
            }]
        }
    }

    fn request(method: &str, id: Option<Value>) -> Request {
        Request::new(method.to_owned(), None, id)
    }

    #[tokio::test]
    async fn it_serves_the_builtin_introspection_methods() {
        let server = Server::new(Config::default(), StubDispatcher);

        let (response, notifier) = Server::handle_single_request(
            Arc::clone(&server.inner),
            request("rpc.methods", Some(1.into())),
            None,
            None,
        )
        .await
        .expect("expected a response");
        assert!(notifier.is_none());
        assert_eq!(response.result, Some(serde_json::json!(["foo", "oldFoo"])));

        let (response, _) = Server::handle_single_request(
            Arc::clone(&server.inner),
            request("rpc.deprecatedMethods", Some(2.into())),
            None,
            None,
        )
        .await
        .expect("expected a response");
        assert_eq!(
            response.result,
            Some(serde_json::json!({
                "oldFoo": { "note": "use foo instead", "since": "1.0" },
            }))
        );
    }

    #[tokio::test]
    async fn introspection_notifications_get_no_response() {
        let server = Server::new(Config::default(), StubDispatcher);

        for method in ["rpc.methods", "rpc.deprecatedMethods"] {
            let response = Server::handle_single_request(
                Arc::clone(&server.inner),
                request(method, None),
                None,
                None,
            )
            .await;
            assert!(
                response.is_none(),
                "notification to {} must get no response",
                method
            );
        }
    }
}
