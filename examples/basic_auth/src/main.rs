use std::env;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use nimiq_jsonrpc_client::http::HttpClient;
use nimiq_jsonrpc_client::websocket::WebsocketClient;
use nimiq_jsonrpc_client::Credentials as ClientCredentials;
use nimiq_jsonrpc_server::{Config, Credentials as ServerCredentials, Server};

/// You can pass custom types over JSON-RPC, if they implement Serialize and Deserialize.
#[derive(Debug, Serialize, Deserialize)]
struct HelloWorldData {
    a: u32,
}

/// The trait that defines the RPC interface.
///
/// [`nimiq_jsonrpc_derive::proxy`] will derive an implementation that sends a JSON-RPC request, when a method is
/// called. The default name for that implementation is the trait's name with `Proxy` as suffix. It'll have a
/// constructor `new` that takes a single argument, an implementer of [`nimiq_jsonrpc_client::Client`].
///
#[nimiq_jsonrpc_derive::proxy(name = "HelloWorldProxy")]
#[async_trait]
trait HelloWorld {
    type Error;

    async fn hello(&mut self, name: String, x: HelloWorldData) -> Result<String, Self::Error>;
}

/// Define a service that implements our `HelloWorld` RPC interface.
struct HelloWorldService;

/// Then we implement the service. The macro `#[nimiq_jsonrpc_derive::service]` will implement a
/// `nimiq_jsonrpc_server::Dispatcher` for it, which is needed to interpret a request and call the appropriate method.
///
/// Luckily the macro does all the details for us.
///
#[nimiq_jsonrpc_derive::service]
#[async_trait]
impl HelloWorld for HelloWorldService {
    /// All methods must return a [`Result`], where the error can be converted into a [`nimiq_jsonrpc_core::RpcError`].
    /// You can just use [`nimiq_jsonrpc_core::RpcError`] directly and use one of its constructors, or `()`, which will
    /// always convert to an internal error.
    type Error = ();

    /// Here we implement a method that then can be called from a remote client.
    async fn hello(&mut self, name: String, x: HelloWorldData) -> Result<String, Self::Error> {
        Ok(format!("Hello, {}: x={:?}", name, x))
    }
}

#[tokio::main]
async fn main() {
    dotenvy::dotenv().ok();
    if env::var("RUST_LOG").is_err() {
        env::set_var(
            "RUST_LOG",
            "info,nimiq_jsonrpc_core=debug,nimiq_jsonrpc_server=debug,nimiq_jsonrpc_client=debug",
        );
    }

    pretty_env_logger::init();

    let credentials = ClientCredentials::new("user1", "password1");

    let config = Config {
        bind_to: ([127, 0, 0, 1], 8000).into(),
        basic_auth: Some(ServerCredentials::new(
            &credentials.username,
            &credentials.password.0,
        )),
        enable_websocket: false,
        ..Default::default()
    };

    log::info!("Listening on: {}", config.bind_to);

    let server = Server::new(config, HelloWorldService);
    tokio::spawn(async move {
        server.run().await;
    });

    // Over HTTP POST
    let client = HttpClient::new(
        Default::default(),
        "http://localhost:8000/".parse().unwrap(),
        Some(credentials.clone()),
    );
    let mut proxy = HelloWorldProxy::new(client);
    let retval = proxy
        .hello("World".to_owned(), HelloWorldData { a: 42 })
        .await
        .expect("RPC call failed");
    log::info!("RPC call returned: {}", retval);

    // Over websocket
    let client = WebsocketClient::new("ws://localhost:8000/ws".parse().unwrap(), Some(credentials))
        .await
        .unwrap();
    let mut proxy = HelloWorldProxy::new(client);
    let retval = proxy
        .hello("World".to_owned(), HelloWorldData { a: 42 })
        .await
        .expect("RPC call failed");
    log::info!("RPC call returned: {}", retval);
}
