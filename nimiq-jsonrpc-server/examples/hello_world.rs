use async_trait::async_trait;
use serde::{Serialize, Deserialize};

use nimiq_jsonrpc_server::{Server, Config};
use nimiq_jsonrpc_client::http::HttpClient;


#[derive(Debug, Serialize, Deserialize)]
struct Goodbye {
    a: u32,
}


#[nimiq_jsonrpc_derive::proxy]
#[async_trait]
trait Foobar {
    async fn hello(&mut self, name: String) -> String;
    async fn bye(&mut self) -> Goodbye;
    //async fn may_fail(&self) -> Result<String, ()>;
}

struct FoobarService;

#[nimiq_jsonrpc_derive::service]
#[async_trait]
impl Foobar for FoobarService {
    async fn hello(&mut self, name: String) -> String {
        println!("Hello, {}", name);
        format!("Hello, {}", name)
    }

    async fn bye(&mut self) -> Goodbye {
        Goodbye { a: 42 }
    }

    //#[jsonrpc(result)]
    /*async fn may_fail(&self) -> Result<String, ()> {
        // This will respond with a non-descriptive internal error
        Err(())
    }*/
}




#[tokio::main]
async fn main() {
    dotenv::dotenv().ok();
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
