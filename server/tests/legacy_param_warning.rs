use std::sync::Mutex;

use async_trait::async_trait;
use log::{Level, Metadata, Record};
use serde_json::{json, Value};

use nimiq_jsonrpc_core::Request;
use nimiq_jsonrpc_server::Dispatcher;

// ---- minimal capturing logger ------------------------------------------------

static WARNINGS: Mutex<Vec<String>> = Mutex::new(Vec::new());

struct CaptureLogger;

impl log::Log for CaptureLogger {
    fn enabled(&self, metadata: &Metadata) -> bool {
        metadata.level() <= Level::Warn
    }

    fn log(&self, record: &Record) {
        if record.level() == Level::Warn {
            WARNINGS.lock().unwrap().push(record.args().to_string());
        }
    }

    fn flush(&self) {}
}

static LOGGER: CaptureLogger = CaptureLogger;

fn install_logger() {
    // `set_logger` only succeeds once per process; ignore the error on the
    // second-and-later calls so every test can call this freely.
    let _ = log::set_logger(&LOGGER);
    log::set_max_level(log::LevelFilter::Warn);
}

fn drain_warnings() -> Vec<String> {
    std::mem::take(&mut *WARNINGS.lock().unwrap())
}

// ---- service under test ------------------------------------------------------

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

async fn dispatch(method: &str, params: Value) -> nimiq_jsonrpc_core::Response {
    let mut service = CalculatorService;
    let request = Request::new(method.to_owned(), Some(params), Some(json!(1)));
    let (response, _) = service
        .dispatch(request, None, 0, None)
        .await
        .expect("expected a response for a request carrying an id");
    response
}

// The capture logger is process-global, so these cases must not run
// concurrently. Drive them from a single `#[tokio::test]` and drain between
// steps rather than relying on per-test isolation.
#[tokio::test]
async fn legacy_param_name_emits_a_single_warning() {
    install_logger();

    // New name: must NOT warn.
    let _ = drain_warnings();
    let response = dispatch(
        "addNumbers",
        json!({ "firstOperand": 2, "secondOperand": 3 }),
    )
    .await;
    assert!(response.error.is_none(), "{:?}", response.error);
    assert_eq!(response.result, Some(json!(5)));
    assert!(
        drain_warnings().is_empty(),
        "the renamed (current) parameter names must not trigger a deprecation warning"
    );

    // Legacy (aliased) name: must warn exactly once, and still succeed.
    let response = dispatch(
        "addNumbers",
        json!({ "first_operand": 2, "second_operand": 3 }),
    )
    .await;
    assert!(response.error.is_none(), "{:?}", response.error);
    assert_eq!(response.result, Some(json!(5)));
    let warnings = drain_warnings();
    assert_eq!(
        warnings.len(),
        1,
        "expected exactly one deprecation warning, got: {warnings:?}"
    );
    assert!(
        warnings[0].contains("addNumbers") && warnings[0].contains("deprecated"),
        "warning should name the method and mention deprecation, got: {}",
        warnings[0]
    );

    // A single legacy name among otherwise-current names still warns once.
    let response = dispatch(
        "addNumbers",
        json!({ "first_operand": 2, "secondOperand": 3 }),
    )
    .await;
    assert!(response.error.is_none(), "{:?}", response.error);
    assert_eq!(drain_warnings().len(), 1);
}
