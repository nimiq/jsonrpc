//! Verifies that `enable_websocket` actually controls whether the `/ws` endpoint is mounted.

use std::net::{SocketAddr, TcpListener};

use async_trait::async_trait;
use nimiq_jsonrpc_client::websocket::WebsocketClient;
use nimiq_jsonrpc_server::{Config, Server};
use serde::{Deserialize, Serialize};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
};

#[derive(Debug, Serialize, Deserialize)]
struct Greeting {
    name: String,
}

#[nimiq_jsonrpc_derive::proxy(name = "HelloWorldProxy")]
#[async_trait]
trait HelloWorld {
    type Error;

    async fn hello(&self, greeting: Greeting) -> Result<String, Self::Error>;
}

struct HelloWorldService;

#[nimiq_jsonrpc_derive::service]
#[async_trait]
impl HelloWorld for HelloWorldService {
    type Error = ();

    async fn hello(&self, greeting: Greeting) -> Result<String, Self::Error> {
        Ok(format!("Hello, {}", greeting.name))
    }
}

/// Reserves a free port on the loopback interface for the server to bind to.
fn free_port() -> u16 {
    TcpListener::bind(("127.0.0.1", 0))
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// Starts a server with the given websocket setting and returns the address it listens on.
async fn start_server(enable_websocket: bool) -> SocketAddr {
    let bind_to: SocketAddr = ([127, 0, 0, 1], free_port()).into();

    let server = Server::new(
        Config {
            bind_to,
            enable_websocket,
            ..Default::default()
        },
        HelloWorldService,
    );
    tokio::spawn(async move { server.run().await });

    // Give the server a moment to bind before the first connection attempt
    for _ in 0..100 {
        if TcpStream::connect(bind_to).await.is_ok() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }

    bind_to
}

/// Sends a raw request over a fresh connection and returns the HTTP status code.
async fn request_status(bind_to: SocketAddr, head: &str, body: &str) -> u16 {
    let request = format!(
        "{head} HTTP/1.1\r\nHost: {bind_to}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );

    let mut stream = TcpStream::connect(bind_to).await.unwrap();
    stream.write_all(request.as_bytes()).await.unwrap();

    let mut response = String::new();
    stream.read_to_string(&mut response).await.unwrap();

    response
        .split_whitespace()
        .nth(1)
        .expect("no status code in response")
        .parse()
        .expect("status code is not a number")
}

/// Performs a JSON-RPC call over a websocket connection.
async fn websocket_call(bind_to: SocketAddr) -> Result<String, ()> {
    let client = WebsocketClient::new(format!("ws://{bind_to}/ws").parse().unwrap(), None)
        .await
        .map_err(|_| ())?;

    HelloWorldProxy::new(client)
        .hello(Greeting {
            name: "World".to_owned(),
        })
        .await
        .map_err(|_| ())
}

const RPC_BODY: &str =
    r#"{"jsonrpc":"2.0","method":"hello","params":{"greeting":{"name":"World"}},"id":1}"#;

#[tokio::test]
async fn websocket_endpoint_is_available_when_enabled() {
    let bind_to = start_server(true).await;
    assert_eq!(websocket_call(bind_to).await, Ok("Hello, World".to_owned()));
}

#[tokio::test]
async fn websocket_endpoint_is_absent_when_disabled() {
    let bind_to = start_server(false).await;

    assert_eq!(request_status(bind_to, "GET /ws", "").await, 404);
    assert_eq!(websocket_call(bind_to).await, Err(()));
}

#[tokio::test]
async fn http_endpoint_still_works_when_websocket_is_disabled() {
    let bind_to = start_server(false).await;
    assert_eq!(request_status(bind_to, "POST /", RPC_BODY).await, 200);
}
