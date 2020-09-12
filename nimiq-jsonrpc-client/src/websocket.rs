use std::{
    collections::HashMap,
    sync::Arc,
    fmt::Debug
};

use tokio_tungstenite::{connect_async, WebSocketStream};
use tokio::{
    sync::{mpsc, oneshot, RwLock},
    net::TcpStream,
};
use thiserror::Error;
use url::Url;
use serde::{Serialize, Deserialize};
use futures::{
    stream::{BoxStream, StreamExt, SplitSink},
    sink::SinkExt,
};
use serde_json::Value;
use tungstenite::Message;
use async_trait::async_trait;

use nimiq_jsonrpc_core::{Request, Response, RequestOrResponse, SubscriptionMessage, SubscriptionId};

use crate::Client;


#[derive(Debug, Error)]
pub enum Error {
    #[error("Websocket protocol error: {0}")]
    Websocket(#[from] tungstenite::Error),

    #[error("JSON-RPC protocl error: {0}")]
    JsonRpc(#[from] nimiq_jsonrpc_core::Error),

    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("{0}")]
    OneshotRecv(#[from] oneshot::error::RecvError),

    #[error("{0}")]
    MpscSend(#[from] mpsc::error::SendError<SubscriptionMessage<Value>>),
}


type StreamsMap = HashMap<SubscriptionId, mpsc::Sender<SubscriptionMessage<Value>>>;
type RequestsMap = HashMap<u64, oneshot::Sender<Response>>;

pub struct WebsocketClient {
    streams: Arc<RwLock<StreamsMap>>,
    requests: Arc<RwLock<RequestsMap>>,
    sender: SplitSink<WebSocketStream<TcpStream>, Message>,
    next_id: u64,
}


impl WebsocketClient {
    pub async fn new(url: Url) -> Result<Self, Error> {
        let (ws_stream, _) = connect_async(url).await?;

        let (ws_tx, mut ws_rx) = ws_stream.split();

        let streams = Arc::new(RwLock::new(HashMap::new()));
        let requests = Arc::new(RwLock::new(HashMap::new()));

        {
            let streams = Arc::clone(&streams);
            let requests = Arc::clone(&requests);

            tokio::spawn(async move {
                while let Some(message_result) = ws_rx.next().await {
                    match message_result {
                        Ok(message) => {
                            if let Err(e) = Self::handle_websocket_message(&streams, &requests, message).await {
                                log::error!("{}", e);
                            }
                        }
                        Err(e) => {
                            log::error!("{}", e);
                        }
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

    async fn handle_websocket_message(streams: &Arc<RwLock<StreamsMap>>, requests: &Arc<RwLock<RequestsMap>>, message: Message) -> Result<(), Error> {
        let data = message.into_text()?;

        log::debug!("Received message: {:?}", data);

        let message = RequestOrResponse::from_str(&data)?;

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
impl Client for WebsocketClient {
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

        self.sender.send(Message::Binary(serde_json::to_vec(&request)?)).await?;

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