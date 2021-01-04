use async_trait::async_trait;
use tokio::time::Duration;
use futures::stream::{BoxStream, StreamExt};
use async_stream::stream;

use nimiq_jsonrpc_server::{Server, Config};
use nimiq_jsonrpc_client::websocket::WebsocketClient;


#[nimiq_jsonrpc_derive::proxy(name = "HelloWorldProxy")]
#[async_trait]
trait HelloWorld {
    type Error;

    #[stream]
    async fn hello_subscribe(&mut self) -> Result<BoxStream<'static, String>, Self::Error>;
}

struct HelloWorldService;

#[nimiq_jsonrpc_derive::service]
#[async_trait]
impl HelloWorld for HelloWorldService {
    type Error = ();

    #[stream]
    async fn hello_subscribe(&mut self) -> Result<BoxStream<'static, String>, Self::Error> {
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
    dotenv::dotenv().ok();
    pretty_env_logger::init();

    let config = Config::default();
    log::info!("Listening on: {}", config.bind_to);

    let server = Server::new(config, HelloWorldService);
    tokio::spawn(async move {
        server.run().await;
    });

    let url = "ws://localhost:8000/ws".parse().unwrap();
    let client = WebsocketClient::with_url(url).await.unwrap();
    let mut proxy = HelloWorldProxy::new(client);

    let mut stream = proxy.hello_subscribe().await.unwrap();

    while let Some(item) = stream.next().await {
        println!("Received item from stream: {}", item);
    }

}
