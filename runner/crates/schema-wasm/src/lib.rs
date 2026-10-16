//! `tmuxedo-schema` behind a small C ABI, for `wasm32-unknown-unknown`.
//!
//! No bindings generator: three exports and plain memory. The caller writes
//! two JSON texts into buffers from [`tsx_alloc`], calls [`tsx_validate`] and
//! reads the answer: a `u32` length (little endian) followed by that many
//! bytes of JSON (`{"schema": [...], "value": [...]}`, see
//! `tmuxedo_schema::Outcome::to_json`). Every buffer is given back with
//! [`tsx_free`] and its whole length.

use std::ptr;
use std::slice;

const NOT_UTF8: &str = r#"{"schema":[{"path":"","code":"json","message":"not UTF-8"}],"value":[]}"#;

/// The answer for two JSON texts as bytes.
pub fn validate_bytes(schema: &[u8], value: &[u8]) -> String {
    let Ok(s) = std::str::from_utf8(schema) else {
        return NOT_UTF8.to_string();
    };
    let Ok(v) = std::str::from_utf8(value) else {
        return NOT_UTF8.to_string();
    };
    tmuxedo_schema::validate_text(s, v).to_json().to_string()
}

fn leak(buf: Vec<u8>) -> *mut u8 {
    Box::into_raw(buf.into_boxed_slice()) as *mut u8
}

/// A zeroed buffer of `len` bytes for the caller to fill.
#[no_mangle]
pub extern "C" fn tsx_alloc(len: usize) -> *mut u8 {
    leak(vec![0u8; len])
}

/// Give a buffer back.
///
/// # Safety
/// `ptr` and `len` are a buffer from [`tsx_alloc`], or an answer from
/// [`tsx_validate`] with its whole length (4 + the length it starts with),
/// and it is not used again.
#[no_mangle]
pub unsafe extern "C" fn tsx_free(ptr: *mut u8, len: usize) {
    if !ptr.is_null() {
        drop(Box::from_raw(ptr::slice_from_raw_parts_mut(ptr, len)));
    }
}

/// Validate a value against a schema; both are JSON texts.
///
/// # Safety
/// `s` and `v` point to `s_len` and `v_len` readable bytes.
#[no_mangle]
pub unsafe extern "C" fn tsx_validate(s: *const u8, s_len: usize, v: *const u8, v_len: usize) -> *mut u8 {
    let schema = slice::from_raw_parts(s, s_len);
    let value = slice::from_raw_parts(v, v_len);
    let text = validate_bytes(schema, value);
    let mut buf = Vec::with_capacity(4 + text.len());
    buf.extend_from_slice(&(text.len() as u32).to_le_bytes());
    buf.extend_from_slice(text.as_bytes());
    leak(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(schema: &str, value: &str) -> String {
        unsafe {
            let s = tsx_alloc(schema.len());
            let v = tsx_alloc(value.len());
            ptr::copy_nonoverlapping(schema.as_ptr(), s, schema.len());
            ptr::copy_nonoverlapping(value.as_ptr(), v, value.len());
            let out = tsx_validate(s, schema.len(), v, value.len());
            let mut head = [0u8; 4];
            ptr::copy_nonoverlapping(out, head.as_mut_ptr(), 4);
            let len = u32::from_le_bytes(head) as usize;
            let text = slice::from_raw_parts(out.add(4), len).to_vec();
            tsx_free(s, schema.len());
            tsx_free(v, value.len());
            tsx_free(out, 4 + len);
            String::from_utf8(text).unwrap()
        }
    }

    #[test]
    fn the_abi_round_trips() {
        let schema = r#"{"type":"object","properties":{"on":{"type":"boolean"}}}"#;
        let good = call(schema, r#"{"on":true}"#);
        assert_eq!(good, r#"{"schema":[],"value":[]}"#);
        let bad = call(schema, r#"{"on":"yes"}"#);
        assert!(bad.contains(r#""code":"type""#), "{bad}");
        assert!(bad.contains(r#""path":"/on""#), "{bad}");
        assert!(call("", "").contains(r#""code":"json""#));
    }

    #[test]
    fn bytes_that_are_not_text_are_refused() {
        assert_eq!(validate_bytes(&[0xff], b"{}"), NOT_UTF8);
        assert_eq!(validate_bytes(b"{}", &[0xff]), NOT_UTF8);
    }
}
