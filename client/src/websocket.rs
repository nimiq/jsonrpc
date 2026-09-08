use std::{
    collections::HashMap,
    fmt::Debug,
    str::FromStr,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
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

    /// Error in the internal oneshot channel.
    #[error("{0}")]
    OneshotRecv(#[from] oneshot::error::RecvError),

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

/// A websocket JSON-RPC client.
///
pub struct WebsocketClient {
    streams: Arc<RwLock<StreamsMap>>,
    requests: Arc<RwLock<RequestsMap>>,
    sender: RwLock<SplitSink<WebSocketStream<MaybeTlsStream<TcpStream>>, Message>>,
    closed: Arc<watch::Sender<bool>>,
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

        let streams = Arc::new(RwLock::new(HashMap::new()));
        let requests = Arc::new(RwLock::new(HashMap::new()));
        let closed = Arc::new(watch::channel(false).0);

        {
            let streams = Arc::clone(&streams);
            let requests = Arc::clone(&requests);
            let closed = Arc::clone(&closed);

            tokio::spawn(async move {
                while let Some(message_result) = ws_rx.next().await {
                    match message_result {
                        Ok(message) => {
                            if let Err(e) =
                                Self::handle_websocket_message(&streams, &requests, message).await
                            {
                                log::error!("{}", e);
                            }
                        }
                        Err(e) => {
                            log::error!("{}", e);
                        }
                    }
                }

                // The connection ended - either cleanly, or because it died. Propagate that to
                // everyone waiting on it, instead of leaving them pending forever.
                log::debug!("Websocket connection ended, closing streams and pending requests");
                Self::teardown(&streams, &requests, &closed).await;
            });
        }

        Ok(Self {
            next_id: AtomicU64::new(1),
            sender: RwLock::new(ws_tx),
            streams,
            requests,
            closed,
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
        *self.closed.borrow()
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
        let _ = self.closed.subscribe().wait_for(|closed| *closed).await;
    }

    /// Marks the connection as closed and drops every subscription and request sender. This ends
    /// all subscription streams and makes all pending requests resolve with
    /// [`Error::OneshotRecv`].
    ///
    /// The closed flag is set before the maps are cleared: a registration that is racing with this
    /// either observes the flag (and is rejected), or holds the respective lock and is thus
    /// removed by the clear that follows.
    async fn teardown(
        streams: &Arc<RwLock<StreamsMap>>,
        requests: &Arc<RwLock<RequestsMap>>,
        closed: &watch::Sender<bool>,
    ) {
        closed.send_replace(true);
        requests.write().await.clear();
        streams.write().await.clear();
    }

    async fn handle_websocket_message(
        streams: &Arc<RwLock<StreamsMap>>,
        requests: &Arc<RwLock<RequestsMap>>,
        message: Message,
    ) -> Result<(), Error> {
        // FIXME: This will also accept pings
        let data = message.into_text()?;

        log::trace!("Received message: {:?}", data);

        let message = RequestOrResponse::from_str(&data)?;

        match message {
            RequestOrResponse::Request(request) => {
                if request.id.is_some() {
                    log::error!("Received unexpected request, which is not a notification.");
                } else if let Some(params) = request.params {
                    let message: SubscriptionMessage<Value> = serde_json::from_value(params)
                        .expect("Failed to deserialize request parameters");

                    let mut streams = streams.write().await;
                    if let Some(tx) = streams.get_mut(&message.subscription) {
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
                let mut requests = requests.write().await;

                if let Some(tx) = response.id.as_u64().and_then(|id| requests.remove(&id)) {
                    drop(requests);
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
            let mut requests = self.requests.write().await;

            if self.is_closed() {
                return Err(Error::ConnectionClosed);
            }

            let (tx, rx) = oneshot::channel();
            requests.insert(request_id, tx);

            rx
        };

        if let Err(e) = self.sender.write().await.send(message).await {
            // The request was never sent, so nothing will ever resolve it.
            self.requests.write().await.remove(&request_id);
            return Err(e.into());
        }

        let response = rx.await?;
        log::debug!("Received response: {:?}", response);

        Ok(response.into_result()?)
    }

    async fn connect_stream<T: Unpin + 'static>(&self, id: SubscriptionId) -> BoxStream<'static, T>
    where
        T: for<'de> Deserialize<'de> + Debug + Send + Sync,
    {
        let (tx, mut rx) = mpsc::channel(16);

        {
            let mut streams = self.streams.write().await;

            if self.is_closed() {
                // This can't return an error, so signal the dead connection with a stream that
                // has already ended.
                log::error!("Can't subscribe to {}: the connection is closed", id);
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

        if let Some(tx) = self.streams.write().await.remove(&id) {
            log::debug!("Closing stream of subscription ID: {}", id);
            drop(tx);
        } else {
            log::error!("Unknown subscription ID: {}", id);
        }

        Ok(())
    }

    /// Close the websocket connection
    async fn close(&self) {
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

        // Tear down right away instead of waiting for the peer to answer the close handshake -
        // it might never do so, and pending requests and streams must not hang on that.
        Self::teardown(&self.streams, &self.requests, &self.closed).await;
    }
}
