//! Verifies that the `ip_whitelist` configuration is actually enforced on the request path.

use std::{
    collections::HashSet,
    net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener},
};

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

/// Sends a JSON-RPC request over a fresh connection and returns the HTTP status code.
async fn post_request(bind_to: SocketAddr) -> u16 {
    let body =
        r#"{"jsonrpc":"2.0","method":"hello","params":{"greeting":{"name":"World"}},"id":1}"#;
    let request = format!(
        "POST / HTTP/1.1\r\nHost: {bind_to}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
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

/// Starts a server with the given whitelist and returns the address it listens on.
async fn start_server(ip_whitelist: Option<HashSet<IpAddr>>) -> SocketAddr {
    let bind_to: SocketAddr = ([127, 0, 0, 1], free_port()).into();

    let server = Server::new(
        Config {
            bind_to,
            ip_whitelist,
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

/// Starts a server with the given whitelist and returns the status code of a loopback request.
async fn status_for_whitelist(ip_whitelist: Option<HashSet<IpAddr>>) -> u16 {
    post_request(start_server(ip_whitelist).await).await
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

#[tokio::test]
async fn request_from_whitelisted_ip_is_accepted() {
    let whitelist = HashSet::from([IpAddr::V4(Ipv4Addr::LOCALHOST)]);
    assert_eq!(status_for_whitelist(Some(whitelist)).await, 200);
}

#[tokio::test]
async fn request_from_non_whitelisted_ip_is_rejected() {
    // The documented repro: whitelist a foreign address, then call from loopback
    let whitelist = HashSet::from([IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1))]);
    assert_eq!(status_for_whitelist(Some(whitelist)).await, 403);
}

#[tokio::test]
async fn request_is_accepted_when_no_whitelist_is_configured() {
    assert_eq!(status_for_whitelist(None).await, 200);
}

#[tokio::test]
async fn websocket_from_whitelisted_ip_is_accepted() {
    let whitelist = HashSet::from([IpAddr::V4(Ipv4Addr::LOCALHOST)]);
    let bind_to = start_server(Some(whitelist)).await;
    assert_eq!(websocket_call(bind_to).await, Ok("Hello, World".to_owned()));
}

#[tokio::test]
async fn websocket_from_non_whitelisted_ip_is_rejected() {
    let whitelist = HashSet::from([IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1))]);
    let bind_to = start_server(Some(whitelist)).await;
    assert_eq!(websocket_call(bind_to).await, Err(()));
}

#[tokio::test]
async fn websocket_is_accepted_when_no_whitelist_is_configured() {
    let bind_to = start_server(None).await;
    assert_eq!(websocket_call(bind_to).await, Ok("Hello, World".to_owned()));
}
