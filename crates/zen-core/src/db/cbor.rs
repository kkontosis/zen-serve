//! Deterministic CBOR for zen-db values (spec/zendb.md §1).
//!
//! * Values: null, booleans, integers in −2^63 … 2^64−1, 64-bit floats,
//!   text, bytes, arrays and maps. No tags, no other simple values.
//! * Shortest arguments, definite lengths.
//! * A number whose value is an integer in −2^53 … 2^53 is an integer, even
//!   if it was a float. Other finite floats are always 64-bit. NaN is refused.
//! * Map keys are text or unsigned integers, sorted by the bytewise order of
//!   their encodings; duplicates are refused.

use crate::{Error, Result};

/// A zen-db value.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    /// `null`.
    Null,
    /// `false` / `true`.
    Bool(bool),
    /// An integer in −2^63 … 2^64−1.
    Int(i128),
    /// A float. Integral values in −2^53 … 2^53 encode as integers.
    Float(f64),
    /// UTF-8 text.
    Text(String),
    /// A byte string.
    Bytes(Vec<u8>),
    /// An array.
    Array(Vec<Value>),
    /// A map, in any order; encoding sorts it.
    Map(Vec<(Value, Value)>),
}

const INT_MIN: i128 = -(1i128 << 63);
const INT_MAX: i128 = (1i128 << 64) - 1;
const SAFE: f64 = 9_007_199_254_740_992.0; // 2^53
/// Nesting limit of the decoder.
pub const MAX_DEPTH: usize = 64;

impl Value {
    /// A text value.
    pub fn text(s: &str) -> Value {
        Value::Text(s.to_string())
    }

    /// A map from `(key, value)` pairs.
    pub fn map<K: Into<Value>>(items: Vec<(K, Value)>) -> Value {
        Value::Map(items.into_iter().map(|(k, v)| (k.into(), v)).collect())
    }

    /// The value with integral floats in the safe range turned into integers,
    /// as the encoding would.
    pub fn normalized_number(&self) -> Option<Value> {
        match self {
            Value::Int(i) => Some(Value::Int(*i)),
            Value::Float(f) => Some(norm_float(*f)),
            _ => None,
        }
    }

    /// Look up a map entry by text key.
    pub fn get(&self, key: &str) -> Option<&Value> {
        match self {
            Value::Map(m) => m
                .iter()
                .find(|(k, _)| matches!(k, Value::Text(t) if t == key))
                .map(|(_, v)| v),
            _ => None,
        }
    }

    /// Look up a map entry by integer key.
    pub fn at(&self, key: i128) -> Option<&Value> {
        match self {
            Value::Map(m) => m
                .iter()
                .find(|(k, _)| matches!(k, Value::Int(i) if *i == key))
                .map(|(_, v)| v),
            _ => None,
        }
    }
}

impl From<&str> for Value {
    fn from(s: &str) -> Value {
        Value::text(s)
    }
}

impl From<i128> for Value {
    fn from(i: i128) -> Value {
        Value::Int(i)
    }
}

impl From<u64> for Value {
    fn from(i: u64) -> Value {
        Value::Int(i as i128)
    }
}

impl From<Vec<u8>> for Value {
    fn from(b: Vec<u8>) -> Value {
        Value::Bytes(b)
    }
}

fn norm_float(f: f64) -> Value {
    if f.is_finite() && f.fract() == 0.0 && f.abs() <= SAFE {
        Value::Int(f as i128)
    } else {
        Value::Float(f)
    }
}

fn head(out: &mut Vec<u8>, major: u8, arg: u64) {
    let m = major << 5;
    if arg < 24 {
        out.push(m | arg as u8);
    } else if arg <= 0xff {
        out.extend_from_slice(&[m | 24, arg as u8]);
    } else if arg <= 0xffff {
        out.push(m | 25);
        out.extend_from_slice(&(arg as u16).to_be_bytes());
    } else if arg <= 0xffff_ffff {
        out.push(m | 26);
        out.extend_from_slice(&(arg as u32).to_be_bytes());
    } else {
        out.push(m | 27);
        out.extend_from_slice(&arg.to_be_bytes());
    }
}

