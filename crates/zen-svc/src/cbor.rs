//! The engines' `engine_state` maps: CBOR, read strictly. Byte strings may
//! hold keys, so values are zeroized when dropped.

use ciborium::Value;

/// Why a state was refused.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Bad(pub &'static str);

pub(crate) fn text(s: &str) -> Value {
    Value::Text(s.to_owned())
}

/// CBOR bytes of `v`, which is zeroized after.
pub(crate) fn encode(v: Value) -> Vec<u8> {
    let mut out = Vec::new();
    ciborium::into_writer(&v, &mut out).expect("CBOR to a Vec");
    zeroize_value(v);
    out
}

/// Drops a CBOR value; its byte strings are zeroized first.
pub(crate) fn zeroize_value(v: Value) {
    match v {
        Value::Bytes(mut b) => zeroize::Zeroize::zeroize(&mut b),
        Value::Map(m) => m.into_iter().for_each(|(k, v)| {
            zeroize_value(k);
            zeroize_value(v)
        }),
        Value::Array(a) => a.into_iter().for_each(zeroize_value),
        _ => {}
    }
}

/// A CBOR map's fields, taken one by one; [`Fields::done`] refuses any
/// left over.
pub(crate) struct Fields(Vec<(String, Value)>);

impl Drop for Fields {
    fn drop(&mut self) {
        std::mem::take(&mut self.0)
            .into_iter()
            .for_each(|(_, v)| zeroize_value(v));
    }
}

impl Fields {
    pub(crate) fn new(v: Value) -> Result<Fields, Bad> {
        let Value::Map(m) = v else {
            zeroize_value(v);
            return Err(Bad("not a map"));
        };
        let mut out = Fields(Vec::with_capacity(m.len()));
        for (k, v) in m {
            let Value::Text(k) = k else {
                zeroize_value(v);
                return Err(Bad("a key that isn't text"));
            };
            if out.0.iter().any(|(x, _)| *x == k) {
                zeroize_value(v);
                return Err(Bad("a repeated key"));
            }
            out.0.push((k, v));
        }
        Ok(out)
    }

    pub(crate) fn opt(&mut self, k: &str) -> Option<Value> {
        let i = self.0.iter().position(|(x, _)| x == k)?;
        Some(self.0.swap_remove(i).1)
    }

    pub(crate) fn take(&mut self, k: &str) -> Result<Value, Bad> {
        self.opt(k).ok_or(Bad("a missing field"))
    }

    pub(crate) fn uint(&mut self, k: &str) -> Result<u64, Bad> {
        match self.take(k)? {
            Value::Integer(i) => u64::try_from(i).map_err(|_| Bad("not a uint")),
            _ => Err(Bad("not a uint")),
        }
    }

    pub(crate) fn bytes(&mut self, k: &str) -> Result<Vec<u8>, Bad> {
        match self.take(k)? {
            Value::Bytes(b) => Ok(b),
            v => {
                zeroize_value(v);
                Err(Bad("not bytes"))
            }
        }
    }

    pub(crate) fn done(&mut self) -> Result<(), Bad> {
        if self.0.is_empty() {
            Ok(())
        } else {
            Err(Bad("an unknown field"))
        }
    }
}
