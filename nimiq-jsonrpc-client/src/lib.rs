#[cfg(feature = "http")]
pub mod http;
#[cfg(feature = "websocket")]
pub mod websocket;


use serde::{
    ser::Serialize,
    de::Deserialize,
};
use async_trait::async_trait;


#[async_trait]
pub trait Client {
    async fn call_method<P, R>(&mut self, method: &str, params: &P) -> R
        where P: Serialize + std::fmt::Debug + Send + Sync,
              R: for<'de> Deserialize<'de> + std::fmt::Debug + Send + Sync;
}