/// Length of a head with argument `arg`.
pub fn head_len(arg: u64) -> usize {
    if arg < 24 {
        1
    } else if arg <= 0xff {
        2
    } else if arg <= 0xffff {
        3
    } else if arg <= 0xffff_ffff {
        5
    } else {
        9
    }
}

fn enc_int(out: &mut Vec<u8>, i: i128) -> Result<()> {
    if !(INT_MIN..=INT_MAX).contains(&i) {
        return Err(Error::Param);
    }
    if i >= 0 {
        head(out, 0, i as u64);
    } else {
        head(out, 1, (-1 - i) as u64);
    }
    Ok(())
}

fn enc(out: &mut Vec<u8>, v: &Value, depth: usize) -> Result<()> {
    if depth > MAX_DEPTH {
        return Err(Error::Param);
    }
    match v {
        Value::Null => out.push(0xf6),
        Value::Bool(false) => out.push(0xf4),
        Value::Bool(true) => out.push(0xf5),
        Value::Int(i) => enc_int(out, *i)?,
        Value::Float(f) => {
            if f.is_nan() {
                return Err(Error::Param);
            }
            match norm_float(*f) {
                Value::Int(i) => enc_int(out, i)?,
                _ => {
                    out.push(0xfb);
                    out.extend_from_slice(&f.to_bits().to_be_bytes());
                }
            }
        }
        Value::Text(s) => {
            head(out, 3, s.len() as u64);
            out.extend_from_slice(s.as_bytes());
        }
        Value::Bytes(b) => {
            head(out, 2, b.len() as u64);
            out.extend_from_slice(b);
        }
        Value::Array(a) => {
            head(out, 4, a.len() as u64);
            for x in a {
                enc(out, x, depth + 1)?;
            }
        }
        Value::Map(m) => {
            let mut items = Vec::with_capacity(m.len());
            for (k, x) in m {
                match k {
                    Value::Text(_) | Value::Int(0..) => {}
                    Value::Float(f) if f.fract() == 0.0 && *f >= 0.0 && *f <= SAFE => {}
                    _ => return Err(Error::Param),
                }
                let mut kb = Vec::new();
                enc(&mut kb, k, depth + 1)?;
                let mut vb = Vec::new();
                enc(&mut vb, x, depth + 1)?;
                items.push((kb, vb));
            }
            items.sort_by(|a, b| a.0.cmp(&b.0));
            if items.windows(2).any(|w| w[0].0 == w[1].0) {
                return Err(Error::Param);
            }
            head(out, 5, items.len() as u64);
            for (k, x) in items {
                out.extend_from_slice(&k);
                out.extend_from_slice(&x);
            }
        }
    }
    Ok(())
}

/// Encode a value deterministically.
pub fn encode(v: &Value) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    enc(&mut out, v, 0)?;
    Ok(out)
}

struct Dec<'a> {
    b: &'a [u8],
    i: usize,
}

