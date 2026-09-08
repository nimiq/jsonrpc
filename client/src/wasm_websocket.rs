//! A client implementation for JSON-RPC over Websocket using web_sys.
//!
//! # EXPERIMENTAL
//!
//! This is still experimental.

use std::{
    cell::RefCell,
    collections::HashMap,
    fmt::Debug,
    str::FromStr,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
};

use async_trait::async_trait;
use futures::stream::{self, BoxStream, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;
use tokio::sync::{mpsc, oneshot, watch, RwLock};
use url::Url;

use wasm_bindgen::{closure::Closure, JsCast, JsValue};
use web_sys::{ErrorEvent, MessageEvent, WebSocket};

use nimiq_jsonrpc_core::{
    Request, RequestOrResponse, Response, SubscriptionId, SubscriptionMessage,
};

use crate::Client;

type StreamsMap = HashMap<SubscriptionId, mpsc::Sender<SubscriptionMessage<Value>>>;
type RequestsMap = HashMap<u64, oneshot::Sender<Response>>;

/// Error type for this client
#[derive(Debug, Error)]
pub enum Error {
    /// Something on the Javascript side went wrong.
    #[error("JS: {0:?}")]
    Js(JsValue),

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

impl From<JsValue> for Error {
    fn from(v: JsValue) -> Self {
        Self::Js(v)
    }
}

/// Commands for the task that owns the websocket. The websocket itself isn't `Send`, so it can't
/// be held by the client and is driven through this channel instead.
enum Command {
    Send(Vec<u8>),
    Close,
}

/// A JSON-RPC client over a Javascript websocket
///
pub struct WebsocketClient {
    streams: Arc<RwLock<StreamsMap>>,
    requests: Arc<RwLock<RequestsMap>>,
    closed: Arc<watch::Sender<bool>>,
    next_id: AtomicU64,
    sender: mpsc::Sender<Command>,
}

impl WebsocketClient {
    /// Creates a new websocket client, connecting to the specified url.
    pub async fn new(url: Url) -> Result<Self, Error> {
        let ws = WebSocket::new(url.as_ref())?;
        ws.set_binary_type(web_sys::BinaryType::Arraybuffer);

        let streams = Arc::new(RwLock::new(HashMap::new()));
        let requests = Arc::new(RwLock::new(HashMap::new()));
        let closed = Arc::new(watch::channel(false).0);

        // Let the onmessage callback spawn a future to handle the message
        let onmessage_callback = {
            let streams = Arc::clone(&streams);
            let requests = Arc::clone(&requests);

            Closure::wrap(Box::new(move |e: MessageEvent| {
                // TODO: Currently we only send the JSON-RPC as data (blob)
                if let Ok(buf) = e.data().dyn_into::<js_sys::ArrayBuffer>() {
                    let data = js_sys::Uint8Array::new(&buf).to_vec();
                    let data = String::from_utf8(data).unwrap();

                    let streams = Arc::clone(&streams);
                    let requests = Arc::clone(&requests);

                    wasm_bindgen_futures::spawn_local(async move {
                        Self::handle_websocket_message(&streams, &requests, data)
                            .await
                            .unwrap();
                    })
                } else {
                    log::error!("Failed to cast message");
                }
            }) as Box<dyn FnMut(MessageEvent)>)
        };
        ws.set_onmessage(Some(onmessage_callback.as_ref().unchecked_ref()));
        onmessage_callback.forget();

        // Log errors only
        let onerror_callback = Closure::wrap(Box::new(move |e: ErrorEvent| {
            log::error!("Websocket error: {:?}", e);
        }) as Box<dyn FnMut(ErrorEvent)>);
        ws.set_onerror(Some(onerror_callback.as_ref().unchecked_ref()));
        onerror_callback.forget();

        // The connection ended - either cleanly, or because it died. Propagate that to everyone
        // waiting on it, instead of leaving them pending forever.
        let onclose_callback = {
            let streams = Arc::clone(&streams);
            let requests = Arc::clone(&requests);
            let closed = Arc::clone(&closed);

            Closure::wrap(Box::new(move |e: JsValue| {
                log::debug!("Websocket closed: {:?}", e);

                let streams = Arc::clone(&streams);
                let requests = Arc::clone(&requests);
                let closed = Arc::clone(&closed);

                wasm_bindgen_futures::spawn_local(async move {
                    Self::teardown(&streams, &requests, &closed).await;
                })
            }) as Box<dyn FnMut(JsValue)>)
        };
        ws.set_onclose(Some(onclose_callback.as_ref().unchecked_ref()));
        onclose_callback.forget();

        // Register onopen so we can wait for the websocket to be open
        let (ready_tx, ready_rx) = oneshot::channel::<()>();
        let ready_tx = RefCell::new(Some(ready_tx));
        let onopen_callback = Closure::wrap(Box::new(move |_| {
            if let Some(ready_tx) = ready_tx.replace(None) {
                ready_tx.send(()).unwrap();
            }
        }) as Box<dyn FnMut(JsValue)>);

        ws.set_onopen(Some(onopen_callback.as_ref().unchecked_ref()));
        onopen_callback.forget();

        // Spawn future to do the sending for us
        let (msg_tx, mut msg_rx) = mpsc::channel::<Command>(1);
        wasm_bindgen_futures::spawn_local(async move {
            while let Some(command) = msg_rx.recv().await {
                match command {
                    Command::Send(message) => {
                        if let Err(e) = ws.send_with_u8_array(&message) {
                            log::error!("Failed to send message: {:?}", e);
                        }
                    }
                    Command::Close => {
                        if let Err(e) = ws.close() {
                            log::error!("Failed to close the websocket: {:?}", e);
                        }
                        break;
                    }
                }
            }
        });

        // Now wait for the websocket to be open
        ready_rx.await.unwrap();

        // Return the client
        Ok(Self {
            next_id: AtomicU64::new(1),
            streams,
            requests,
            closed,
            sender: msg_tx,
        })
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
    /// fail.
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
        data: String,
    ) -> Result<(), Error> {
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

        let message = Command::Send(serde_json::to_vec(&request)?);

        // Register the request *before* sending it: the response can arrive as soon as the send
        // completes, and `handle_websocket_message` discards responses it can't match to a
        // pending request.
        let rx = {
            let mut requests = self.requests.write().await;

            if self.is_closed() {
                return Err(Error::ConnectionClosed);
            }

            let (tx, rx) = oneshot::channel();
            requests.insert(request_id, tx);

            rx
        };

        if self.sender.send(message).await.is_err() {
            // The task owning the websocket is gone, so the request was never sent and nothing
            // will ever resolve it.
            self.requests.write().await.remove(&request_id);
            return Err(Error::ConnectionClosed);
        }

        Ok(rx.await?.into_result()?)
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

        // End the stream when the sender is dropped, instead of panicking.
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

    async fn close(&self) {
        // Ask the task owning the websocket to close it. We don't do anything if it fails.
        let _ = self.sender.send(Command::Close).await;

        // Tear down right away instead of waiting for the `onclose` callback - it might never
        // fire, and pending requests and streams must not hang on that.
        Self::teardown(&self.streams, &self.requests, &self.closed).await;
    }
}
