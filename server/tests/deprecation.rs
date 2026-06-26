//! Tests for marking RPC methods as `#[deprecated]`.

use nimiq_jsonrpc_core::{Request, RpcError};
use nimiq_jsonrpc_server::Dispatcher;
use serde_json::json;

#[derive(Default)]
struct TestService;

#[nimiq_jsonrpc_derive::service(rename_all = "camelCase")]
#[allow(dead_code)]
impl TestService {
    async fn current_method(&self, value: u32) -> Result<u32, RpcError> {
        Ok(value + 1)
    }

    #[deprecated = "use current_method instead"]
    async fn old_method(&self, value: u32) -> Result<u32, RpcError> {
        Ok(value + 1)
    }

    #[deprecated]
    async fn other_old_method(&self) -> Result<(), RpcError> {
        Ok(())
    }
}

#[test]
fn it_reports_deprecated_methods() {
    let service = TestService;

    // All methods are listed (renamed to camelCase).
    let mut methods = service.method_names();
    methods.sort();
    assert_eq!(
        methods,
        vec!["currentMethod", "oldMethod", "otherOldMethod"]
    );

    // Only the `#[deprecated]` ones show up here.
    let mut deprecated = service.deprecated_methods();
    deprecated.sort();
    assert_eq!(deprecated, vec!["oldMethod", "otherOldMethod"]);
}

#[tokio::test]
async fn deprecated_methods_still_dispatch() {
    let mut service = TestService;

    let request = Request::new(
        "oldMethod".to_owned(),
        Some(json!({ "value": 41 })),
        Some(json!(1)),
    );

    let (response, _notifier) = service
        .dispatch(request, None, 0, None)
        .await
        .expect("expected a response");

    assert_eq!(response.result, Some(json!(42)));
    assert!(response.error.is_none());
}
