// Example trait and implementation
use async_trait::async_trait;
use nimiq_jsonrpc_core::Request;

#[derive(Serialize, Deserialize)]
struct Goodbye {
    a: u32,
}

#[async_trait]
pub trait Foobar {
    async fn hello(&self, name: String) -> String;
    async fn bye(&self) -> Goodbye;
}

#[derive(Dispatcher)]
#[jsonrpc(trait="Foobar")]
struct FoobarService;

impl Foobar for FoobarService {
    async fn hello(&self, name: String) -> String {
        format!("Hello, {}", name)
    }

    async fn bye(&self) -> Goodbye {
        Goodbye { a: 42 }
    }
}


#[derive(Proxy)]
#[jsonrpc(trait="Foobar", client="client")]
struct FoobarProxy {
    client: (),
}



mod example_derived {
    #[derive(Serialize, Deserialize)]
    struct FoobarServiceHelloMethodParams {
        name: String,
    }

    impl<T: Foobar> nimiq_jsonrpc_core::Dispatcher for T {
        async fn dispatch(&mut self, request: nimiq_jsonrpc_core::Request) -> Option<nimiq_jsonrpc_core::Response> {
            match request.method.as_str() {
                "hello" => {
                    return nimiq_jsonrpc_core::Dispatcher::dispatch_method_with_params(
                        request,
                        |params: FoobarServiceHelloMethodParams| super::Foobar::hello(self, params.name)
                    );
                },

                "bye" => {
                    return nimiq_jsonrpc_core::Dispatcher::dispatch_method_without_params(
                        request,
                        || super::Foobar::bye(self)
                    );
                }

                // ...

                _ => {
                    return nimiq_jsonrpc_core::Dispatcher::error_response(request.id, nimiq_jsonrpc_core::Error::method_not_found())
                }
            }

            None
        }
    }

    impl Proxy for FoobarProxy {
        async fn call_remote<P, R>(&self, method: &str, params: P) -> Result<R, ProxyError>
            where P: Serialize,
                  R: for<'de> Deserialize<'de>,
        {
            // Field name specified in attribute
            let client = self.client;

            let request = nimiq_jsonrpc_core::Request::new(method.to_onwed(), params);

            response: nimiq_jsonrpc_core::Response = client.call_remote(request).await?;

            match response {
                nimiq_jsonrpc_core::Response { result: Some(result), .. } => {
                    let value = serde_json::from_value(result)?;
                    Ok(value)
                },
                nimiq_jsonrpc_core::Response { error: Some(error), .. } => {
                    Err(error)
                },
                _ => panic!("Invalid response: {:?}", response),
            }
        }
    }

    impl Foobar for FoobarProxy {
        fn hello(name: String) -> String {
            let params = FoobarServiceHelloMethodParams {
                name
            };
        }
    }
}