impl Dec<'_> {
    fn take(&mut self, n: usize) -> Result<&[u8]> {
        let end = self.i.checked_add(n).ok_or(Error::Format)?;
        if end > self.b.len() {
            return Err(Error::Format);
        }
        let s = &self.b[self.i..end];
        self.i = end;
        Ok(s)
    }

    fn arg(&mut self, info: u8) -> Result<u64> {
        Ok(match info {
            0..=23 => info as u64,
            24 => self.take(1)?[0] as u64,
            25 => u16::from_be_bytes(self.take(2)?.try_into().expect("2")) as u64,
            26 => u32::from_be_bytes(self.take(4)?.try_into().expect("4")) as u64,
            27 => u64::from_be_bytes(self.take(8)?.try_into().expect("8")),
            _ => return Err(Error::Format),
        })
    }

    fn len(&mut self, info: u8) -> Result<usize> {
        let n = self.arg(info)?;
        // Every item takes at least one byte, so a longer length can't be real.
        if n > (self.b.len() - self.i) as u64 {
            return Err(Error::Format);
        }
        Ok(n as usize)
    }

    fn value(&mut self, depth: usize) -> Result<Value> {
        if depth > MAX_DEPTH {
            return Err(Error::Format);
        }
        let ib = self.take(1)?[0];
        let (major, info) = (ib >> 5, ib & 0x1f);
        Ok(match major {
            0 => Value::Int(self.arg(info)? as i128),
            1 => Value::Int(-1 - self.arg(info)? as i128),
            2 => {
                let n = self.len(info)?;
                Value::Bytes(self.take(n)?.to_vec())
            }
            3 => {
                let n = self.len(info)?;
                let s = self.take(n)?;
                Value::Text(String::from_utf8(s.to_vec()).map_err(|_| Error::Format)?)
            }
            4 => {
                let n = self.len(info)?;
                let mut a = Vec::with_capacity(n.min(1024));
                for _ in 0..n {
                    a.push(self.value(depth + 1)?);
                }
                Value::Array(a)
            }
            5 => {
                let n = self.len(info)?;
                let mut m: Vec<(Value, Value)> = Vec::with_capacity(n.min(1024));
                for _ in 0..n {
                    let k = self.value(depth + 1)?;
                    if !matches!(k, Value::Text(_) | Value::Int(0..))
                        || m.iter().any(|(x, _)| *x == k)
                    {
                        return Err(Error::Format);
                    }
                    let v = self.value(depth + 1)?;
                    m.push((k, v));
                }
                Value::Map(m)
            }
            7 => match info {
                20 => Value::Bool(false),
                21 => Value::Bool(true),
                22 => Value::Null,
                27 => {
                    let f =
                        f64::from_bits(u64::from_be_bytes(self.take(8)?.try_into().expect("8")));
                    if f.is_nan() {
                        return Err(Error::Format);
                    }
                    Value::Float(f)
                }
                _ => return Err(Error::Format),
            },
            _ => return Err(Error::Format), // tags
        })
    }
}

/// Decode exactly one value.
pub fn decode(bytes: &[u8]) -> Result<Value> {
    let (v, n) = decode_prefix(bytes)?;
    if n != bytes.len() {
        return Err(Error::Format);
    }
    Ok(v)
}

/// Decode one value at the start of `bytes`; returns it and its length.
pub fn decode_prefix(bytes: &[u8]) -> Result<(Value, usize)> {
    let mut d = Dec { b: bytes, i: 0 };
    let v = d.value(0)?;
    Ok((v, d.i))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(v: &Value) -> String {
        encode(v)
            .unwrap()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }

    #[test]
    fn numbers() {
        assert_eq!(hex(&Value::Int(0)), "00");
        assert_eq!(hex(&Value::Int(-1)), "20");
        assert_eq!(hex(&Value::Int(24)), "1818");
        assert_eq!(hex(&Value::Float(3.0)), "03");
        assert_eq!(hex(&Value::Float(-0.0)), "00");
        assert_eq!(hex(&Value::Float(1.5)), "fb3ff8000000000000");
        assert_eq!(hex(&Value::Int(INT_MAX)), "1bffffffffffffffff");
        assert_eq!(hex(&Value::Int(INT_MIN)), "3b7fffffffffffffff");
        assert!(encode(&Value::Int(INT_MAX + 1)).is_err());
        assert!(encode(&Value::Float(f64::NAN)).is_err());
    }

    #[test]
    fn map_order_and_roundtrip() {
        let v = Value::map(vec![
            ("bb", Value::Int(1)),
            ("a", Value::Int(2)),
            ("c", Value::Null),
        ]);
        assert_eq!(hex(&v), "a36161026163f662626201");
        let d = decode(&encode(&v).unwrap()).unwrap();
        assert_eq!(encode(&d).unwrap(), encode(&v).unwrap());
        assert!(encode(&Value::map(vec![("a", Value::Null), ("a", Value::Null)])).is_err());
        assert!(decode(&[0xa2, 0x61, 0x61, 0xf6, 0x61, 0x61, 0xf6]).is_err());
        assert!(decode(&[0x9f, 0xff]).is_err());
        assert!(decode(&[0xc1, 0x00]).is_err());
        assert!(decode(&[0xf9, 0x3c, 0x00]).is_err());
    }
}
