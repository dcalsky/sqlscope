//! The C ABI of sqlscope, built as a shared and a static library.
//!
//! Every operation goes through one function: [`sqlscope_call`] takes an
//! operation name and a JSON request, both NUL-terminated UTF-8, and returns
//! a NUL-terminated JSON response that the caller releases with
//! [`sqlscope_free`]. The declarations live in `include/sqlscope.h`.
//!
//! A response is either `{"ok": <result>}` or
//! `{"error": {"kind": "<kind>", "message": "<message>"}}` where kind is one
//! of `invalid_argument`, `parse`, `unsupported` or `internal`. Every
//! function is safe to call concurrently from any thread.

use std::ffi::{c_char, CStr, CString};
use std::panic::{catch_unwind, AssertUnwindSafe};

use serde_json::{json, Value};
use sqlscope_json::{execute, invalid, ABI_VERSION};

/// The ABI version implemented by this library.
#[no_mangle]
pub extern "C" fn sqlscope_abi_version() -> u32 {
    ABI_VERSION
}

/// The library version as a static NUL-terminated string; do not free it.
#[no_mangle]
pub extern "C" fn sqlscope_version() -> *const c_char {
    concat!(env!("CARGO_PKG_VERSION"), "\0").as_ptr().cast()
}

/// Reads a NUL-terminated UTF-8 argument.
///
/// # Safety
/// `pointer` must be null or point to a NUL-terminated string.
unsafe fn argument<'a>(pointer: *const c_char, name: &str) -> Result<&'a str, Value> {
    if pointer.is_null() {
        return Err(invalid(format!("{name} is NULL")));
    }
    CStr::from_ptr(pointer)
        .to_str()
        .map_err(|_| invalid(format!("{name} is not UTF-8")))
}

/// Runs `operation` on the JSON `request` and returns the JSON response as a
/// NUL-terminated string owned by the caller; release it with
/// [`sqlscope_free`]. Never returns NULL.
///
/// # Safety
/// `operation` and `request` must be null or point to NUL-terminated strings.
#[no_mangle]
pub unsafe extern "C" fn sqlscope_call(operation: *const c_char, request: *const c_char) -> *mut c_char {
    let response = catch_unwind(AssertUnwindSafe(|| {
        let operation = argument(operation, "operation")?;
        let request = argument(request, "request")?;
        Ok(execute(operation, request.as_bytes()))
    }))
    .unwrap_or_else(|panic| {
        let message = panic
            .downcast_ref::<&str>()
            .map(|s| s.to_string())
            .or_else(|| panic.downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "unknown panic".to_owned());
        Ok(json!({"error": {"kind": "internal", "message": format!("panic: {message}")}}))
    })
    .unwrap_or_else(|error: Value| error);
    // serde_json escapes control characters, so the response has no NUL.
    let bytes = serde_json::to_vec(&response).expect("serializable response");
    CString::new(bytes).expect("JSON has no NUL").into_raw()
}

/// Releases a response returned by [`sqlscope_call`]. NULL is ignored.
///
/// # Safety
/// `response` must be null or a pointer returned by [`sqlscope_call`] that
/// has not been released yet.
#[no_mangle]
pub unsafe extern "C" fn sqlscope_free(response: *mut c_char) {
    if !response.is_null() {
        drop(CString::from_raw(response));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn c_abi_round_trip() {
        let call = |operation: *const c_char, request: *const c_char| unsafe {
            let response = sqlscope_call(operation, request);
            let value: Value = serde_json::from_slice(CStr::from_ptr(response).to_bytes()).unwrap();
            sqlscope_free(response);
            value
        };
        let op = c"output_columns";
        assert_eq!(
            call(op.as_ptr(), c"{\"sql\": \"SELECT a, b AS c FROM t\"}".as_ptr()),
            json!({"ok": ["a", "c"]})
        );
        assert_eq!(
            call(std::ptr::null(), c"{}".as_ptr())["error"]["kind"],
            "invalid_argument"
        );
        assert_eq!(call(op.as_ptr(), std::ptr::null())["error"]["kind"], "invalid_argument");
        assert_eq!(
            call(op.as_ptr(), c"{\"sql\": \"\xff\"}".as_ptr())["error"]["kind"],
            "invalid_argument"
        );
        unsafe { sqlscope_free(std::ptr::null_mut()) };

        let version = unsafe { CStr::from_ptr(sqlscope_version()) };
        assert_eq!(version.to_str().unwrap(), env!("CARGO_PKG_VERSION"));
        assert_eq!(sqlscope_abi_version(), ABI_VERSION);
    }
}
