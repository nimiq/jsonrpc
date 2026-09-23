//! Verifies that the origins configured in `Config::cors` are enforced on the request path, for
//! both the HTTP endpoint and the websocket endpoint.

use std::{
    collections::HashSet,
    net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener},
    time::Duration,
};

use async_trait::async_trait;
use futures::{SinkExt, StreamExt};
use nimiq_jsonrpc_client::websocket::WebsocketClient;
use nimiq_jsonrpc_server::{Config, Cors, Credentials, Server};
use serde::{Deserialize, Serialize};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
};
use tokio_tungstenite::{
    connect_async,
    tungstenite::{client::IntoClientRequest, http::header::ORIGIN, Message},
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

const ALLOWED_ORIGIN: &str = "https://allowed.example";
const OTHER_ORIGIN: &str = "https://other.example";

/// `user:pass`, base64-encoded for HTTP basic auth
const VALID_AUTH: &str = "Authorization: Basic dXNlcjpwYXNz\r\n";

const RPC_BODY: &str =
    r#"{"jsonrpc":"2.0","method":"hello","params":{"greeting":{"name":"World"}},"id":1}"#;

/// Reserves a free port on the loopback interface for the server to bind to.
fn free_port() -> u16 {
    TcpListener::bind(("127.0.0.1", 0))
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// Starts a server with the given configuration on a free port and returns the address.
async fn start_server(config: Config) -> SocketAddr {
    let bind_to: SocketAddr = ([127, 0, 0, 1], free_port()).into();

    let server = Server::new(Config { bind_to, ..config }, HelloWorldService);
    tokio::spawn(async move { server.run().await });

    // Give the server a moment to bind before the first connection attempt
    for _ in 0..100 {
        if TcpStream::connect(bind_to).await.is_ok() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    bind_to
}

fn config(cors: Option<Cors>) -> Config {
    Config {
        cors,
        ..Default::default()
    }
}

fn allowlist() -> Option<Cors> {
    Some(Cors::new().with_origins([ALLOWED_ORIGIN]).unwrap())
}

/// Sends a raw request over a fresh connection and returns the status code and the response,
/// lowercased.
///
/// The whole response is read when `until_closed` is set, which requires the request to carry
/// `Connection: close`. Otherwise only the response head is read, which is what a websocket
/// upgrade needs, since the connection stays open after the `101 Switching Protocols` response.
async fn send(bind_to: SocketAddr, request: String, until_closed: bool) -> (u16, String) {
    let mut stream = TcpStream::connect(bind_to).await.unwrap();
    stream.write_all(request.as_bytes()).await.unwrap();

    let mut response = Vec::new();
    let mut chunk = [0u8; 1024];
    let read = async {
        loop {
            let n = stream.read(&mut chunk).await.unwrap();
            if n == 0 {
                break;
            }
            response.extend_from_slice(&chunk[..n]);
            if !until_closed && response.windows(4).any(|window| window == b"\r\n\r\n") {
                break;
            }
        }
    };
    tokio::time::timeout(Duration::from_secs(5), read)
        .await
        .expect("timed out waiting for the response");

    let response = String::from_utf8_lossy(&response).to_lowercase();
    let status = response
        .split_whitespace()
        .nth(1)
        .expect("no status code in response")
        .parse()
        .expect("status code is not a number");

    (status, response)
}

/// Formats an `Origin` header line, or nothing when no origin is given.
fn origin_header(origin: Option<&str>) -> String {
    origin
        .map(|origin| format!("Origin: {origin}\r\n"))
        .unwrap_or_default()
}

/// Sends a JSON-RPC request over HTTP with the given `Origin` and extra headers and returns the
/// status code and the full response.
///
/// No `Content-Type` is set, which is what a browser does for a cross-origin POST that needs no
/// preflight. That is exactly the request the origin check has to catch.
async fn post(bind_to: SocketAddr, origin: Option<&str>, extra_headers: &str) -> (u16, String) {
    let request = format!(
        "POST / HTTP/1.1\r\nHost: {bind_to}\r\n{}{extra_headers}Content-Length: {}\r\nConnection: close\r\n\r\n{RPC_BODY}",
        origin_header(origin),
        RPC_BODY.len()
    );
    send(bind_to, request, true).await
}

async fn post_status(bind_to: SocketAddr, origin: Option<&str>) -> u16 {
    post(bind_to, origin, "").await.0
}

/// Sends a CORS preflight for a JSON-RPC POST and returns the status code and the full response.
async fn preflight(bind_to: SocketAddr, origin: &str) -> (u16, String) {
    let request = format!(
        "OPTIONS / HTTP/1.1\r\nHost: {bind_to}\r\nOrigin: {origin}\r\nAccess-Control-Request-Method: POST\r\nAccess-Control-Request-Headers: content-type\r\nConnection: close\r\n\r\n"
    );
    send(bind_to, request, true).await
}

/// Attempts a websocket handshake with the given `Origin` header and returns the status code and
/// response head.
async fn upgrade(bind_to: SocketAddr, origin: Option<&str>) -> (u16, String) {
    let request = format!(
        "GET /ws HTTP/1.1\r\nHost: {bind_to}\r\n{}Connection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\r\n",
        origin_header(origin)
    );
    send(bind_to, request, false).await
}

async fn upgrade_status(bind_to: SocketAddr, origin: Option<&str>) -> u16 {
    upgrade(bind_to, origin).await.0
}

/// Performs a JSON-RPC call over a websocket connection using the regular client, which sends no
/// `Origin` header.
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

/// Performs a JSON-RPC call over a websocket connection opened with the given `Origin` header, the
/// way a browser would, and returns the raw response.
async fn websocket_call_from_origin(bind_to: SocketAddr, origin: &str) -> Result<String, ()> {
    let mut request = format!("ws://{bind_to}/ws").into_client_request().unwrap();
    request
        .headers_mut()
        .insert(ORIGIN, origin.parse().unwrap());

    let (mut websocket, _) = connect_async(request).await.map_err(|_| ())?;
    websocket
        .send(Message::Text(RPC_BODY.into()))
        .await
        .map_err(|_| ())?;
    let response = websocket.next().await.ok_or(())?.map_err(|_| ())?;

    Ok(response.into_text().map_err(|_| ())?.to_string())
}

#[tokio::test]
async fn http_request_without_origin_is_accepted_by_default() {
    let bind_to = start_server(config(None)).await;
    assert_eq!(post_status(bind_to, None).await, 200);
}

#[tokio::test]
async fn http_request_with_origin_is_rejected_by_default() {
    // The default configuration allows no browser origin, so a web page cannot reach the server
    let bind_to = start_server(config(None)).await;
    assert_eq!(post_status(bind_to, Some(OTHER_ORIGIN)).await, 403);
}

#[tokio::test]
async fn http_request_with_origin_is_rejected_with_empty_allowlist() {
    // An explicitly empty list, which is what the Nimiq node passes by default
    let cors = Cors::new().with_origins(Vec::<String>::new()).unwrap();
    let bind_to = start_server(config(Some(cors))).await;
    assert_eq!(post_status(bind_to, Some(OTHER_ORIGIN)).await, 403);
}

#[tokio::test]
async fn http_request_from_allowed_origin_is_accepted() {
    let bind_to = start_server(config(allowlist())).await;

    let (status, head) = post(bind_to, Some(ALLOWED_ORIGIN), "").await;
    assert_eq!(status, 200);
    // The CORS headers must agree with the request path, or the browser would still block the page
    assert!(head.contains(&format!("access-control-allow-origin: {ALLOWED_ORIGIN}")));
}

#[tokio::test]
async fn http_request_from_other_origin_is_rejected() {
    let bind_to = start_server(config(allowlist())).await;

    let (status, response) = post(bind_to, Some(OTHER_ORIGIN), "").await;
    assert_eq!(status, 403);
    assert!(!response.contains("access-control-allow-origin"));
    // The body tells an operator which check rejected the request
    assert!(response.ends_with("origin not allowed"));
}

#[tokio::test]
async fn http_request_from_opaque_origin_is_rejected() {
    // Sandboxed frames and `file://` pages send the literal `null` origin
    let bind_to = start_server(config(allowlist())).await;
    assert_eq!(post_status(bind_to, Some("null")).await, 403);
}

#[tokio::test]
async fn http_request_without_origin_is_accepted_with_allowlist() {
    let bind_to = start_server(config(allowlist())).await;
    assert_eq!(post_status(bind_to, None).await, 200);
}

#[tokio::test]
async fn http_request_from_any_origin_is_accepted_with_wildcard() {
    let bind_to = start_server(config(Some(Cors::new().with_any_origin()))).await;
    assert_eq!(post_status(bind_to, Some(OTHER_ORIGIN)).await, 200);
}

#[tokio::test]
async fn request_origin_must_match_exactly() {
    // Browsers always send the canonical form, so the request side is not normalized
    let bind_to = start_server(config(allowlist())).await;

    for origin in [
        "https://allowed.example/",
        "https://ALLOWED.example",
        "https://allowed.example:443",
        "http://allowed.example",
        "https://allowed.example.evil",
    ] {
        assert_eq!(post_status(bind_to, Some(origin)).await, 403, "{origin}");
    }
}

#[tokio::test]
async fn later_origin_configuration_overrides_earlier() {
    let restricted = Cors::new()
        .with_any_origin()
        .with_origins([ALLOWED_ORIGIN])
        .unwrap();
    let bind_to = start_server(config(Some(restricted))).await;
    assert_eq!(post_status(bind_to, Some(ALLOWED_ORIGIN)).await, 200);
    assert_eq!(post_status(bind_to, Some(OTHER_ORIGIN)).await, 403);

    let opened = Cors::new()
        .with_origins([ALLOWED_ORIGIN])
        .unwrap()
        .with_any_origin();
    let bind_to = start_server(config(Some(opened))).await;
    assert_eq!(post_status(bind_to, Some(OTHER_ORIGIN)).await, 200);

    let replaced = Cors::new()
        .with_origins([OTHER_ORIGIN])
        .unwrap()
        .with_origins([ALLOWED_ORIGIN])
        .unwrap();
    let bind_to = start_server(config(Some(replaced))).await;
    assert_eq!(post_status(bind_to, Some(OTHER_ORIGIN)).await, 403);
}

#[test]
fn opaque_origin_cannot_be_allowed() {
    let error = Cors::new().with_origins(["null"]).unwrap_err();
    assert_eq!(error.entry(), "null");
    assert!(error.to_string().contains("opaque"), "{error}");
}

#[test]
fn malformed_origins_are_rejected() {
    for entry in [
        "allowed.example",
        "https://",
        "://allowed.example",
        "https://allowed.example/rpc",
        "https://allowed.example?x=1",
        "https://user@allowed.example",
        "https://allowed .example",
        "https://allowed.example:port",
        "https://allowed.example:99999",
        "https://a:b:c",
        "https://[::1",
        "https://[::1]x",
        "https://[garbage]",
        "https://[fe80::1%eth0]",
        "https://exämple.com",
        "https://*.allowed.example",
        "https://ex%61mple.com",
        "https://allowed.example\\",
        "https://allowed.example:+443",
        "1https://allowed.example",
        "ws://allowed.example",
    ] {
        let error = Cors::new().with_origins([entry]).unwrap_err();
        assert_eq!(error.entry(), entry);
    }
}

#[tokio::test]
async fn configured_origins_are_canonicalized() {
    // Operators type origins in all sorts of ways; browsers send exactly one form
    let cors = Cors::new()
        .with_origins([" HTTPS://Allowed.Example:443/ "])
        .unwrap();
    let bind_to = start_server(config(Some(cors))).await;
    assert_eq!(post_status(bind_to, Some(ALLOWED_ORIGIN)).await, 200);

    // A port that is not the scheme's default is significant
    let cors = Cors::new().with_origins(["http://Localhost:8080"]).unwrap();
    let bind_to = start_server(config(Some(cors))).await;
    assert_eq!(
        post_status(bind_to, Some("http://localhost:8080")).await,
        200
    );
    assert_eq!(post_status(bind_to, Some("http://localhost")).await, 403);

    // An IPv6 literal keeps its brackets and its port, and is compressed the way browsers do it
    let cors = Cors::new()
        .with_origins(["http://[0:0:0:0:0:0:0:1]:8080"])
        .unwrap();
    let bind_to = start_server(config(Some(cors))).await;
    assert_eq!(post_status(bind_to, Some("http://[::1]:8080")).await, 200);

    // Browsers write an IPv4-mapped address in hex groups
    let cors = Cors::new()
        .with_origins(["http://[::FFFF:127.0.0.1]"])
        .unwrap();
    let bind_to = start_server(config(Some(cors))).await;
    assert_eq!(
        post_status(bind_to, Some("http://[::ffff:7f00:1]")).await,
        200
    );

    // Extension pages have their own schemes without a default port
    let cors = Cors::new()
        .with_origins(["chrome-extension://abc"])
        .unwrap();
    let bind_to = start_server(config(Some(cors))).await;
    assert_eq!(
        post_status(bind_to, Some("chrome-extension://abc")).await,
        200
    );
}

#[tokio::test]
async fn wildcard_entry_allows_any_origin() {
    let cors = Cors::new().with_origins([ALLOWED_ORIGIN, " * "]).unwrap();
    let bind_to = start_server(config(Some(cors))).await;
    assert_eq!(post_status(bind_to, Some(OTHER_ORIGIN)).await, 200);

    // Entries next to the wildcard are still validated
    assert!(Cors::new().with_origins(["*", "garbage"]).is_err());
}

#[tokio::test]
async fn preflight_reflects_the_allowlist() {
    let bind_to = start_server(config(allowlist())).await;

    let (status, head) = preflight(bind_to, ALLOWED_ORIGIN).await;
    assert_eq!(status, 200);
    assert!(head.contains(&format!("access-control-allow-origin: {ALLOWED_ORIGIN}")));

    let (status, head) = preflight(bind_to, OTHER_ORIGIN).await;
    assert_eq!(status, 200);
    assert!(!head.contains("access-control-allow-origin"));
}

#[tokio::test]
async fn allowed_origin_still_requires_basic_auth() {
    let bind_to = start_server(Config {
        basic_auth: Some(Credentials::new("user", "pass")),
        ..config(allowlist())
    })
    .await;

    assert_eq!(post(bind_to, Some(ALLOWED_ORIGIN), "").await.0, 401);
    assert_eq!(post(bind_to, Some(ALLOWED_ORIGIN), VALID_AUTH).await.0, 200);
}

#[tokio::test]
async fn other_origin_is_rejected_regardless_of_basic_auth() {
    let bind_to = start_server(Config {
        basic_auth: Some(Credentials::new("user", "pass")),
        ..config(allowlist())
    })
    .await;

    // Valid credentials from a foreign page are ambient authority, not proof of intent
    assert_eq!(post(bind_to, Some(OTHER_ORIGIN), VALID_AUTH).await.0, 403);
    // The origin check runs before basic auth, so a foreign page learns nothing about it
    assert_eq!(post(bind_to, Some(OTHER_ORIGIN), "").await.0, 403);
}

#[tokio::test]
async fn allowed_origin_does_not_bypass_ip_whitelist() {
    let bind_to = start_server(Config {
        ip_whitelist: Some(HashSet::from([IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1))])),
        ..config(allowlist())
    })
    .await;

    assert_eq!(post_status(bind_to, Some(ALLOWED_ORIGIN)).await, 403);
}

#[tokio::test]
async fn websocket_without_origin_is_accepted_by_default() {
    let bind_to = start_server(config(None)).await;
    assert_eq!(upgrade_status(bind_to, None).await, 101);
    assert_eq!(websocket_call(bind_to).await, Ok("Hello, World".to_owned()));
}

#[tokio::test]
async fn websocket_with_origin_is_rejected_by_default() {
    let bind_to = start_server(config(None)).await;
    assert_eq!(upgrade_status(bind_to, Some(OTHER_ORIGIN)).await, 403);
}

#[tokio::test]
async fn websocket_from_allowed_origin_is_accepted() {
    let bind_to = start_server(config(allowlist())).await;

    let (status, head) = upgrade(bind_to, Some(ALLOWED_ORIGIN)).await;
    assert_eq!(status, 101);
    assert!(head.contains("sec-websocket-accept"));

    let response = websocket_call_from_origin(bind_to, ALLOWED_ORIGIN).await;
    assert!(response.unwrap().contains("Hello, World"));
}

#[tokio::test]
async fn websocket_from_other_origin_is_rejected() {
    let bind_to = start_server(config(allowlist())).await;

    let (status, head) = upgrade(bind_to, Some(OTHER_ORIGIN)).await;
    assert_eq!(status, 403);
    assert!(!head.contains("sec-websocket-accept"));

    assert_eq!(
        websocket_call_from_origin(bind_to, OTHER_ORIGIN).await,
        Err(())
    );
}

#[tokio::test]
async fn websocket_without_origin_is_accepted_with_allowlist() {
    let bind_to = start_server(config(allowlist())).await;
    assert_eq!(upgrade_status(bind_to, None).await, 101);
    assert_eq!(websocket_call(bind_to).await, Ok("Hello, World".to_owned()));
}

#[tokio::test]
async fn websocket_from_any_origin_is_accepted_with_wildcard() {
    let bind_to = start_server(config(Some(Cors::new().with_any_origin()))).await;
    assert_eq!(upgrade_status(bind_to, Some(OTHER_ORIGIN)).await, 101);
}
