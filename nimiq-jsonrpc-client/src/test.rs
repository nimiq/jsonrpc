use std::{
    collections::HashMap,
    sync::Arc,
    fmt::Debug
};

use tokio::{
    sync::{mpsc, oneshot, RwLock},
};
use thiserror::Error;
use serde::{Serialize, Deserialize};
use futures::{
    stream::{BoxStream, StreamExt, SplitSink},
    sink::SinkExt,
};
use serde_json::Value;
use async_trait::async_trait;

use nimiq_jsonrpc_core::{Request, Response, RequestOrResponse, SubscriptionMessage, SubscriptionId};

use crate::Client;

/// Error type returned by websocket client.
#[derive(Debug, Error)]
pub enum Error {
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


type StreamsMap = HashMap<SubscriptionId, mpsc::Sender<SubscriptionMessage<Value>>>;
type RequestsMap = HashMap<u64, oneshot::Sender<Response>>;


pub struct TestClient {
    streams: Arc<RwLock<StreamsMap>>,
    requests: Arc<RwLock<RequestsMap>>,
    sender: mpsc::Sender<Value>,
    next_id: u64,
}


impl TestClient {
    /// Creates a new JSON-RPC websocket client.
    ///
    /// # Arguments
    ///
    ///  - `url`: The URL of the websocket endpoint (.e.g `ws://localhost:8000/ws`)
    ///
    pub async fn new(tx: mpsc::Sender<Value>, rx: mpsc::Receiver<Value>) -> Result<Self, Error> {
        let streams = Arc::new(RwLock::new(HashMap::new()));
        let requests = Arc::new(RwLock::new(HashMap::new()));

        {
            let streams = Arc::clone(&streams);
            let requests = Arc::clone(&requests);

            tokio::spawn(async move {
                while let Some(message) = rx.next().await {
                    if let Err(e) = Self::handle_message(&streams, &requests, message).await {
                        log::error!("{}", e);
                    }
                }
            });
        }

        Ok(Self {
            next_id: 1,
            sender: ws_tx,
            streams,
            requests,
        })
    }

    async fn handle_message(streams: &Arc<RwLock<StreamsMap>>, requests: &Arc<RwLock<RequestsMap>>, message: Value) -> Result<(), Error> {
        log::debug!("Received message: {:?}", data);

        let message = RequestOrResponse::from_value(message)?;

        match message {
            RequestOrResponse::Request(request) => {
                if request.id.is_some() {
                    log::error!("Received unexpected request, which is not a notification.");
                }
                else {
                    if let Some(params) = request.params {
                        let message: SubscriptionMessage<Value> = serde_json::from_value(params)
                            .expect("Failed to deserialize request parameters");

                        let mut streams = streams.write().await;
                        if let Some(tx) = streams.get_mut(&message.subscription) {
                            tx.send(message).await?;
                        }
                        else {
                            log::error!("Notification for unknown stream ID: {}", message.subscription);
                        }
                    }
                    else {
                        log::error!("No 'params' field in notification.");
                    }
                }
            },
            RequestOrResponse::Response(response) => {
                let mut requests = requests.write().await;

                if let Some(tx) = response.id.as_u64().and_then(|id| requests.remove(&id)) {
                    drop(requests);
                    tx.send(response).ok();
                }
                else {
                    log::error!("Response for unknown request ID: {}", response.id);
                }
            }
        }

        Ok(())
    }
}

#[async_trait]
impl Client for TestClient {
    type Error = Error;

    async fn send_request<P, R>(&mut self, method: &str, params: &P) -> Result<R, Self::Error>
        where
            P: Serialize + Debug + Send + Sync,
            R: for<'de> Deserialize<'de> + Debug + Send + Sync
    {
        let request_id = self.next_id;
        self.next_id += 1;

        let request = Request::build(method.to_owned(), Some(params), Some(&request_id))
            .expect("Failed to serialize JSON-RPC request.");

        self.sender.send(request.to_value()?).await?;

        let (tx, rx) = oneshot::channel();

        {
            let mut requests = self.requests.write().await;
            requests.insert(request_id, tx);
        }

        Ok(rx.await?.into_result()?)
    }

    async fn connect_stream<T>(&mut self, id: SubscriptionId) -> BoxStream<'static, T>
        where
            T: for<'de> Deserialize<'de> + Debug + Send + Sync
    {
        let (tx, rx) = mpsc::channel(16);

        self.streams.write().await.insert(id, tx);

        Box::pin(rx.map(|message: SubscriptionMessage<Value>| {
            serde_json::from_value(message.result)
                .expect("Failed to deserialize notification")
        }))
    }
}