//! Order-preserving sort keys of private indexes (spec/zendb.md §5.1).

use super::cbor::Value;
use crate::{Error, Result};

/// A declared field type (zendb.md §3.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Type {
    /// UTF-8 text.
    Text,
    /// Bytes.
    Bytes,
    /// Integer.
    Int,
    /// 64-bit float.
    Float,
    /// Boolean.
    Bool,
}

impl Type {
    /// Parse a type name.
    pub fn parse(s: &str) -> Result<Type> {
        Ok(match s {
            "text" => Type::Text,
            "bytes" => Type::Bytes,
            "int" => Type::Int,
            "float" => Type::Float,
            "bool" => Type::Bool,
            _ => return Err(Error::Param),
        })
    }
}

/// Longest sort key an index accepts (zendb.md §5.1).
pub const MAX_SORT_KEY: usize = 4096;

fn escaped(out: &mut Vec<u8>, tag: u8, b: &[u8]) {
    out.push(tag);
    for &x in b {
        out.push(x);
        if x == 0 {
            out.push(0xff);
        }
    }
    out.push(0);
}

fn int(out: &mut Vec<u8>, i: i128) {
    if i <= i64::MAX as i128 {
        out.push(0x20);
        out.extend_from_slice(&((i as i64 as u64) ^ (1 << 63)).to_be_bytes());
    } else {
        out.push(0x21);
        out.extend_from_slice(&(i as u64).to_be_bytes());
    }
}

fn float(out: &mut Vec<u8>, f: f64) -> Result<()> {
    if f.is_nan() {
        return Err(Error::Param);
    }
    let f = if f == 0.0 { 0.0 } else { f };
    let bits = f.to_bits();
    out.push(0x28);
    let k = if bits >> 63 == 0 {
        bits ^ (1 << 63)
    } else {
        !bits
    };
    out.extend_from_slice(&k.to_be_bytes());
    Ok(())
}

/// Append the encoding of one component. With `ty` the value must match the
/// declared type (an `int` field takes integral floats, a `float` field
/// integers); without it, the value's own type decides (pk components).
pub fn component(out: &mut Vec<u8>, v: &Value, ty: Option<Type>) -> Result<()> {
    let v = v.normalized_number().unwrap_or_else(|| v.clone());
    match (&v, ty) {
        (Value::Null, _) => out.push(0x00),
        (Value::Bool(b), None | Some(Type::Bool)) => out.push(if *b { 0x11 } else { 0x10 }),
        (Value::Int(i), None | Some(Type::Int)) => int(out, *i),
        (Value::Int(i), Some(Type::Float)) => float(out, *i as f64)?,
        (Value::Float(f), None | Some(Type::Float)) => float(out, *f)?,
        (Value::Text(s), None | Some(Type::Text)) => escaped(out, 0x30, s.as_bytes()),
        (Value::Bytes(b), None | Some(Type::Bytes)) => escaped(out, 0x40, b),
        _ => return Err(Error::Param),
    }
    Ok(())
}

/// One indexed field: its value, declared type and direction.
pub struct Field<'a> {
    /// The value (`Null` for a missing field).
    pub value: &'a Value,
    /// The declared type.
    pub ty: Type,
    /// Descending order.
    pub desc: bool,
}

/// The sort key of an index entry: each field (inverted when descending),
/// then the pk components ascending. Fails with `Param` on a type mismatch
/// or a key over [`MAX_SORT_KEY`].
pub fn sort_key(fields: &[Field<'_>], pk: &[&Value]) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    for f in fields {
        let start = out.len();
        component(&mut out, f.value, Some(f.ty))?;
        if f.desc {
            for b in &mut out[start..] {
                *b ^= 0xff;
            }
        }
    }
    for p in pk {
        if matches!(p, Value::Null | Value::Array(_) | Value::Map(_)) {
            return Err(Error::Param);
        }
        component(&mut out, p, None)?;
    }
    if out.len() > MAX_SORT_KEY {
        return Err(Error::Param);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn k(v: Value, ty: Type) -> Vec<u8> {
        let mut o = Vec::new();
        component(&mut o, &v, Some(ty)).unwrap();
        o
    }

    #[test]
    fn order() {
        let ints = [
            i64::MIN as i128,
            -5,
            -1,
            0,
            1,
            7,
            i64::MAX as i128,
            u64::MAX as i128,
        ];
        for w in ints.windows(2) {
            assert!(k(Value::Int(w[0]), Type::Int) < k(Value::Int(w[1]), Type::Int));
        }
        let fl = [f64::NEG_INFINITY, -2.5, -0.5, 0.0, 0.25, 3.0, f64::INFINITY];
        for w in fl.windows(2) {
            assert!(k(Value::Float(w[0]), Type::Float) < k(Value::Float(w[1]), Type::Float));
        }
        assert_eq!(
            k(Value::Float(-0.0), Type::Float),
            k(Value::Int(0), Type::Float)
        );
        let t = ["", "a", "a\0", "a\0b", "ab", "b"];
        for w in t.windows(2) {
            assert!(k(Value::text(w[0]), Type::Text) < k(Value::text(w[1]), Type::Text));
        }
        assert!(k(Value::Null, Type::Int) < k(Value::Int(i64::MIN as i128), Type::Int));
        assert!(component(&mut Vec::new(), &Value::text("x"), Some(Type::Int)).is_err());
        assert!(component(&mut Vec::new(), &Value::Float(1.5), Some(Type::Int)).is_err());
    }
}
