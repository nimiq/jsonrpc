use std::{
    net::SocketAddr,
    sync::Arc,
    future::Future,
};

use futures::{
    stream::{StreamExt, FuturesUnordered},
    sink::SinkExt,
};
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

use nimiq_jsonrpc_core::{SingleOrBatch, Request, Response, Error as JsonRpcError};


#[derive(Debug, Error)]
pub enum Error {
    #[error("HTTP error: {0}")]
    Warp(#[from] warp::Error),
    #[error("Queue error: {0}")]
    Mpsc(#[from] tokio::sync::mpsc::error::SendError<warp::ws::Message>),
}


#[derive(Clone, Debug)]
pub struct Config {
    pub bind_to: SocketAddr,

    // TODO: cors header, allowed methods
}

impl Default for Config {
    fn default() -> Self {
        Self {
            bind_to: ([127, 0, 0, 1], 8000).into()
        }
    }
}


struct Inner<D: Dispatcher> {
    config: Config,
    //subscriptions: RwLock<HashMap<Subscription, broadcast::Sender>>,
    dispatcher: RwLock<D>,
}


pub struct Server<D: Dispatcher> {
    inner: Arc<Inner<D>>,
}


impl<D: Dispatcher> Server<D> {
    pub fn new(config: Config, dispatcher: D) -> Self {
        Self {
            inner: Arc::new(Inner {
                config,
                dispatcher: RwLock::new(dispatcher),
                //subscriptions: RwLock::new(HashMap::new()),
            })
        }
    }

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

    fn upgrade_to_ws(inner: Arc<Inner<D>>, ws: warp::ws::Ws) -> impl warp::Reply {
        ws.on_upgrade(move |websocket| {
            let (mut tx, mut rx) = websocket.split();

            let (multiplex_tx, mut multiplex_rx) = mpsc::channel(16); // TODO: What size?

            // Forwards multiplexer queue output to websocket
            let forward_fut = async move {
                while let Some(message) = multiplex_rx.recv().await {
                    tx.send(message).await?;
                }
                Ok::<(), Error>(())
            };

            // Handles requests received from websocket
            let handle_fut = {
                let mut tx = multiplex_tx.clone();
                async move {
                    while let Some(message) = rx.next().await.transpose()? {
                        if let Some(response) = Self::handle_raw_request(Arc::clone(&inner), message.as_bytes(), Some(tx.clone())).await {
                            tx.send(warp::ws::Message::binary(response)).await?;
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

    async fn handle_raw_request(inner: Arc<Inner<D>>, request: &[u8], tx: Option<mpsc::Sender<warp::ws::Message>>) -> Option<Vec<u8>> {
        match serde_json::from_slice(request) {
            Ok(request) => Self::handle_request(inner, request, tx).await,
            Err(_e) => {
                Some(SingleOrBatch::Single(Response::new_error(Value::Null, JsonRpcError::invalid_request())))
            }
        }.map(|response| {
            serde_json::to_vec(&response)
                .expect("Failed to serialize JSON RPC response")
        })
    }

    async fn handle_request(inner: Arc<Inner<D>>, request: SingleOrBatch<Request>, tx: Option<mpsc::Sender<warp::ws::Message>>) -> Option<SingleOrBatch<Response>> {
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

    async fn handle_single_request(inner: Arc<Inner<D>>, request: Request, _tx: Option<mpsc::Sender<warp::ws::Message>>) -> Option<Response> {
        // TODO: Handle subscriptions
        let mut dispatcher = inner.dispatcher.write().await;
        dispatcher.dispatch(request).await
    }
}


#[async_trait]
pub trait Dispatcher: Send + Sync + 'static {
    async fn dispatch(&mut self, request: Request) -> Option<Response>;
}



pub async fn dispatch_method_with_args<P, R, E, F, Fut>(request: Request, f: F) -> Option<Response>
    where P: for<'de> Deserialize<'de> + Send,
          R: Serialize,
          JsonRpcError: From<E>,
          F: FnOnce(P) -> Fut + Send,
          Fut: Future<Output=Result<R, E>> + Send
{
    let params = match request.params {
        Some(params) => params,
        None => return error_response(request.id, JsonRpcError::invalid_params),
    };

    let params = match serde_json::from_value(params) {
        Ok(params) => params,
        Err(_e) => return error_response(request.id, JsonRpcError::invalid_params),
    };

    let result = f(params).await;

    response(request.id, result)
}

pub async fn dispatch_method_without_args<R, E, F, Fut>(request: Request, f: F) -> Option<Response>
    where R: Serialize,
          JsonRpcError: From<E>,
          F: FnOnce() -> Fut + Send,
          Fut: Future<Output=Result<R, E>> + Send
{
    let result = f().await;

    match request.params {
        Some(Value::Null) | None => {},
        _ => return error_response(request.id, JsonRpcError::invalid_params),
    }

    response(request.id, result)
}

fn response<R, E>(id_opt: Option<Value>, result: Result<R, E>) -> Option<Response>
    where R: Serialize,
          JsonRpcError: From<E>,
{
    let response = match (id_opt, result) {
        (Some(id), Ok(retval)) => {
            let retval = serde_json::to_value(retval).expect("Failed to serialize return value");
            Some(Response::new_success(id, retval))
        },
        (Some(id), Err(e)) => {
            Some(Response::new_error(id, JsonRpcError::from(e)))
        },
        (None, _) => None,
    };

    log::debug!("Sending response: {:?}", response);

    response
}

pub fn error_response<E: FnOnce() -> JsonRpcError>(id_opt: Option<Value>, e: E) -> Option<Response> {
    if let Some(id) = id_opt {
        Some(Response::new_error(id, e()))
    }
    else {
        None
    }
}
