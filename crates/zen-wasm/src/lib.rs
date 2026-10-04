//! WASM bindings for `@zen/client` (docs/MILESTONE-4.md, step 2).
//!
//! A thin layer: every format and every algorithm lives in zen-core, every
//! wire type in zen-proto. This crate converts between JS values and those
//! types, and keeps secrets in WASM memory behind opaque handles, which
//! zeroize on `free()`.
//!
//! * [`wire`]: CBOR `encode<Type>` / `decode<Type>` for every zen-proto type.
//! * [`crypto`]: fs keys, keyslots, the fs header, identities, sealing,
//!   password keys, OPAQUE, the filesystem op encodings.
//!
//! Errors are JS `Error`s named `ZenCryptoError` whose message is a stable
//! code: `decrypt`, `format`, `param`, `signature`, `rng`, or `wire: …`.

use wasm_bindgen::prelude::*;

pub mod crypto;
pub mod wire;

#[wasm_bindgen(typescript_custom_section)]
const TS_BYTEBUF: &str = "/** A CBOR byte string. */\nexport type ByteBuf = Uint8Array;";

/// A `ZenCryptoError` with a stable code as its message.
pub(crate) fn js_err(code: &str) -> JsValue {
    let e = js_sys::Error::new(code);
    e.set_name("ZenCryptoError");
    e.into()
}

pub(crate) fn core_err(e: zen_core::Error) -> JsValue {
    js_err(match e {
        zen_core::Error::Decrypt => "decrypt",
        zen_core::Error::Format => "format",
        zen_core::Error::Param => "param",
        zen_core::Error::Signature => "signature",
        zen_core::Error::Rng => "rng",
    })
}

/// The JS conversion of wire values: objects for structs, `Uint8Array` for
/// byte strings, `bigint` for every `u64`, `undefined` for absent fields.
pub(crate) fn serializer() -> serde_wasm_bindgen::Serializer {
    serde_wasm_bindgen::Serializer::new()
        .serialize_maps_as_objects(true)
        .serialize_large_number_types_as_bigints(true)
}

/// Define `encode<Type>` and `decode<Type>` for a list of wire types.
#[macro_export]
#[doc(hidden)]
macro_rules! wire_fns {
    ($($ty:path => $ts:literal, $enc:ident, $dec:ident, $jsenc:literal, $jsdec:literal;)*) => {
        use wasm_bindgen::prelude::*;
        $(
            #[doc = concat!("CBOR-encode a `", $ts, "`.")]
            #[wasm_bindgen(js_name = $jsenc)]
            pub fn $enc(#[wasm_bindgen(unchecked_param_type = $ts)] value: JsValue) -> Result<Vec<u8>, JsValue> {
                let v: $ty = serde_wasm_bindgen::from_value(value)
                    .map_err(|e| $crate::js_err(&format!("wire: {e}")))?;
                Ok(zen_proto::to_cbor(&v))
            }

            #[doc = concat!("Decode a CBOR `", $ts, "`.")]
            #[wasm_bindgen(js_name = $jsdec, unchecked_return_type = $ts)]
            pub fn $dec(bytes: &[u8]) -> Result<JsValue, JsValue> {
                let v: $ty = zen_proto::from_cbor(bytes)
                    .map_err(|e| $crate::js_err(&format!("wire: {e}")))?;
                serde::Serialize::serialize(&v, &$crate::serializer())
                    .map_err(|e| $crate::js_err(&format!("wire: {e}")))
            }
        )*
    };
}
