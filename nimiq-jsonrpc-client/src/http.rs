use serde::{
    ser::Serialize,
    de::Deserialize,
};
use async_trait::async_trait;

use nimiq_jsonrpc_core::{Request, Response};

use crate::Client;



pub struct HttpClient {
    next_id: usize,
    client: reqwest::Client,
    url: String,
}

impl HttpClient {
    pub fn new(url: &str) -> Self {
        Self {
            next_id: 1,
            client: reqwest::Client::new(),
            url: url.to_owned(),
        }
    }
}

#[async_trait]
impl Client for HttpClient {
    async fn call_method<P, R>(&mut self, method: &str, args: &P) -> R
        where P: Serialize + std::fmt::Debug + Send + Sync,
              R: for<'de> Deserialize<'de> + std::fmt::Debug + Send + Sync,
    {
        log::debug!("Method called: {}: {:?}", method, args);

        let args_value = serde_json::to_value(args)
            .expect("Failed to serialize args struct");
        let mut request = Request::new(method.to_owned(), Some(args_value));

        request.id = Some(serde_json::Value::Number(self.next_id.into()));
        self.next_id += 1;

        let response: Response = self.client.post(&self.url)
            .json(&request)
            .send()
            .await.expect("HTTP request failed")
            .json()
            .await.expect("Failed to deserialize JSON RPC response");

        match (response.result, response.error) {
            (Some(result), None ) => {
                serde_json::from_value(result)
                    .expect("Failed to deserialize return value")
            },
            (None, Some(error)) => {
                panic!("JSON RPC error: {}", error);
            }
            _ => {
                panic!("Invalid JSON RPC response");
            }
        }
    }
}
