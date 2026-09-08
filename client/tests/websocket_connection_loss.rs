#![cfg(feature = "websocket-client")]
//! Regression tests for connection teardown in [`WebsocketClient`].
//!
//! When the websocket connection dies, every subscription stream must end and every in-flight
//! request must resolve with an error. Before this was fixed, both hung forever because the
//! senders lived in maps owned by the client, which the reader task could not drop.

use std::{future::Future, sync::Arc, time::Duration};

use futures::{sink::SinkExt, stream::StreamExt};
use nimiq_jsonrpc_client::{
    websocket::{Config, WebsocketClient},
    Client,
};
use nimiq_jsonrpc_core::SubscriptionId;
use serde_json::{json, Value};
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::{accept_async, tungstenite::Message, WebSocketStream};
use url::Url;

/// Fails the test instead of hanging forever. The assertions below are on the values the client
/// produces, never on this timeout firing.
async fn bounded<F: Future>(f: F) -> F::Output {
    tokio::time::timeout(Duration::from_secs(10), f)
        .await
        .expect("client did not react to the connection being closed")
}

/// Reads the next JSON-RPC message the client sent.
async fn next_request(ws: &mut WebSocketStream<TcpStream>) -> Value {
    loop {
        let message = ws
            .next()
            .await
            .expect("client closed the connection")
            .expect("websocket error");

        match message {
            Message::Text(text) => return serde_json::from_str(&text).unwrap(),
            Message::Binary(bytes) => return serde_json::from_slice(&bytes).unwrap(),
            _ => continue,
        }
    }
}

async fn respond(ws: &mut WebSocketStream<TcpStream>, id: &Value, result: Value) {
    let response = json!({ "jsonrpc": "2.0", "result": result, "id": id });
    ws.send(Message::text(response.to_string())).await.unwrap();
}

async fn notify(ws: &mut WebSocketStream<TcpStream>, subscription: u64, result: Value) {
    let notification = json!({
        "jsonrpc": "2.0",
        "method": "notification",
        "params": { "subscription": subscription, "result": result },
    });
    ws.send(Message::text(notification.to_string()))
        .await
        .unwrap();
}

/// Binds a server on a random port and hands each accepted connection to `handler`.
async fn serve<F, Fut>(handler: F) -> Url
where
    F: FnOnce(WebSocketStream<TcpStream>) -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send,
{
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = Url::parse(&format!("ws://{}/", listener.local_addr().unwrap())).unwrap();

    tokio::spawn(async move {
        let (tcp, _) = listener.accept().await.unwrap();
        handler(accept_async(tcp).await.unwrap()).await;
    });

    url
}

#[tokio::test(flavor = "multi_thread")]
async fn connection_loss_ends_streams_and_fails_pending_requests() {
    let url = serve(|mut ws| async move {
        // The subscribe call, which we answer with the subscription ID.
        let request = next_request(&mut ws).await;
        respond(&mut ws, &request["id"], json!(1)).await;

        // The second call is never answered: it is the request that must be in flight when the
        // connection dies.
        let _ = next_request(&mut ws).await;

        // Prove the subscription is live, then drop the TCP connection without a close handshake.
        notify(&mut ws, 1, json!(42)).await;
        drop(ws);
    })
    .await;

    let client = Arc::new(WebsocketClient::with_url(url).await.unwrap());

    let subscription: u64 = client.send_request("subscribe", &()).await.unwrap();
    let mut stream = client
        .connect_stream::<u64>(SubscriptionId::Number(subscription))
        .await;

    let pending = tokio::spawn({
        let client = Arc::clone(&client);
        async move { client.send_request::<_, u64>("never_answered", &()).await }
    });

    assert_eq!(bounded(stream.next()).await, Some(42));

    // The connection is gone: the stream must end and the in-flight request must fail.
    assert_eq!(bounded(stream.next()).await, None);
    assert!(bounded(pending).await.unwrap().is_err());
}

