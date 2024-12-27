//! A client implementation for JSON-RPC over Websocket using web_sys.
//!
//! # EXPERIMENTAL
//!
//! This is still experimental.

use std::{cell::RefCell, collections::HashMap, fmt::Debug, str::FromStr, sync::Arc};

use async_trait::async_trait;
use futures::stream::{BoxStream, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;
use tokio::sync::{mpsc, oneshot, RwLock};
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
}

impl From<JsValue> for Error {
    fn from(v: JsValue) -> Self {
        Self::Js(v)
    }
}

/// A JSON-RPC client over a Javascript websocket
///
pub struct WebsocketClient {
    streams: Arc<RwLock<StreamsMap>>,
    requests: Arc<RwLock<RequestsMap>>,
    next_id: u64,
    sender: mpsc::Sender<Vec<u8>>,
}

impl WebsocketClient {
    /// Creates a new websocket client, connecting to the specified url.
    pub async fn new(url: Url) -> Result<Self, Error> {
        let ws = WebSocket::new(url.as_ref())?;
        ws.set_binary_type(web_sys::BinaryType::Arraybuffer);

        let streams = Arc::new(RwLock::new(HashMap::new()));
        let requests = Arc::new(RwLock::new(HashMap::new()));

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
        let (msg_tx, mut msg_rx) = mpsc::channel::<Vec<u8>>(1);
        wasm_bindgen_futures::spawn_local(async move {
            while let Some(message) = msg_rx.recv().await {
                ws.send_with_u8_array(&message).unwrap();
            }
        });

        // Now wait for the websocket to be open
        ready_rx.await.unwrap();

        // Return the client
        Ok(Self {
            next_id: 1,
            streams,
            requests,
            sender: msg_tx,
        })
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

    async fn send_request<P, R>(&mut self, method: &str, params: &P) -> Result<R, Self::Error>
    where
        P: Serialize + Debug + Send + Sync,
        R: for<'de> Deserialize<'de> + Debug + Send + Sync,
    {
        let request_id = self.next_id;
        self.next_id += 1;

        let request = Request::build(method.to_owned(), Some(params), Some(&request_id))
            .expect("Failed to serialize JSON-RPC request.");

        self.sender
            .send(serde_json::to_vec(&request)?)
            .await
            .unwrap();

        let (tx, rx) = oneshot::channel();

        {
            let mut requests = self.requests.write().await;
            requests.insert(request_id, tx);
        }

        Ok(rx.await?.into_result()?)
    }

    async fn connect_stream<T: Unpin + 'static>(
        &mut self,
        id: SubscriptionId,
    ) -> BoxStream<'static, T>
    where
        T: for<'de> Deserialize<'de> + Debug + Send + Sync,
    {
        let (tx, mut rx) = mpsc::channel(16);

        self.streams.write().await.insert(id, tx);

        let stream = async_stream::stream! {
            loop {
                let message = rx.recv().await.unwrap();
                yield serde_json::from_value(message.result).unwrap();
            }
        };

        stream.boxed()
    }

    async fn disconnect_stream(&mut self, id: SubscriptionId) -> Result<(), Self::Error> {
        if let Some(tx) = self.streams.write().await.remove(&id) {
            log::debug!("Closing stream of subscription ID: {}", id);
            drop(tx);
        } else {
            log::error!("Unknown subscription ID: {}", id);
        }

        Ok(())
    }

    async fn close(&mut self) {}
}
