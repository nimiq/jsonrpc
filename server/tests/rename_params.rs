use async_trait::async_trait;
use serde_json::{json, Value};

use nimiq_jsonrpc_core::Request;
use nimiq_jsonrpc_server::Dispatcher;

#[async_trait]
trait Calculator {
    type Error;

    async fn add_numbers(
        &self,
        first_operand: u32,
        second_operand: u32,
    ) -> Result<u32, Self::Error>;
}

struct CalculatorService;

#[nimiq_jsonrpc_derive::service(rename_all = "camelCase")]
#[async_trait]
impl Calculator for CalculatorService {
    type Error = ();

    async fn add_numbers(
        &self,
        first_operand: u32,
        second_operand: u32,
    ) -> Result<u32, Self::Error> {
        Ok(first_operand + second_operand)
    }
}

/// Dispatches a request to `CalculatorService` and returns the JSON response
/// (always requesting an `id` so a response is produced).
async fn dispatch(method: &str, params: Value) -> nimiq_jsonrpc_core::Response {
    let mut service = CalculatorService;
    let request = Request::new(method.to_owned(), Some(params), Some(json!(1)));
    let (response, _) = service
        .dispatch(request, None, 0, None)
        .await
        .expect("expected a response for a request carrying an id");
    response
}

#[tokio::test]
async fn method_name_is_renamed_to_camel_case() {
    // `add_numbers` -> `addNumbers`; the original snake_case name is gone.
    assert!(CalculatorService.match_method("addNumbers"));
    assert!(!CalculatorService.match_method("add_numbers"));
}

#[tokio::test]
async fn camel_case_param_names_are_accepted_on_the_wire() {
    let response = dispatch(
        "addNumbers",
        json!({ "firstOperand": 2, "secondOperand": 3 }),
    )
    .await;

    assert!(
        response.error.is_none(),
        "camelCase params should deserialize, got error: {:?}",
        response.error
    );
    assert_eq!(response.result, Some(json!(5)));
}

#[tokio::test]
async fn original_snake_case_param_names_are_still_accepted() {
    // The original snake_case names survive as a serde `alias`, so the
    // pre-rename wire spelling still deserializes. This is the transition shim
    // that keeps existing clients working after a service adopts `rename_all`.
    let response = dispatch(
        "addNumbers",
        json!({ "first_operand": 2, "second_operand": 3 }),
    )
    .await;

    assert!(
        response.error.is_none(),
        "snake_case aliases should still deserialize, got error: {:?}",
        response.error
    );
    assert_eq!(response.result, Some(json!(5)));
}

#[tokio::test]
async fn sending_both_spellings_is_rejected_as_duplicate() {
    // Supplying a field under both its rename and its alias is a duplicate, not
    // a silent merge.
    let response = dispatch(
        "addNumbers",
        json!({ "firstOperand": 2, "first_operand": 2, "secondOperand": 3 }),
    )
    .await;

    let error = response
        .error
        .expect("supplying a field under both its rename and its alias is invalid");
    // -32602 == invalid params per the JSON-RPC spec.
    assert_eq!(error.code, -32602);
}

#[async_trait]
trait KebabCalculator {
    type Error;

    async fn add_numbers(
        &self,
        first_operand: u32,
        second_operand: u32,
    ) -> Result<u32, Self::Error>;
}

struct KebabService;

#[nimiq_jsonrpc_derive::service(rename_all = "kebab-case")]
#[async_trait]
impl KebabCalculator for KebabService {
    type Error = ();

    async fn add_numbers(
        &self,
        first_operand: u32,
        second_operand: u32,
    ) -> Result<u32, Self::Error> {
        Ok(first_operand + second_operand)
    }
}

async fn dispatch_kebab(method: &str, params: Value) -> nimiq_jsonrpc_core::Response {
    let mut service = KebabService;
    let request = Request::new(method.to_owned(), Some(params), Some(json!(1)));
    let (response, _) = service
        .dispatch(request, None, 0, None)
        .await
        .expect("expected a response for a request carrying an id");
    response
}

#[tokio::test]
async fn kebab_case_compiles_and_accepts_both_spellings() {
    // `add_numbers` -> `add-numbers`.
    assert!(KebabService.match_method("add-numbers"));

    // The kebab wire name.
    let renamed = dispatch_kebab(
        "add-numbers",
        json!({ "first-operand": 2, "second-operand": 3 }),
    )
    .await;
    assert!(
        renamed.error.is_none(),
        "kebab params should deserialize, got error: {:?}",
        renamed.error
    );
    assert_eq!(renamed.result, Some(json!(5)));

    // The original snake_case alias.
    let aliased = dispatch_kebab(
        "add-numbers",
        json!({ "first_operand": 2, "second_operand": 3 }),
    )
    .await;
    assert!(
        aliased.error.is_none(),
        "snake_case alias should still deserialize, got error: {:?}",
        aliased.error
    );
    assert_eq!(aliased.result, Some(json!(5)));
}

// A `snake_case` service: every name maps to itself, so no `rename`/`alias`
// attribute is emitted at all (the `new == old` no-op branch). The field name
// stays the plain original, and only that spelling is accepted.
#[async_trait]
trait SnakeCalculator {
    type Error;

    async fn add_numbers(
        &self,
        first_operand: u32,
        second_operand: u32,
    ) -> Result<u32, Self::Error>;
}

struct SnakeService;

#[nimiq_jsonrpc_derive::service(rename_all = "snake_case")]
#[async_trait]
impl SnakeCalculator for SnakeService {
    type Error = ();

    async fn add_numbers(
        &self,
        first_operand: u32,
        second_operand: u32,
    ) -> Result<u32, Self::Error> {
        Ok(first_operand + second_operand)
    }
}

#[tokio::test]
async fn snake_case_identity_emits_no_alias() {
    let mut service = SnakeService;
    let request = Request::new(
        "add_numbers".to_owned(),
        Some(json!({ "first_operand": 2, "second_operand": 3 })),
        Some(json!(1)),
    );
    let (response, _) = service.dispatch(request, None, 0, None).await.unwrap();
    assert!(response.error.is_none(), "{:?}", response.error);
    assert_eq!(response.result, Some(json!(5)));
}
