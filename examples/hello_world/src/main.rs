use std::env;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use nimiq_jsonrpc_client::http::HttpClient;
use nimiq_jsonrpc_server::{Config, Server};

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

    async fn hello(&self, name: String, x: HelloWorldData) -> Result<String, Self::Error>;
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
    async fn hello(&self, name: String, x: HelloWorldData) -> Result<String, Self::Error> {
        Ok(format!("Hello, {}: x={:?}", name, x))
    }
}

#[tokio::main]
async fn main() {
    // Load environment variables from `.env` file. You can set the RUST_LOG there, if you like.
    dotenvy::dotenv().ok();

    // Default to displaying our debug messages, and only info messages otherwise.
    if env::var("RUST_LOG").is_err() {
        env::set_var(
            "RUST_LOG",
            "info,nimiq_jsonrpc_core=debug,nimiq_jsonrpc_server=debug,nimiq_jsonrpc_client=debug",
        );
    }

    pretty_env_logger::init();

    let config = Config {
        bind_to: ([127, 0, 0, 1], 8000).into(),
        enable_websocket: false, // JSON-RPC over websocket is enabled by default, but we don't need it in this example.
        ..Default::default()
    };

    log::info!("Listening on: {}", config.bind_to);

    // Start our `FoobarService` as a JSON-RPC server
    let server = Server::new(config, HelloWorldService);
    tokio::spawn(async move {
        server.run().await;
    });

    // The server is running now, so we can connect to it. The `HttpClient` will send all requests as a HTTP POST to
    // the specified URL.
    let client = HttpClient::with_url("http://localhost:8000/".parse().unwrap());

    // Next we can use the proxy that we generated earlier and construct it with the client.
    let proxy = HelloWorldProxy::new(client);

    // The proxy implements our `HelloWorld` RPC interface and will send a request to the server, when a method is
    // called.
    let retval = proxy
        .hello("World".to_owned(), HelloWorldData { a: 42 })
        .await
        .expect("RPC call failed");
    log::info!("RPC call returned: {}", retval);
}