#[tokio::test(flavor = "multi_thread")]
async fn explicit_close_ends_streams_and_fails_pending_requests() {
    let url = serve(|mut ws| async move {
        let request = next_request(&mut ws).await;
        respond(&mut ws, &request["id"], json!(1)).await;

        // Never answer anything else, and never reply to the close frame either.
        std::future::pending::<()>().await;
    })
    .await;

    let client = Arc::new(WebsocketClient::with_url(url).await.unwrap());

    let subscription: u64 = client.send_request("subscribe", &()).await.unwrap();
    let mut stream = client
        .connect_stream::<u64>(SubscriptionId::Number(subscription))
        .await;

    let pending = tokio::spawn({
        let client = Arc::clone(&client);
        async move { client.send_request::<_, u64>("never_answered", &()).await }
    });

    client.close().await;

    assert_eq!(bounded(stream.next()).await, None);
    assert!(bounded(pending).await.unwrap().is_err());

    // A closed client must fail fast rather than register into a dead connection.
    assert!(bounded(client.send_request::<_, u64>("after_close", &()))
        .await
        .is_err());
    let mut after_close = client
        .connect_stream::<u64>(SubscriptionId::Number(2))
        .await;
    assert_eq!(bounded(after_close.next()).await, None);
}

#[tokio::test(flavor = "multi_thread")]
async fn connection_loss_is_observable_without_a_pending_request() {
    let url = serve(|mut ws| async move {
        let request = next_request(&mut ws).await;
        respond(&mut ws, &request["id"], json!(1)).await;

        // Drop the TCP connection without a close handshake.
        drop(ws);
    })
    .await;

    let client = WebsocketClient::with_url(url).await.unwrap();
    assert!(!client.is_closed());

    let _: u64 = client.send_request("subscribe", &()).await.unwrap();

    bounded(client.closed()).await;
    assert!(client.is_closed());
}

/// A connection that is silently blackholed - no close frame, no reset, nothing to observe - can
/// only be detected by noticing that nothing comes back anymore.
#[tokio::test(flavor = "multi_thread")]
async fn blackholed_connection_is_detected_by_the_heartbeat() {
    let url = serve(|mut ws| async move {
        let request = next_request(&mut ws).await;
        respond(&mut ws, &request["id"], json!(1)).await;

        // Stop reading and writing, but hold the connection open: pings are never answered and
        // the client never sees an error or an EOF.
        std::future::pending::<()>().await;
    })
    .await;

    let client = Arc::new(
        WebsocketClient::new_with_config(
            url,
            None,
            Config {
                ping_interval: Some(Duration::from_millis(50)),
                ping_timeout: Duration::from_millis(300),
            },
        )
        .await
        .unwrap(),
    );

    let subscription: u64 = client.send_request("subscribe", &()).await.unwrap();
    let mut stream = client
        .connect_stream::<u64>(SubscriptionId::Number(subscription))
        .await;

    let pending = tokio::spawn({
        let client = Arc::clone(&client);
        async move { client.send_request::<_, u64>("never_answered", &()).await }
    });

    assert_eq!(bounded(stream.next()).await, None);
    assert!(bounded(pending).await.unwrap().is_err());
    assert!(client.is_closed());
}

/// The heartbeat must not kill a connection that is merely idle. The server answers pings with
/// pongs, which is enough to keep it alive.
#[tokio::test(flavor = "multi_thread")]
async fn heartbeat_keeps_an_idle_connection_alive() {
    let url = serve(|mut ws| async move {
        // Polling the stream is what makes tungstenite answer pings automatically.
        loop {
            let request = next_request(&mut ws).await;
            respond(&mut ws, &request["id"], json!("alive")).await;
        }
    })
    .await;

    let client = WebsocketClient::new_with_config(
        url,
        None,
        Config {
            ping_interval: Some(Duration::from_millis(50)),
            ping_timeout: Duration::from_millis(300),
        },
    )
    .await
    .unwrap();

    // Stay completely idle for many ping intervals, well past the timeout.
    tokio::time::sleep(Duration::from_millis(1500)).await;

    assert!(!client.is_closed());

    let reply: String = client.send_request("still_alive", &()).await.unwrap();
    assert_eq!(reply, "alive");
}
