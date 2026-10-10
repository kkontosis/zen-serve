//! zen-db vectors (spec/zendb.md §17, §19.9): `spec/test-vectors/zendb.json`.
//!
//! Values in the file use a tagged JSON form, so every implementation can
//! rebuild them exactly: `null`, `true`/`false`, `{"int": "<decimal>"}`,
//! `{"float": <number> | "Infinity" | "-Infinity"}`, `{"text": "…"}`,
//! `{"bytes": "<hex>"}`, `[…]` for arrays, `{"map": [[key, value], …]}`.

use crate::db::cbor::{self, Value};
use crate::db::prolly;
use crate::db::sortkey::{self, Field, Type};
use crate::db::{self, DbKeys};
use crate::rng::DetRng;
use crate::token::TopicKeys;
use crate::vectors::fixture_fs;
use crate::{Error, Result};
use serde_json::{Value as J, json};

fn h(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// A value in the tagged JSON form.
pub fn to_json(v: &Value) -> J {
    match v {
        Value::Null => J::Null,
        Value::Bool(b) => J::Bool(*b),
        Value::Int(i) => json!({"int": i.to_string()}),
        Value::Float(f) if f.is_infinite() => {
            json!({"float": if *f > 0.0 { "Infinity" } else { "-Infinity" }})
        }
        Value::Float(f) => json!({"float": f}),
        Value::Text(s) => json!({"text": s}),
        Value::Bytes(b) => json!({"bytes": h(b)}),
        Value::Array(a) => J::Array(a.iter().map(to_json).collect()),
        Value::Map(m) => {
            json!({"map": m.iter().map(|(k, v)| json!([to_json(k), to_json(v)])).collect::<Vec<_>>()})
        }
    }
}

/// The inverse of [`to_json`].
pub fn from_json(j: &J) -> Value {
    match j {
        J::Null => Value::Null,
        J::Bool(b) => Value::Bool(*b),
        J::Array(a) => Value::Array(a.iter().map(from_json).collect()),
        J::Object(o) => {
            if let Some(i) = o.get("int") {
                Value::Int(i.as_str().expect("int").parse().expect("decimal"))
            } else if let Some(f) = o.get("float") {
                Value::Float(match f.as_str() {
                    Some("Infinity") => f64::INFINITY,
                    Some("-Infinity") => f64::NEG_INFINITY,
                    _ => f.as_f64().expect("float"),
                })
            } else if let Some(t) = o.get("text") {
                Value::text(t.as_str().expect("text"))
            } else if let Some(b) = o.get("bytes") {
                Value::Bytes(
                    (0..b.as_str().unwrap().len() / 2)
                        .map(|i| {
                            u8::from_str_radix(&b.as_str().unwrap()[2 * i..2 * i + 2], 16).unwrap()
                        })
                        .collect(),
                )
            } else {
                let m = o.get("map").expect("map").as_array().expect("pairs");
                Value::Map(
                    m.iter()
                        .map(|p| (from_json(&p[0]), from_json(&p[1])))
                        .collect(),
                )
            }
        }
        _ => panic!("not a tagged value: {j}"),
    }
}

fn t(s: &str) -> Value {
    Value::text(s)
}

fn int(i: i128) -> Value {
    Value::Int(i)
}

fn enc(v: &Value) -> Vec<u8> {
    cbor::encode(v).expect("valid vector value")
}

/// The fixture database: fixture fs, `ns = "app"`.
pub const NS: &str = "app";
/// Fixture table id.
pub const TABLE_ID: [u8; 16] = [0x11; 16];
/// Fixture index id.
pub const INDEX_ID: [u8; 16] = [0x22; 16];

fn cbor_vectors() -> J {
    let ok = vec![
        Value::Null,
        Value::Bool(false),
        Value::Bool(true),
        int(0),
        int(23),
        int(24),
        int(255),
        int(256),
        int(65_536),
        int(-1),
        int(-24),
        int(-25),
        int(1 << 53),
        int(i64::MIN as i128),
        int(u64::MAX as i128),
        Value::Float(3.0),
        Value::Float(-0.0),
        Value::Float(1.5),
        Value::Float(-1e300),
        Value::Float(9_007_199_254_740_994.0),
        Value::Float(f64::INFINITY),
        Value::Float(f64::NEG_INFINITY),
        t(""),
        t("zen ✓"),
        Value::Bytes(vec![]),
        Value::Bytes(vec![0, 1, 2]),
        Value::Array(vec![int(1), t("a"), Value::Array(vec![])]),
        Value::map(vec![("b", int(1)), ("a", int(2)), ("aa", Value::Null)]),
        Value::Map(vec![(int(10), t("x")), (int(2), t("y")), (int(1), t("z"))]),
    ];
    let refused_encode = [
        ("NaN", Value::Float(f64::NAN)),
        ("integer above 2^64-1", int(1 << 64)),
        ("integer below -2^63", int(-(1 << 63) - 1)),
        (
            "duplicate key",
            Value::map(vec![("a", int(1)), ("a", int(2))]),
        ),
        ("negative integer key", Value::Map(vec![(int(-1), int(1))])),
        (
            "bytes key",
            Value::Map(vec![(Value::Bytes(vec![1]), int(1))]),
        ),
    ];
    let refused_decode: [(&str, &[u8]); 8] = [
        ("indefinite array", &[0x9f, 0xff]),
        ("tag", &[0xc1, 0x00]),
        ("16-bit float", &[0xf9, 0x3c, 0x00]),
        ("32-bit float", &[0xfa, 0x3f, 0xc0, 0x00, 0x00]),
        ("undefined", &[0xf7]),
        ("duplicate key", &[0xa2, 0x61, 0x61, 0xf6, 0x61, 0x61, 0xf6]),
        ("trailing bytes", &[0x00, 0x00]),
        ("invalid UTF-8", &[0x61, 0xff]),
    ];
    for (_, b) in &refused_decode {
        assert!(cbor::decode(b).is_err());
    }
    json!({
        "description": "Deterministic CBOR (zendb.md §1). `ok`: value → encoding; `decoded` is the value decoding gives back (integral floats come back as integers). `refused_encode` and `refused_decode` must fail.",
        "ok": ok.iter().map(|v| {
            let b = enc(v);
            json!({"value": to_json(v), "cbor": h(&b), "decoded": to_json(&cbor::decode(&b).unwrap())})
        }).collect::<Vec<_>>(),
        "refused_encode": refused_encode.iter().map(|(why, v)| {
            assert!(cbor::encode(v).is_err());
            json!({"why": why, "value": to_json(v)})
        }).collect::<Vec<_>>(),
        "refused_decode": refused_decode.iter().map(|(why, b)| json!({"why": why, "cbor": h(b)})).collect::<Vec<_>>(),
    })
}

fn sort_key_vectors() -> Result<J> {
    let single: Vec<(Value, Type)> = vec![
        (Value::Null, Type::Int),
        (Value::Bool(false), Type::Bool),
        (Value::Bool(true), Type::Bool),
        (int(-1), Type::Int),
        (int(0), Type::Int),
        (int(i64::MAX as i128), Type::Int),
        (int(u64::MAX as i128), Type::Int),
        (Value::Float(-2.5), Type::Float),
        (int(0), Type::Float),
        (Value::Float(0.25), Type::Float),
        (Value::Float(f64::INFINITY), Type::Float),
        (Value::Float(4.0), Type::Int),
        (t(""), Type::Text),
        (t("a\0b"), Type::Text),
        (Value::Bytes(vec![0, 0xff]), Type::Bytes),
    ];
    let name = |ty: Type| match ty {
        Type::Text => "text",
        Type::Bytes => "bytes",
        Type::Int => "int",
        Type::Float => "float",
        Type::Bool => "bool",
    };
    let mut cases = Vec::new();
    for (v, ty) in &single {
        for desc in [false, true] {
            let k = sortkey::sort_key(
                &[Field {
                    value: v,
                    ty: *ty,
                    desc,
                }],
                &[&int(7)],
            )?;
            cases.push(json!({"fields": [{"value": to_json(v), "type": name(*ty), "desc": desc}], "pk": [to_json(&int(7))], "key": h(&k)}));
        }
    }
    let (a, b, pk1, pk2) = (t("todo"), int(3), t("card-9"), int(2));
    let k = sortkey::sort_key(
        &[
            Field {
                value: &a,
                ty: Type::Text,
                desc: false,
            },
            Field {
                value: &b,
                ty: Type::Int,
                desc: true,
            },
        ],
        &[&pk1, &pk2],
    )?;
    cases.push(json!({"fields": [{"value": to_json(&a), "type": "text", "desc": false}, {"value": to_json(&b), "type": "int", "desc": true}],
                      "pk": [to_json(&pk1), to_json(&pk2)], "key": h(&k)}));
    let refused = [
        (t("x"), Type::Int, "text in an int field"),
        (Value::Float(1.5), Type::Int, "fraction in an int field"),
        (Value::Float(f64::NAN), Type::Float, "NaN"),
        (t(&"x".repeat(4096)), Type::Text, "longer than 4,096 bytes"),
    ];
    Ok(json!({
        "description": "Sort keys (zendb.md §5.1): fields encoded by declared type, inverted when desc, then the pk components by their own type.",
        "cases": cases,
        "refused": refused.iter().map(|(v, ty, why)| {
            assert!(sortkey::sort_key(&[Field { value: v, ty: *ty, desc: false }], &[&int(7)]).is_err());
            json!({"why": why, "fields": [{"value": to_json(v), "type": name(*ty), "desc": false}], "pk": [to_json(&int(7))]})
        }).collect::<Vec<_>>(),
    }))
}

fn row_vectors() -> Result<J> {
    let pk = t("u1");
    let small = Value::map(vec![("id", pk.clone()), ("name", t("Ada"))]);
    let mut padded = Vec::new();
    for n in [0usize, 200, 240, 1000, 5000, 20_000] {
        let f = Value::map(vec![("id", pk.clone()), ("blob", Value::Bytes(vec![7; n]))]);
        let row = db::encode_row(&pk, &f, None, true)?;
        padded.push(if row.len() <= 1024 {
            json!({"blob_len": n, "row_len": row.len(), "row": h(&row)})
        } else {
            json!({"blob_len": n, "row_len": row.len(), "row_digest": h(&db::parts_digest(&row))})
        });
    }
    // Parts, with a small part size so the vector stays readable.
    let big = Value::map(vec![
        ("id", pk.clone()),
        ("text", t(&"lorem ipsum ".repeat(12))),
    ]);
    let fb = enc(&big);
    let part = 64;
    let parts: Vec<String> = fb.chunks(part).map(h).collect();
    let digest = db::parts_digest(&fb);
    let row = db::encode_row(
        &pk,
        &Value::Map(vec![]),
        Some((parts.len() as u32, digest)),
        false,
    )?;
    Ok(json!({
        "description": "Rows (zendb.md §4). `padded`: Row{1: pk, 2: {id, blob: blob_len × 0x07}, 5: pad} of a table with pad: true; its hex when at most 1,024 bytes, else `row_digest` = H(\"zen/v1/db-parts-digest\", Row) as a compact check. `parts` uses a part size of 64 for readability; real parts are max_value_bytes − 1024.",
        "plain": {"pk": to_json(&pk), "fields": to_json(&small), "pk_element": h(&enc(&pk)), "row": h(&db::encode_row(&pk, &small, None, false)?)},
        "padded": padded,
        "parts": {"fields": to_json(&big), "fields_cbor": h(&fb), "part_size": part, "parts": parts, "digest": h(&digest), "row": h(&row)},
        "buckets": BUCKETS.iter().map(|n| json!([n, db::row_bucket(*n)])).collect::<Vec<_>>(),
    }))
}

const BUCKETS: [usize; 10] = [0, 256, 257, 1024, 1025, 4096, 4097, 16384, 16385, 40000];
const GROUPS: [&str; 3] = ["payments", "$index", "audit-view"];

fn entries(range: std::ops::Range<i128>) -> Result<Vec<Vec<u8>>> {
    range
        .map(|i| {
            let v = int(i * 10);
            let pk = t(&format!("r{i}"));
            sortkey::sort_key(
                &[Field {
                    value: &v,
                    ty: Type::Int,
                    desc: false,
                }],
                &[&pk],
            )
        })
        .collect()
}

fn tree_json(tree: &prolly::Tree) -> J {
    json!({
        "nodes": tree.nodes.iter().map(|(id, n)| json!({"id": h(id), "level": n.level, "node": h(&n.encode())})).collect::<Vec<_>>(),
        "root": tree.root.as_ref().map(|r| json!({"root": h(&r.root), "height": r.height, "count": r.count, "record": h(&r.encode())})),
    })
}

fn prolly_vectors(dbk: &DbKeys) -> Result<J> {
    let ix = dbk.index(&INDEX_ID);
    let (kb, kn) = ix.raw();
    let base = entries(0..40)?;
    let tree = prolly::build(&ix, 4, base.clone())?;
    // Insert 3 entries, delete 5: the tree must equal the one built from scratch.
    let mut changed: Vec<Vec<u8>> = base
        .iter()
        .enumerate()
        .filter(|(i, _)| ![3, 4, 17, 30, 39].contains(i))
        .map(|(_, e)| e.clone())
        .collect();
    let extra: Vec<Vec<u8>> = [55i128, 125, 999]
        .iter()
        .map(|i| {
            let v = int(*i);
            sortkey::sort_key(
                &[Field {
                    value: &v,
                    ty: Type::Int,
                    desc: false,
                }],
                &[&t("x")],
            )
        })
        .collect::<Result<_>>()?;
    changed.extend(extra.iter().cloned());
    let tree2 = prolly::build(&ix, 4, changed)?;
    let boundaries: Vec<J> = base
        .iter()
        .take(8)
        .flat_map(|e| (0..2u8).map(move |l| (l, e)))
        .map(|(l, e)| json!({"level": l, "key": h(e), "fanout": 4, "boundary": ix.is_boundary(l, e, 4)}))
        .collect();
    let shards: Vec<J> = ["a", "b", "c", "d", "e"]
        .iter()
        .map(|s| {
            let pk = enc(&t(s));
            json!({"pk_element": h(&pk), "shards": 4, "shard": ix.shard(&pk, 4)})
        })
        .collect();
    Ok(json!({
        "description": "A private index (zendb.md §5.4) of the fixture database, index_id 22…22, fanout 4. Entries are sort keys of an int field (10·i) with pk text \"r<i>\". `after` deletes entries 3, 4, 17, 30, 39 and inserts the three `inserted` keys; it equals a fresh build.",
        "index_id": h(&INDEX_ID),
        "k_boundary": h(&kb), "k_node": h(&kn),
        "boundaries": boundaries,
        "shards": shards,
        "entries": base.iter().map(|e| h(e)).collect::<Vec<_>>(),
        "tree": tree_json(&tree),
        "inserted": extra.iter().map(|e| h(e)).collect::<Vec<_>>(),
        "deleted": [3, 4, 17, 30, 39],
        "after": tree_json(&tree2),
    }))
}

fn sealed_index_vectors(dbk: &DbKeys) -> Result<J> {
    let es = entries(0..12)?;
    let blob = Value::Map(vec![(
        int(1),
        Value::Array(es.iter().map(|e| Value::Bytes(e.clone())).collect()),
    )]);
    let b = enc(&blob);
    let part = 64;
    let n = b.len().div_ceil(part).next_power_of_two();
    let mut padded = b.clone();
    padded.resize(n * part, 0);
    let digest = db::parts_digest(&b);
    let head = Value::Map(vec![
        (int(1), int(n as i128)),
        (int(2), Value::Bytes(digest.to_vec())),
        (int(3), int(es.len() as i128)),
    ]);
    Ok(json!({
        "description": "A sealed index (zendb.md §5.7) with a part size of 64 for readability: the blob, its parts (rounded up to a power of two and zero-padded), and the head.",
        "head_key": h(&dbk.key(&[b"s".as_ref(), &INDEX_ID])),
        "part_key_1": h(&dbk.key(&[b"s".as_ref(), &INDEX_ID, &1u32.to_be_bytes()])),
        "blob": h(&b),
        "part_size": part,
        "parts": padded.chunks(part).map(h).collect::<Vec<_>>(),
        "head": h(&enc(&head)),
    }))
}

fn crdt_vectors(dbk: &DbKeys) -> Result<J> {
    let fs = fixture_fs();
    let ck = dbk.crdt(&TABLE_ID);
    let pk = enc(&t("card-1"));
    let object = dbk.key(&[b"t".as_ref(), &TABLE_ID, &pk]);
    let title = ck.field("title");
    let labels_f = ck.field("labels");
    let red = enc(&t("red"));
    let red_tok = ck.elem("labels", &pk, &red);
    let zero = [0u8; 16];
    let seal = |field: &[u8; 16], elem: &[u8; 16], v: &Value, seed: &str| -> Result<J> {
        let pt = enc(v);
        let s = db::seal_crdt_value(&fs, &object, field, elem, &pt, &mut DetRng::new(seed))?;
        Ok(
            json!({"field": h(field), "elem": h(elem), "plaintext": h(&pt), "rng_seed": seed, "sealed": h(&s)}),
        )
    };
    Ok(json!({
        "description": "CRDT tables (zendb.md §19.2): K_crdt and tokens for table_id 11…11, and kind-7 values sealed for the object of pk \"card-1\".",
        "table_id": h(&TABLE_ID),
        "k_crdt": h(&ck.raw()),
        "pk_element": h(&pk),
        "object": h(&object),
        "fields": {"title": h(&title), "labels": h(&labels_f), "votes": h(&ck.field("votes"))},
        "elem": {"field": "labels", "element": to_json(&t("red")), "element_cbor": h(&red), "token": h(&red_tok)},
        "values": {
            "row": seal(&zero, &zero, &Value::Map(vec![(int(1), t("card-1"))]), "crdt-row")?,
            "lww": seal(&title, &zero, &Value::Map(vec![(int(1), t("Buy milk"))]), "crdt-lww")?,
            "counter": seal(&ck.field("votes"), &zero, &Value::Map(vec![(int(1), int(3))]), "crdt-ctr")?,
            "set": seal(&labels_f, &red_tok, &Value::Map(vec![(int(1), t("red"))]), "crdt-set")?,
        },
    }))
}

fn msg_vectors() -> Result<J> {
    let fs = fixture_fs();
    let topic = TopicKeys::new(&fs, &["shop", "pay"]);
    let msg = Value::Map(vec![
        (int(1), t("order.paid")),
        (int(2), Value::Bytes(vec![0x33; 16])),
        (int(3), Value::Bytes(vec![0x44; 16])),
        (
            int(4),
            Value::Array(vec![
                Value::Bytes(b"zen".to_vec()),
                Value::Bytes(b"inbox".to_vec()),
                Value::Bytes(vec![0x55; 16]),
            ]),
        ),
        (int(8), Value::map(vec![("trace", t("t-1"))])),
        (
            int(9),
            Value::map(vec![("order", t("o-7")), ("amount", int(1250))]),
        ),
    ]);
    let body = Value::map(vec![("photo", Value::Bytes(vec![9; 100]))]);
    let body_ref = Value::Map(vec![
        (int(1), t(NS)),
        (int(2), int(2)),
        (int(3), Value::Bytes(db::parts_digest(&enc(&body)).to_vec())),
    ]);
    let offset: [u8; 12] = [0, 0, 0, 0, 0, 0, 0x30, 0x39, 0, 0, 0, 1];
    let mut event_id = topic.id().to_vec();
    event_id.extend_from_slice(&offset);
    Ok(json!({
        "description": "Broker encodings (zendb.md §10, §11.3): a Msg, a BodyRef, an event id, and consumer-group ids on topic (\"shop\", \"pay\").",
        "msg": {"value": to_json(&msg), "cbor": h(&enc(&msg))},
        "body_ref": {"body": to_json(&body), "value": to_json(&body_ref), "cbor": h(&enc(&body_ref))},
        "event_id": {"topic_id": h(topic.id()), "offset": h(&offset), "id": h(&event_id)},
        "group_ids": GROUPS.iter().map(|g| json!({"name": g, "id": h(&topic.group_id(g.as_bytes()))})).collect::<Vec<_>>(),
    }))
}

/// Every zen-db vector, as the content of `zendb.json`.
pub fn zendb_vectors() -> Result<J> {
    let fs = fixture_fs();
    let dbk = DbKeys::new(&fs, NS);
    let pk = enc(&t("u1"));
    if dbk.prefix().len() != 48 {
        return Err(Error::Format);
    }
    Ok(json!({
        "description": "zen-db vectors (spec/zendb.md §17, §19.9) for the fixture fs (keys.json) and the database ns \"app\". Hex is lowercase.",
        "database": {
            "ns": NS,
            "k_db": h(&dbk.raw_k_db()),
            "prefix": h(dbk.prefix()),
            "keys": [
                {"elements": ["cat", "db"], "key": h(&dbk.key(&[b"cat".as_ref(), b"db"]))},
                {"elements": ["t", h(&TABLE_ID), h(&pk)], "note": "table id and pk element as raw bytes", "key": h(&dbk.key(&[b"t".as_ref(), &TABLE_ID, &pk]))},
                {"elements": ["cat", "m", "u64(3)"], "key": h(&dbk.key(&[b"cat".as_ref(), b"m", &3u64.to_be_bytes()]))},
            ],
        },
        "cbor": cbor_vectors(),
        "sort_keys": sort_key_vectors()?,
        "rows": row_vectors()?,
        "prolly": prolly_vectors(&dbk)?,
        "sealed_index": sealed_index_vectors(&dbk)?,
        "crdt": crdt_vectors(&dbk)?,
        "broker": msg_vectors()?,
    }))
}
