//! Native string codecs behind `Buffer` (js/node_compat.js): the byte->string
//! decoders of `Buffer#toString` and the `*Slice` family for LONG spans
//! (`decodeRange`), and the UTF-8 byte count of a LONG string
//! (`utf8Length`). Short ones stay in JS, where they beat the fixed cost of
//! a native call.
//!
//! Why native: a JS decoder builds its result one `String.fromCharCode` at a
//! time, and a JS byte count reads one code unit at a time; on long input
//! both are several times slower than V8's own routines. V8 already owns
//! the decoders node uses for these encodings -- node's StringBytes::Encode
//! hands utf8 to `String::NewFromUtf8` and latin1 / ucs2 to
//! `NewFromOneByte` / `NewFromTwoByte` -- so calling the same V8 entry
//! points gives node's output by construction (U+FFFD replacement of
//! invalid UTF-8 included) rather than by re-implementing it. hex and base64
//! stay in JS: they ride V8's native `Uint8Array.prototype.toHex` /
//! `toBase64` already.
//!
//! No `unsafe` here: `ArrayBufferView::get_contents` is the safe read of a
//! view's bytes (a copy into `storage` for a small on-heap array, a borrow of
//! the off-heap backing store otherwise).

use crate::node_ops::throw_type_error;

/// `bufferUtf8Length(string)` -> the bytes `string` encodes to as UTF-8: a
/// surrogate pair 4, a lone surrogate 3 (its U+FFFD), as node's
/// Buffer.byteLength counts.
pub(crate) fn op_buffer_utf8_length(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let Ok(string) = v8::Local::<v8::String>::try_from(args.get(0)) else {
        throw_type_error(scope, "bufferUtf8Length: argument must be a string");
        return;
    };
    let length = string.utf8_length(scope);
    rv.set(v8::Number::new(scope, length as f64).into());
}

/// The `kind` argument of `bufferDecode`, shared with js/node_compat.js.
const KIND_UTF8: i32 = 0;
const KIND_LATIN1: i32 = 1;
const KIND_ASCII: i32 = 2;
const KIND_UTF16LE: i32 = 3;

/// `bufferDecode(view, start, end, kind)` -> string.
///
/// `start`/`end` are byte indices into `view` that the JS caller has already
/// clamped (to `0 <= start <= end <= view.byteLength`); they are clamped
/// again here only so that a bad call can never index out of bounds.
pub(crate) fn op_buffer_decode(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let Ok(view) = v8::Local::<v8::ArrayBufferView>::try_from(args.get(0)) else {
        throw_type_error(scope, "bufferDecode: argument must be an ArrayBufferView");
        return;
    };
    let len = view.byte_length();
    let index = |scope: &mut v8::PinScope<'_, '_>, i: i32, fallback: usize| {
        let n = args.get(i).number_value(scope).unwrap_or(0.0);
        if n.is_nan() || n < 0.0 {
            0
        } else if n >= fallback as f64 {
            fallback
        } else {
            n as usize
        }
    };
    let end = index(scope, 2, len);
    let start = index(scope, 1, end);
    let kind = args.get(3).int32_value(scope).unwrap_or(KIND_UTF8);
    let mut storage = [0u8; v8::TYPED_ARRAY_MAX_SIZE_IN_HEAP];
    let contents = view.get_contents(&mut storage);
    // get_contents spans the whole view; a detached buffer reads as empty.
    let bytes = contents.get(start..end.min(contents.len())).unwrap_or(&[]);
    let normal = v8::NewStringType::Normal;
    let decoded = match kind {
        KIND_LATIN1 => v8::String::new_from_one_byte(scope, bytes, normal),
        KIND_ASCII => {
            // node's ascii decode clears the high bit of every byte; a span
            // that is already 7-bit needs no copy.
            if bytes.is_ascii() {
                v8::String::new_from_one_byte(scope, bytes, normal)
            } else {
                let masked: Vec<u8> = bytes.iter().map(|b| b & 0x7f).collect();
                v8::String::new_from_one_byte(scope, &masked, normal)
            }
        }
        KIND_UTF16LE => {
            // An odd trailing byte is dropped, as node's ucs2 decode does.
            let units: Vec<u16> = bytes
                .as_chunks::<2>()
                .0
                .iter()
                .map(|pair| u16::from_le_bytes(*pair))
                .collect();
            v8::String::new_from_two_byte(scope, &units, normal)
        }
        _ => v8::String::new_from_utf8(scope, bytes, normal),
    };
    match decoded {
        Some(s) => rv.set(s.into()),
        // Only past v8::String::kMaxLength, which Buffer#toString refuses
        // up front with ERR_STRING_TOO_LONG; this is the backstop.
        None => throw_string_too_long(scope),
    }
}

/// node's ERR_STRING_TOO_LONG: a plain Error carrying the code.
fn throw_string_too_long(scope: &mut v8::PinScope<'_, '_>) {
    let message = v8::String::new(
        scope,
        "Cannot create a string longer than 0x1fffffe8 characters",
    )
    .unwrap();
    let exception = v8::Exception::error(scope, message);
    if let Ok(obj) = v8::Local::<v8::Object>::try_from(exception) {
        let key = v8::String::new(scope, "code").unwrap();
        let code = v8::String::new(scope, "ERR_STRING_TOO_LONG").unwrap();
        obj.set(scope, key.into(), code.into());
    }
    scope.throw_exception(exception);
}
