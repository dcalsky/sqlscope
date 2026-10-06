//! The WebAssembly module behind the sqlscope TypeScript SDK.
//!
//! [`call`] takes an operation name and a JSON request and returns the JSON
//! response of the protocol in `sqlscope-json`, the same one the C ABI uses.
//! Panics abort the module (wasm32-unknown-unknown cannot unwind); the host
//! sees a `WebAssembly.RuntimeError`.

use wasm_bindgen::prelude::wasm_bindgen;

/// Runs `operation` on the JSON `request` and returns the JSON response.
#[wasm_bindgen]
pub fn call(operation: &str, request: &str) -> String {
    let response = sqlscope_json::execute(operation, request.as_bytes());
    serde_json::to_string(&response).expect("serializable response")
}

/// The sqlscope version ("X.Y.Z").
#[wasm_bindgen]
pub fn version() -> String {
    env!("CARGO_PKG_VERSION").to_owned()
}
