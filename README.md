# Nimiq JSON-RPC

A Rust implementation of the [JSON-RPC 2.0 specification](https://www.jsonrpc.org/specification#notification).

 - `nimiq-jsonrpc-core` implements the data structures (using [serde](https://crates.io/crates/serde)) for JSON-RPC.
 - `nimiq-jsonrpc-client` is a client implementation for a HTTP (using [reqwest](https://crates.io/crates/reqwest)) and websocket client (using [tokio-tungstenite](https://crates.io/crates/tokio-tungstenite)).
 - `nimiq-jsonrpc-server` is a server implementation for HTTP and websocket (using [warp](https://crates.io/crates/warp)).

# Example

```rust
use async_trait::async_trait;
use serde::{Serialize, Deserialize};

use nimiq_jsonrpc_server::{Server, Config};
use nimiq_jsonrpc_client::http::HttpClient;


#[nimiq_jsonrpc_derive::proxy]
#[async_trait]
trait Foobar {
    async fn hello(&mut self, name: String) -> String;
}

struct FoobarService;

#[nimiq_jsonrpc_derive::service]
#[async_trait]
impl Foobar for FoobarService {
    async fn hello(&mut self, name: String) -> String {
        println!("Hello, {}", name);
        format!("Hello, {}", name)
    }
}


#[tokio::main]
async fn main() {
    dotenvy::dotenv().ok();
    pretty_env_logger::init();

    let config = Config::default();

    log::info!("Listening on: {}", config.bind_to);

    let server = Server::new(config, FoobarService);
    tokio::spawn(async move {
        server.run().await;
    });

    let client = HttpClient::new("http://localhost:8000/");
    let mut proxy = FoobarProxy::new(client);

    let retval = proxy.hello("World".to_owned()).await;
    log::info!("RPC call returned: {}", retval);
}
```

# TODO

 - [_] Share code between websocket clients.