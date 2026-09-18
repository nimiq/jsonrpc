use std::{
    collections::HashMap,
    fmt::Debug,
    str::FromStr,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex, MutexGuard, PoisonError,
    },
};

use async_trait::async_trait;
use base64::Engine;
use futures::{
    sink::SinkExt,
    stream::{self, BoxStream, SplitSink, StreamExt},
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;
use tokio::{
    net::TcpStream,
    sync::{mpsc, oneshot, watch, RwLock},
    task::AbortHandle,
};
use tokio_tungstenite::tungstenite::{
    client::IntoClientRequest,
    protocol::{frame::coding::CloseCode, CloseFrame},
    Message,
};
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};
use url::Url;

use nimiq_jsonrpc_core::{
    Request, RequestOrResponse, Response, SubscriptionId, SubscriptionMessage,
};

use crate::{Client, Credentials};

/// Error type returned by websocket client.
#[derive(Debug, Error)]
pub enum Error {
    /// HTTP error
    #[error("HTTP protocol error: {0}")]
    HTTP(#[from] http::Error),

    /// Websocket error
    #[error("Websocket protocol error: {0}")]
    Websocket(#[from] tokio_tungstenite::tungstenite::Error),

    /// JSON-RPC protocol error
    #[error("JSON-RPC protocol error: {0}")]
    JsonRpc(#[from] nimiq_jsonrpc_core::Error),

    /// JSON error
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    /// Error in the internal MPSC channel.
    #[error("{0}")]
    MpscSend(#[from] mpsc::error::SendError<SubscriptionMessage<Value>>),

    /// The websocket connection is closed, either because it was closed explicitly, or because it
    /// died. The client can't be used anymore and has to be re-created.
    #[error("The websocket connection is closed")]
    ConnectionClosed,
}

type StreamsMap = HashMap<SubscriptionId, mpsc::Sender<SubscriptionMessage<Value>>>;
type RequestsMap = HashMap<u64, oneshot::Sender<Response>>;

/// Connection state shared between the client and its reader task.
///
/// The maps are behind synchronous locks rather than async ones: nothing is ever awaited while
/// holding one, and [`SharedConnectionState::teardown`] can thus run synchronously, in particular
/// from a [`TeardownGuard`].
struct SharedConnectionState {
    streams: Mutex<StreamsMap>,
    requests: Mutex<RequestsMap>,
    closed: watch::Sender<bool>,
}

impl SharedConnectionState {
    fn new() -> Self {
        Self {
            streams: Mutex::new(HashMap::new()),
            requests: Mutex::new(HashMap::new()),
            closed: watch::channel(false).0,
        }
    }

    fn is_closed(&self) -> bool {
        *self.closed.borrow()
    }

    /// Marks the connection as closed and drops every subscription and request sender. This ends
    /// all subscription streams and makes all pending requests resolve with
    /// [`Error::ConnectionClosed`]. Running this more than once is harmless.
    ///
    /// The closed flag is set before the maps are cleared: a registration that is racing with this
    /// either observes the flag (and is rejected), or holds the respective lock and is thus
    /// removed by the clear that follows.
    fn teardown(&self) {
        self.closed.send_replace(true);
        lock(&self.requests).clear();
        lock(&self.streams).clear();
    }
}

/// Locks one of the maps in [`SharedConnectionState`]. Poisoning is ignored: every critical
/// section is a single map operation, so a panic inside one leaves the map consistent. This must
/// not panic itself, as it also runs from [`TeardownGuard`] during unwinding.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Tears the connection down when dropped. The reader task holds one, so that the teardown runs
/// however the task ends: because the connection ended, because it panicked, or because it was
/// aborted.
struct TeardownGuard(Arc<SharedConnectionState>);

impl Drop for TeardownGuard {
    fn drop(&mut self) {
        self.0.teardown();
    }
}

/// Unregisters a pending request when dropped, so that a `send_request` that is cancelled (e.g.
/// by a timeout) before its request went out doesn't leave an entry behind that nothing will ever
/// resolve. Once the response arrived, the reader task has removed the entry already and this is
/// a no-op.
struct RequestGuard<'a> {
    shared: &'a SharedConnectionState,
    id: u64,
}

impl Drop for RequestGuard<'_> {
    fn drop(&mut self) {
        lock(&self.shared.requests).remove(&self.id);
    }
}

/// A websocket JSON-RPC client.
///
pub struct WebsocketClient {
    shared: Arc<SharedConnectionState>,
    sender: RwLock<SplitSink<WebSocketStream<MaybeTlsStream<TcpStream>>, Message>>,
    reader: AbortHandle,
    next_id: AtomicU64,
}

impl WebsocketClient {
    /// Creates a new JSON-RPC websocket client.
    ///
    /// # Arguments
    ///
    ///  - `url`: The URL of the websocket endpoint (.e.g `ws://localhost:8000/ws`)
    ///  - `basic_auth`: Credentials for HTTP basic auth.
    ///
    pub async fn new(url: Url, basic_auth: Option<Credentials>) -> Result<Self, Error> {
        let request = {
            let uri: http::Uri = url.to_string().parse().unwrap();
            let mut request = uri.into_client_request()?;

            if let Some(basic_auth) = basic_auth {
                let header_value = format!(
                    "Basic {}",
                    base64::prelude::BASE64_STANDARD
                        .encode(format!("{}:{}", basic_auth.username, basic_auth.password.0))
                );
                request.headers_mut().append(
                    "Authorization",
                    header_value
                        .parse()
                        .map_err(|e| Error::HTTP(http::Error::from(e)))?,
                );
            }

            request
        };

        log::debug!("HTTP request: {:?}", request);

        let (ws_stream, _) = connect_async(request).await?;

        let (ws_tx, mut ws_rx) = ws_stream.split();

        let shared = Arc::new(SharedConnectionState::new());

        let reader = tokio::spawn({
            let shared = Arc::clone(&shared);

            async move {
                // Propagates the end of the connection - however it ends - to everyone waiting
                // on it, instead of leaving them pending forever.
                let _teardown = TeardownGuard(Arc::clone(&shared));

                while let Some(message_result) = ws_rx.next().await {
                    match message_result {
                        Ok(message) => {
                            if let Err(e) = Self::handle_websocket_message(&shared, message).await {
                                log::error!("{}", e);
                            }
                        }
                        Err(e) => {
                            log::error!("{}", e);
                        }
                    }
                }

                log::debug!("Websocket connection ended, closing streams and pending requests");
            }
        })
        .abort_handle();

        Ok(Self {
            next_id: AtomicU64::new(1),
            sender: RwLock::new(ws_tx),
            shared,
            reader,
        })
    }

    /// Creates a new JSON-RPC websocket client.
    ///
    /// # Arguments
    ///
    ///  - `url`: The URL of the websocket endpoint (.e.g `ws://localhost:8000/ws`)
    ///
    pub async fn with_url(url: Url) -> Result<Self, Error> {
        Self::new(url, None).await
    }

    /// Returns whether the connection is closed, i.e. whether it either died or was closed with
    /// [`Client::close`]. A closed client can't be used anymore and has to be re-created.
    pub fn is_closed(&self) -> bool {
        self.shared.is_closed()
    }

    /// Resolves as soon as the connection is closed, i.e. as soon as it either dies or is closed
    /// with [`Client::close`]. Resolves immediately if it is closed already.
    ///
    /// This allows detecting a dead connection without waiting for a request or a subscription to
    /// fail:
    ///
    /// ```no_run
    /// # async fn example(client: nimiq_jsonrpc_client::websocket::WebsocketClient) {
    /// client.closed().await;
    /// // Reconnect here.
    /// # }
    /// ```
    pub async fn closed(&self) {
        // This only fails if the sender was dropped, which can't happen while `self` is alive.
        let _ = self
            .shared
            .closed
            .subscribe()
            .wait_for(|closed| *closed)
            .await;
    }

    async fn handle_websocket_message(
        shared: &SharedConnectionState,
        message: Message,
    ) -> Result<(), Error> {
        // Pings are answered by tokio-tungstenite, and a close frame ends the stream on its own.
        let data = match message {
            Message::Text(_) | Message::Binary(_) => message.into_text()?,
            Message::Ping(_) | Message::Pong(_) | Message::Close(_) | Message::Frame(_) => {
                return Ok(())
            }
        };

        log::trace!("Received message: {:?}", data);

        let message = RequestOrResponse::from_str(&data)?;

        match message {
            RequestOrResponse::Request(request) => {
                if request.id.is_some() {
                    log::error!("Received unexpected request, which is not a notification.");
                } else if let Some(params) = request.params {
                    let message: SubscriptionMessage<Value> = serde_json::from_value(params)?;

                    // Copy the sender out rather than sending under the lock: a subscriber that
                    // stops polling its stream parks this task on its full channel, which must
                    // not block everyone else on the map, in particular the teardown.
                    let tx = lock(&shared.streams).get(&message.subscription).cloned();

                    if let Some(tx) = tx {
                        tx.send(message).await?;
                    } else {
                        log::error!(
                            "Notification for unknown stream ID: {}",
                            message.subscription
                        );
                    }
                } else {
                    log::error!("No 'params' field in notification.");
                }
            }
            RequestOrResponse::Response(response) => {
                let tx = response
                    .id
                    .as_u64()
                    .and_then(|id| lock(&shared.requests).remove(&id));

                if let Some(tx) = tx {
                    tx.send(response).ok();
                } else {
                    log::error!("Response for unknown request ID: {}", response.id);
                }
            }
        }

        Ok(())
    }
}

#[async_trait]
impl Client for WebsocketClient {
    type Error = Error;

    async fn send_request<P, R>(&self, method: &str, params: &P) -> Result<R, Self::Error>
    where
        P: Serialize + Debug + Send + Sync,
        R: for<'de> Deserialize<'de> + Debug + Send + Sync,
    {
        let request_id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let request = Request::build(method.to_owned(), Some(params), Some(&request_id))
            .expect("Failed to serialize JSON-RPC request.");

        log::debug!("Sending request: {:?}", request);

        let message = Message::binary(serde_json::to_vec(&request)?);

        // Register the request *before* sending it: the response can arrive as soon as the send
        // completes, and the reader task discards responses it can't match to a pending request.
        let rx = {
            let mut requests = lock(&self.shared.requests);

            if self.is_closed() {
                return Err(Error::ConnectionClosed);
            }

            let (tx, rx) = oneshot::channel();
            requests.insert(request_id, tx);

            rx
        };
        let _guard = RequestGuard {
            shared: &self.shared,
            id: request_id,
        };

        self.sender.write().await.send(message).await?;

        // The only way the sender goes away without a response is the teardown.
        let response = rx.await.map_err(|_| Error::ConnectionClosed)?;
        log::debug!("Received response: {:?}", response);

        Ok(response.into_result()?)
    }

    async fn connect_stream<T: Unpin + 'static>(&self, id: SubscriptionId) -> BoxStream<'static, T>
    where
        T: for<'de> Deserialize<'de> + Debug + Send + Sync,
    {
        let (tx, mut rx) = mpsc::channel(16);

        {
            let mut streams = lock(&self.shared.streams);

            if self.is_closed() {
                // This can't return an error, so signal the dead connection with a stream that
                // has already ended.
                log::warn!("Can't subscribe to {}: the connection is closed", id);
                return stream::empty().boxed();
            }

            streams.insert(id, tx);
        }

        let stream = async_stream::stream! {
            while let Some(message) = rx.recv().await {
                yield serde_json::from_value(message.result).unwrap();
            }
        };

        stream.boxed()
    }

    async fn disconnect_stream(&self, id: SubscriptionId) -> Result<(), Self::Error> {
        if self.is_closed() {
            // The connection is gone, so all streams ended already.
            return Ok(());
        }

        if let Some(tx) = lock(&self.shared.streams).remove(&id) {
            log::debug!("Closing stream of subscription ID: {}", id);
            drop(tx);
        } else {
            log::error!("Unknown subscription ID: {}", id);
        }

        Ok(())
    }

    /// Close the websocket connection
    async fn close(&self) {
        // Tear down before anything that can stall: sending the close frame below blocks on a
        // full write buffer, and pending requests and streams must not hang on that. The reader
        // has nothing left to deliver to afterwards, and the peer may never answer the close
        // handshake anyway, so stop it rather than leaving it around until the peer goes away.
        self.shared.teardown();
        self.reader.abort();

        // Try to send the close message
        // We don't do anything if it fails
        let _ = self
            .sender
            .write()
            .await
            .send(Message::Close(Some(CloseFrame {
                code: CloseCode::Normal,
                reason: "".into(),
            })))
            .await;
    }
}
