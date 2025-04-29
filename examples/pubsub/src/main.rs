use std::env;

use async_stream::stream;
use async_trait::async_trait;
use futures::stream::{BoxStream, StreamExt};
use tokio::time::Duration;

use nimiq_jsonrpc_client::{websocket::WebsocketClient, Client};
use nimiq_jsonrpc_server::{Config, Server};

#[nimiq_jsonrpc_derive::proxy(name = "HelloWorldProxy")]
#[async_trait]
trait HelloWorld {
    type Error;

    #[stream]
    async fn hello_subscribe(&self) -> Result<BoxStream<'static, String>, Self::Error>;
}

struct HelloWorldService;

#[nimiq_jsonrpc_derive::service]
#[async_trait]
impl HelloWorld for HelloWorldService {
    type Error = ();

    #[stream]
    async fn hello_subscribe(&self) -> Result<BoxStream<'static, String>, Self::Error> {
        log::info!("Client subscribed");

        let mut interval = tokio::time::interval(Duration::from_secs(1));

        let stream = stream! {
            loop {
                let instant = interval.tick().await;
                yield format!("Hello, World: {:?}", instant);
            }
        };

        Ok(stream.boxed())
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

    let config = Config::default();
    log::info!("Listening on: {}", config.bind_to);

    let server = Server::new(config, HelloWorldService);
    tokio::spawn(async move {
        server.run().await;
    });

    let url = "ws://localhost:8000/ws".parse().unwrap();
    let client = WebsocketClient::with_url(url).await.unwrap();
    let proxy = HelloWorldProxy::new(client);

    let mut stream = proxy.hello_subscribe().await.unwrap();

    // Run the test for 5 seconds before closing the stream'
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(5)).await;

        println!("Close stream");
        if let Err(e) = proxy.client.disconnect_stream(1.into()).await {
            panic!("Error while disconnecting stream: {}", e)
        };
    });

    while let Some(item) = stream.next().await {
        println!("Received item from stream: {}", item);
    }
}
