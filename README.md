[![Build + Test](https://github.com/nimiq/jsonrpc/actions/workflows/build+test.yml/badge.svg)](https://github.com/nimiq/jsonrpc/actions/workflows/build+test.yml)
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
    async fn hello(&self, name: String) -> String;
}

struct FoobarService;

#[nimiq_jsonrpc_derive::service]
#[async_trait]
impl Foobar for FoobarService {
    async fn hello(&self, name: String) -> String {
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

# Deprecating methods

RPC methods can be marked with the standard Rust `#[deprecated]` attribute, in a `#[proxy]` trait
or a `#[service]` impl (inherent or trait impl):

```rust
#[nimiq_jsonrpc_derive::proxy]
#[async_trait]
trait Foobar {
    type Error;

    #[deprecated(since = "1.1.0", note = "use `hello` instead")]
    async fn hi(&self, name: String) -> Result<String, Self::Error>;
    async fn hello(&self, name: String) -> Result<String, Self::Error>;
}
```

This gives you reporting at three levels:

 - **Rust callers** of the generated proxy get a compile-time deprecation warning (the standard
   `#[deprecated]` behaviour).
 - **The server** logs a warning (via the `log` crate) every time a deprecated method is dispatched,
   so operators can see who is still calling it.
 - **Any client** (in any language) can query the built-in `rpc.deprecatedMethods` method, which
   returns the deprecated methods together with their metadata, e.g.
   `{"hi": {"note": "use `hello` instead", "since": "1.1.0"}}`, and `rpc.methods` for the full list
   of method names. This is also exposed in Rust through `Dispatcher::deprecated_methods()`.

Both `#[deprecated = "note"]` and `#[deprecated(since = "...", note = "...")]` are supported; the
note and version show up in the server log and in `rpc.deprecatedMethods`. One caveat: Rust itself
rejects `#[deprecated]` on the methods of a trait impl (`#[service] impl MyInterface for
MyService`), so there the macro consumes the attribute instead of leaving it in place — mark the
method on the interface trait as well to give Rust callers the usual compile-time warning. (The
reverse also holds: an attribute only on the interface trait is invisible to the macro, so it's
the one on the trait impl that feeds the server-side reporting.)

# TODO

 - [_] Share code between websocket clients.