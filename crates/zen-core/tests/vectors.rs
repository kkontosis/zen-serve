//! The committed vectors in spec/test-vectors must be reproduced byte-exactly,
//! and must also open/verify when read back from the files alone.

use serde_json::Value;
use zen_core::keys::FsKeys;
use zen_core::keyslot::{self, Unlock};
use zen_core::sig::{PublicIdentity, verify_device_cert};
use zen_core::token::TopicKeys;
use zen_core::{labels, seal, vectors};

fn dir() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../spec/test-vectors")
}

fn load(name: &str) -> Value {
    let text = std::fs::read_to_string(dir().join(name)).unwrap_or_else(|e| panic!("{name}: {e}"));
    serde_json::from_str(&text).unwrap()
}

fn hx(v: &Value) -> Vec<u8> {
    hex::decode(v.as_str().expect("hex string")).expect("valid hex")
}

#[test]
fn committed_vectors_match_generator() {
    for (name, value) in vectors::generate().unwrap() {
        assert_eq!(
            load(name),
            value,
            "{name} is stale: run `cargo run -p zen-core --example gen_vectors --features test-utils`"
        );
    }
}

fn fs_from_file() -> FsKeys {
    FsKeys::from_bundle(&hx(&load("keys.json")["fs_epoch0"]["bundle"])).unwrap()
}

#[test]
fn sealed_vectors_open_from_files() {
    let fs = fs_from_file();
    let s = load("seal.json");
    let v = &s["kv_value"];
    assert_eq!(
        seal::open_value(&fs, &hx(&v["stored_key"]), &hx(&v["sealed"])).unwrap(),
        hx(&v["plaintext"])
    );

    let e = &s["event"];
    let topic = TopicKeys::new(&fs, &["chat", "alice-bob"]);
    assert_eq!(topic.id(), hx(&e["topic_id"]));
    let token: [u8; 16] = hx(&e["key_token"]).try_into().unwrap();
    let body = seal::open_event(&fs, &topic, Some(&token), &hx(&e["sealed"])).unwrap();
    assert_eq!(body.encode(), hx(&e["body"]));
}

#[test]
fn epoch_chain_vector_recovers_epoch0() {
    let k = load("keys.json");
    let next = FsKeys::from_bundle(&hx(&k["fs_epoch1"]["bundle"])).unwrap();
    let prev = next.previous(&hx(&k["epoch_chain_record"])).unwrap();
    assert_eq!(*prev.to_bundle(), hx(&k["fs_epoch0"]["bundle"]));
}

#[test]
fn keyslot_vectors_open_from_files() {
    let k = load("keyslots.json");
    let bundle = hx(&load("keys.json")["fs_epoch0"]["bundle"]);
    let pw = k["passphrase"]["passphrase"].as_str().unwrap().as_bytes();
    let fs = keyslot::open(&hx(&k["passphrase"]["slot"]), Unlock::Passphrase(pw)).unwrap();
    assert_eq!(*fs.to_bundle(), bundle);
    let rk: [u8; 32] = hx(&k["recovery"]["recovery_key"]).try_into().unwrap();
    let fs = keyslot::open(&hx(&k["recovery"]["slot"]), Unlock::Recovery(&rk)).unwrap();
    assert_eq!(*fs.to_bundle(), bundle);
    let dev = vectors::fixture_device();
    assert_eq!(dev.public().encode(), hx(&k["device"]["device_public"]));
    let fs = keyslot::open(&hx(&k["device"]["slot"]), Unlock::Device(&dev)).unwrap();
    assert_eq!(*fs.to_bundle(), bundle);
}

#[test]
fn prf_keyslot_vector_opens_from_file() {
    let v = &load("prf_keyslot.json")["webauthn_prf"];
    let bundle = hx(&load("keys.json")["fs_epoch0"]["bundle"]);
    let slot = hx(&v["slot"]);
    let (cred, salt) = keyslot::webauthn_prf_params(&slot).unwrap();
    assert_eq!(
        (cred.to_vec(), salt.to_vec()),
        (hx(&v["credential_id"]), hx(&v["prf_salt"]))
    );
    let out: [u8; 32] = hx(&v["prf_output"]).try_into().unwrap();
    let fs = keyslot::open(&slot, Unlock::WebAuthnPrf(&out)).unwrap();
    assert_eq!(*fs.to_bundle(), bundle);
    assert_eq!(slot.len(), 196);
}

#[test]
fn opaque_keyslot_vector_opens_from_file() {
    let v = &load("opaque_keyslot.json")["opaque_export"];
    let bundle = hx(&load("keys.json")["fs_epoch0"]["bundle"]);
    let slot = hx(&v["slot"]);
    assert_eq!(
        keyslot::opaque_export_credential(&slot).unwrap().to_vec(),
        hx(&v["credential_id"])
    );
    let key: [u8; 64] = hx(&v["export_key"]).try_into().unwrap();
    assert_eq!(
        blake3::derive_key(labels::OPAQUE_KEYSLOT, &key).to_vec(),
        hx(&v["secret"])
    );
    let fs = keyslot::open(&slot, Unlock::OpaqueExport(&key)).unwrap();
    assert_eq!(*fs.to_bundle(), bundle);
    assert_eq!(slot.len(), 164);
}

#[test]
fn signature_vectors_verify_from_files() {
    let s = load("signatures.json");
    let user = PublicIdentity::decode(&hx(&s["user"]["public"])).unwrap();
    assert_eq!(user.fingerprint().to_vec(), hx(&s["user"]["fingerprint"]));
    let sig = &s["signature"];
    user.verify(
        labels::SIG_COMMIT,
        &hx(&sig["message"]),
        &hx(&sig["signature"]),
    )
    .unwrap();
    let (device, created) = verify_device_cert(&user, &hx(&s["device_cert"]["cert"])).unwrap();
    assert_eq!(
        device.fingerprint().to_vec(),
        hx(&s["device_cert"]["device_fingerprint"])
    );
    assert_eq!(created, 1_790_000_000);
}

#[test]
fn fs_vectors_open_from_files() {
    use zen_core::fs;
    let fsk = fs_from_file();
    let v = load("fs.json");
    let tree: [u8; 16] = hx(&v["tree"]).try_into().unwrap();
    let node: [u8; 16] = hx(&v["node"]).try_into().unwrap();
    assert_eq!(fsk.fs_data_key().to_vec(), hx(&v["fs_data_key"]));
    let meta = fs::open_meta(&fsk, &tree, &node, &hx(&v["meta"]["sealed"])).unwrap();
    assert_eq!(meta, vectors::fixture_meta());
    assert_eq!(meta.encode().unwrap(), hx(&v["meta"]["plaintext"]));
    // Bound to its node: another node id fails.
    assert!(fs::open_meta(&fsk, &tree, &[0x23; 16], &hx(&v["meta"]["sealed"])).is_err());
    let m = fs::open_manifest(&fsk, &tree, &node, &hx(&v["manifest"]["sealed"])).unwrap();
    assert_eq!(m.encode(), hx(&v["manifest"]["plaintext"]));
    assert_eq!(m.size, 70_000);
    let cid: [u8; 16] = hx(&v["chunk"]["id"]).try_into().unwrap();
    assert_eq!(
        fs::open_chunk(&fsk, &cid, &hx(&v["chunk"]["sealed"])).unwrap(),
        hx(&v["chunk"]["plaintext"])
    );
    // Kinds are not interchangeable.
    assert!(fs::open_manifest(&fsk, &tree, &node, &hx(&v["meta"]["sealed"])).is_err());
    let device: [u8; 32] = hx(&v["device"]).try_into().unwrap();
    let mut chain = [0u8; 32];
    for step in v["op_chain"]["steps"].as_array().unwrap() {
        chain = fs::chain_next(&chain, &hx(&step["op"]), &device);
        assert_eq!(chain.to_vec(), hx(&step["chain"]));
    }
    let hlc = v["hlc"]["hlc"].as_u64().unwrap();
    assert_eq!(fs::hlc_ms(hlc), v["hlc"]["unix_ms"].as_u64().unwrap());
    // Version 2: pages, manifests, a chunk under the file key, a paged write.
    let fk: [u8; 32] = hx(&v["file_key"]["key"]).try_into().unwrap();
    assert_eq!(
        fs::file_chunk_key(fsk.fs_id, &fk).to_vec(),
        hx(&v["file_key"]["chunk_key"])
    );
    let mut page_ids = Vec::new();
    for p in v["pages"].as_array().unwrap() {
        let page = hx(&p["page"]);
        assert_eq!(fs::page_digest(&page).to_vec(), hx(&p["digest"]));
        fs::decode_page(&page).unwrap();
        page_ids.push(<[u8; 16]>::try_from(hx(&p["id"])).unwrap());
    }
    for name in ["manifest_v2_inline", "manifest_v2_paged"] {
        let m = fs::open_any_manifest(&fsk, &tree, &node, &hx(&v[name]["sealed"])).unwrap();
        assert_eq!(m.encode(), hx(&v[name]["plaintext"]));
        let fs::AnyManifest::V2(m) = m else {
            panic!("version 2")
        };
        assert_eq!(
            m.file_key,
            Some(fs::FileKey {
                key_epoch: 0,
                key: fk
            })
        );
    }
    let m = fs::open_any_manifest(&fsk, &tree, &node, &hx(&v["manifest_v2_fs_keys"]["sealed"]))
        .unwrap();
    assert_eq!(m.encode(), hx(&v["manifest_v2_fs_keys"]["plaintext"]));
    let fs::AnyManifest::V2(m) = m else {
        panic!("version 2")
    };
    assert_eq!(m.file_key, None);
    assert_eq!(
        fs::open_file_chunk(fsk.fs_id, &fk, 0, &cid, &hx(&v["file_chunk"]["sealed"])).unwrap(),
        hx(&v["file_chunk"]["plaintext"])
    );
    let paged = hx(&v["write_paged"]["op"]);
    assert_eq!(paged[0], 4);
    let paged_chain = fs::chain_next(&chain, &paged, &device);
    assert_eq!(paged_chain.to_vec(), hx(&v["write_paged"]["chain"]));
    let nodes: Vec<[u8; 16]> = v["purge"]["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| hx(n).try_into().unwrap())
        .collect();
    let purge = fs::purge_bytes(fsk.fs_id, &tree, &nodes);
    assert_eq!(purge, hx(&v["purge"]["op"]));
    assert_eq!(purge.len(), 25 + 16 * nodes.len());
    assert_eq!(
        fs::chain_next(&paged_chain, &purge, &fs::PURGE_DEVICE).to_vec(),
        hx(&v["purge"]["chain"])
    );
    // Version 3: manifests with chunk lengths, form-2 pages, the chunker.
    for (i, p) in v["pages_v2"].as_array().unwrap().iter().enumerate() {
        let page = hx(&p["page"]);
        assert_eq!(fs::page_digest(&page).to_vec(), hx(&p["digest"]));
        let any = fs::decode_any_page(&page).unwrap();
        assert_eq!(any.ids, vec![vectors::fixture_fs_ids().2[i]]);
        assert_eq!(any.lens, Some(vec![[40_000, 30_000][i]]));
        assert!(fs::decode_page(&page).is_err(), "form 1 refuses form 2");
    }
    for name in ["manifest_v3_inline", "manifest_v3_paged"] {
        let m = fs::open_any_manifest(&fsk, &tree, &node, &hx(&v[name]["sealed"])).unwrap();
        assert_eq!(m.encode(), hx(&v[name]["plaintext"]));
        let fs::AnyManifest::V3(m) = m else {
            panic!("version 3")
        };
        assert_eq!((m.size, m.n), (70_000, 2));
    }
    let c = &v["cdc"];
    let params = fs::CdcParams {
        min: c["params"]["min"].as_u64().unwrap() as u32,
        avg_bits: c["params"]["avg_bits"].as_u64().unwrap() as u8,
        max: c["params"]["max"].as_u64().unwrap() as u32,
    };
    assert_eq!(fs::CdcParams::default_for(90_000), Some(params));
    let fs_key = zen_core::cdc::fs_cdc_key(&fsk);
    assert_eq!(fs_key.to_vec(), hx(&c["fs_key"]));
    let file_key = zen_core::cdc::file_cdc_key(fsk.fs_id, &fk);
    assert_eq!(file_key.to_vec(), hx(&c["file_key"]));
    let gear = zen_core::cdc::gear_table(&fs_key);
    for (i, g) in c["gear_first"].as_array().unwrap().iter().enumerate() {
        assert_eq!(gear[i].to_string(), g.as_str().unwrap());
    }
    let stream = |x: &Value| {
        vectors::xorshift_bytes(
            x["xorshift32_seed"].as_u64().unwrap() as u32,
            x["len"].as_u64().unwrap() as usize,
        )
    };
    let input = stream(&c["input"]);
    let ins = stream(&c["insert"]);
    let at = c["insert"]["at"].as_u64().unwrap() as usize;
    let mut edited = input.clone();
    edited.splice(at..at, ins.iter().copied());
    let lens = |key: &[u8; 32], d: &[u8]| -> Vec<u64> {
        zen_core::cdc::Chunker::new(key, params)
            .unwrap()
            .lengths(d)
            .into_iter()
            .map(u64::from)
            .collect()
    };
    let nums = |x: &Value| -> Vec<u64> {
        x.as_array()
            .unwrap()
            .iter()
            .map(|n| n.as_u64().unwrap())
            .collect()
    };
    let before = nums(&c["lengths"]);
    let after = nums(&c["lengths_after_insert"]);
    assert_eq!(lens(&fs_key, &input), before);
    assert_eq!(lens(&fs_key, &edited), after);
    assert_eq!(lens(&file_key, &input), nums(&c["lengths_file_key"]));
    // Only the chunks around the insertion differ: the rest are the same chunks.
    let ends = |l: &[u64], shift_from: u64, by: u64| -> std::collections::HashSet<(u64, u64)> {
        let mut out = std::collections::HashSet::new();
        let mut off = 0;
        for &x in l {
            let start = if off >= shift_from { off - by } else { off };
            out.insert((start, x));
            off += x;
        }
        out
    };
    let a = ends(&before, u64::MAX, 0);
    let b = ends(&after, at as u64 + ins.len() as u64, ins.len() as u64);
    assert!(
        b.difference(&a).count() <= 2,
        "{} chunks changed",
        b.difference(&a).count()
    );
}

#[test]
fn hlc_clock_rules() {
    use zen_core::fs::{Clock, hlc};
    let mut c = Clock::default();
    let a = c.tick(1000);
    assert_eq!(a, hlc(1000, 0));
    let b = c.tick(1000);
    assert_eq!(b, hlc(1000, 1));
    c.observe(hlc(5000, 3));
    assert_eq!(c.tick(1001), hlc(5000, 4));
    assert!(c.tick(6000) > hlc(5000, 4));
}

#[test]
fn pwkey_vectors_verify_from_files() {
    use zen_core::keyslot::Argon2Params;
    use zen_core::pwkey::{self, PasswordKey};
    let v = load("pwkey.json");
    let params = Argon2Params {
        m_cost_kib: v["m_cost_kib"].as_u64().unwrap() as u32,
        t_cost: v["t_cost"].as_u64().unwrap() as u32,
        p_cost: v["p_cost"].as_u64().unwrap() as u32,
    };
    let salt: [u8; 32] = hx(&v["salt"]).try_into().unwrap();
    let key =
        PasswordKey::derive(v["password"].as_str().unwrap().as_bytes(), &salt, params).unwrap();
    assert_eq!(key.public().encode(), hx(&v["public"]));
    let public = PublicIdentity::decode(&hx(&v["public"])).unwrap();
    assert_eq!(public.fingerprint().to_vec(), hx(&v["fingerprint"]));
    let s = &v["session"];
    let origin = s["origin"].as_str().unwrap();
    assert_eq!(
        pwkey::session_message(&hx(&s["challenge"]), origin),
        hx(&s["message"])
    );
    pwkey::verify_session(&public, &hx(&s["challenge"]), origin, &hx(&s["signature"])).unwrap();
    // The purpose separates it from a device's session signature.
    assert!(
        public
            .verify(
                labels::SIG_SESSION,
                &hx(&s["message"]),
                &hx(&s["signature"])
            )
            .is_err()
    );
}

#[test]
fn header_vectors_open_from_files() {
    use zen_core::header::FsHeader;
    let v = load("header.json");
    let fs0 = fs_from_file();
    let h0 = FsHeader::decode(&hx(&v["epoch0"]["header"])).unwrap();
    assert_eq!((h0.fs_id, h0.current_epoch, h0.slots.len()), (7, 0, 1));
    let rk: [u8; 32] = hx(&v["epoch0"]["recovery_key"]).try_into().unwrap();
    let opened = keyslot::open(&h0.slots[0], Unlock::Recovery(&rk)).unwrap();
    assert_eq!(*opened.to_bundle(), *fs0.to_bundle());

    let h1 = FsHeader::decode(&hx(&v["epoch1"]["header"])).unwrap();
    assert_eq!(
        (h1.current_epoch, h1.slots.len(), h1.chain.len()),
        (1, 2, 1)
    );
    let pw = v["epoch1"]["passphrase"].as_str().unwrap().as_bytes();
    let fs1 = keyslot::open(&h1.slots[1], Unlock::Passphrase(pw)).unwrap();
    assert_eq!(
        fs1.to_bundle().to_vec(),
        hx(&v["epoch1"]["fs_epoch1_bundle"])
    );
    assert_eq!(*h1.keys_at(&fs1, 0).unwrap().to_bundle(), *fs0.to_bundle());
    // Re-encoding gives the same bytes.
    assert_eq!(h1.encode().unwrap(), hx(&v["epoch1"]["header"]));
    assert!(h1.policy.is_default());
    // The policy makes a version-2 header.
    let p = FsHeader::decode(&hx(&v["policy"]["header"])).unwrap();
    assert_eq!(hx(&v["policy"]["header"])[0], 2);
    assert!(p.policy.file_keys);
    assert_eq!(
        p.policy.rekey_after_rotate,
        zen_core::header::RekeyAfterRotate::Eager
    );
    assert_eq!(p.policy.encode().unwrap(), hx(&v["policy"]["policy"]));
    assert_eq!(
        (p.slots.clone(), p.chain.clone()),
        (h1.slots.clone(), h1.chain.clone())
    );
    assert_eq!(p.encode().unwrap(), hx(&v["policy"]["header"]));
}

#[test]
fn acl_vector_verifies_from_file() {
    use zen_proto::acl::{AclDoc, SignedAcl};
    let v = load("acl.json");
    let signed: SignedAcl = zen_proto::from_cbor(&hx(&v["signed"])).unwrap();
    assert_eq!(signed.doc, hx(&v["doc"]));
    assert_eq!(
        zen_core::kdf::acl_hash(&signed.doc).to_vec(),
        hx(&v["hash"])
    );
    let doc: AclDoc = zen_proto::from_cbor(&signed.doc).unwrap();
    // Deterministic CBOR: decoding and re-encoding gives the signed bytes.
    assert_eq!(zen_proto::to_cbor(&doc), signed.doc);
    let user = PublicIdentity::decode(&doc.members[0].identity).unwrap();
    assert_eq!(user.fingerprint().to_vec(), signed.signer);
    user.verify(labels::SIG_ACL, &signed.doc, &signed.sig)
        .unwrap();
    verify_device_cert(&user, &doc.members[0].devices[0]).unwrap();
}

#[test]
fn zendb_vectors_read_back() {
    use zen_core::db::{self, DbKeys, cbor};
    use zen_core::db_vectors::from_json;
    let z = load("zendb.json");
    for case in z["cbor"]["ok"].as_array().unwrap() {
        let v = from_json(&case["value"]);
        let b = cbor::encode(&v).unwrap();
        assert_eq!(b, hx(&case["cbor"]));
        assert_eq!(cbor::decode(&b).unwrap(), from_json(&case["decoded"]));
    }
    for case in z["cbor"]["refused_decode"].as_array().unwrap() {
        assert!(cbor::decode(&hx(&case["cbor"])).is_err(), "{}", case["why"]);
    }
    let fs = fs_from_file();
    let dbk = DbKeys::new(&fs, z["database"]["ns"].as_str().unwrap());
    assert_eq!(dbk.prefix(), hx(&z["database"]["prefix"]));
    let c = &z["crdt"];
    let object = hx(&c["object"]);
    for (_, v) in c["values"].as_object().unwrap() {
        let field: [u8; 16] = hx(&v["field"]).try_into().unwrap();
        let elem: [u8; 16] = hx(&v["elem"]).try_into().unwrap();
        let pt = db::open_crdt_value(&fs, &object, &field, &elem, &hx(&v["sealed"])).unwrap();
        assert_eq!(pt, hx(&v["plaintext"]));
        // Bound to the object: another object fails.
        assert!(db::open_crdt_value(&fs, &object[16..], &field, &elem, &hx(&v["sealed"])).is_err());
    }
    for v in z["local"]["records"].as_array().unwrap() {
        let store = v["store"].as_str().unwrap();
        let key = hx(&v["key"]);
        let sealed = hx(&v["sealed"]);
        assert_eq!(
            db::open_local(&fs, store, &key, &sealed).unwrap(),
            hx(&v["plaintext"])
        );
        // Bound to the store and the key.
        assert!(db::open_local(&fs, "other", &key, &sealed).is_err());
        assert!(db::open_local(&fs, store, b"other", &sealed).is_err());
        // Not a CRDT value.
        let zero = [0u8; 16];
        assert!(db::open_crdt_value(&fs, &key, &zero, &zero, &sealed).is_err());
    }
    // The tree after edits equals a fresh build of the edited entry set.
    let p = &z["prolly"];
    let ix = dbk.index(&hx(&p["index_id"]).try_into().unwrap());
    let built = db::prolly::build(
        &ix,
        4,
        p["entries"].as_array().unwrap().iter().map(hx).collect(),
        16,
    )
    .unwrap();
    assert_eq!(
        hex::encode(built.root.unwrap().encode()),
        p["tree"]["root"]["record"].as_str().unwrap()
    );
    // The authenticated tier (§14): entries, the state tree, the NsRoot chain.
    let a = &z["authenticated"];
    let st = dbk.state();
    let (kb, kn) = st.raw();
    assert_eq!(hex::encode(kb), a["k_boundary"].as_str().unwrap());
    assert_eq!(hex::encode(kn), a["k_node"].as_str().unwrap());
    let mut values: Vec<(Vec<u8>, Vec<u8>)> = a["values"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| {
            let (s, p) = (hx(&v["suffix"]), hx(&v["plaintext"]));
            assert_eq!(
                hex::encode(db::auth::value_hash(&p)),
                v["vh"].as_str().unwrap()
            );
            assert_eq!(
                hex::encode(db::auth::state_entry(&s, &p)),
                v["entry"].as_str().unwrap()
            );
            (s, p)
        })
        .collect();
    let tree = db::auth::state_tree(&st, &values, 4).unwrap();
    let nodes = |t: &db::prolly::Tree| -> Vec<(String, String)> {
        t.nodes
            .iter()
            .map(|(id, n)| (hex::encode(id), hex::encode(n.encode())))
            .collect()
    };
    let want = |j: &serde_json::Value| -> Vec<(String, String)> {
        j["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|n| {
                (
                    n["id"].as_str().unwrap().into(),
                    n["node"].as_str().unwrap().into(),
                )
            })
            .collect()
    };
    assert_eq!(nodes(&tree), want(&a["tree"]));
    let r1 = db::auth::NsRoot {
        seq: 1,
        prev: [0; 32],
        tree: db::auth::state_root(&tree),
    };
    assert_eq!(
        hex::encode(r1.encode()),
        a["ns_root"]["encoded"].as_str().unwrap()
    );
    assert_eq!(hex::encode(r1.hash()), a["ns_root"]["nh"].as_str().unwrap());
    values[2].1 = hx(&a["changed"]);
    values.remove(a["deleted"].as_u64().unwrap() as usize);
    let tree2 = db::auth::state_tree(&st, &values, 4).unwrap();
    assert_eq!(nodes(&tree2), want(&a["after"]));
    let enc2 = hx(&a["ns_root_after"]["encoded"]);
    let r2 = db::auth::NsRoot::decode(&enc2).unwrap();
    assert_eq!((r2.seq, r2.prev), (2, r1.hash()));
    assert_eq!(r2.tree, db::auth::state_root(&tree2));
    assert_eq!(
        hex::encode(r2.hash()),
        a["ns_root_after"]["nh"].as_str().unwrap()
    );
}

#[test]
fn prolly_is_history_independent_and_bounded() {
    use zen_core::db::prolly::build;
    let ix = zen_core::db::IndexKeys::from_parts([1; 32], [2; 32]);
    let mut x: u64 = 0x9e37_79b9_7f4a_7c15;
    let mut next = || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x
    };
    for fanout in [2u32, 4, 64] {
        let mut set: Vec<Vec<u8>> = (0..600)
            .map(|_| next().to_be_bytes()[..5].to_vec())
            .collect();
        set.sort();
        set.dedup();
        let a = build(&ix, fanout, set.clone(), 16).unwrap();
        set.reverse();
        let b = build(&ix, fanout, set, 16).unwrap();
        assert_eq!(a.root, b.root);
        assert!(
            a.nodes
                .iter()
                .all(|(_, n)| !n.entries.is_empty() && n.entries.len() <= 1024)
        );
    }
}

#[test]
fn svc_vectors_open_from_files() {
    use zen_core::svc;
    let fs = fs_from_file();
    let v = load("svc.json");
    let topic = TopicKeys::new(&fs, &["svc", "ssh"]);
    assert_eq!(topic.id(), hx(&v["svc"]["id"]));
    let k = svc::SvcKeys::new(&fs, &topic);
    assert_eq!(k.key().as_ref(), &hx(&v["svc"]["k_svc"])[..]);
    let conn: [u8; 16] = hx(&v["conn"]).try_into().unwrap();
    let st = &v["state"];
    let cur = |f: &str| st[f].as_u64().unwrap();
    assert_eq!(
        svc::open_state(
            &k,
            &conn,
            cur("in_acked"),
            cur("out_head"),
            &hx(&st["sealed"])
        )
        .unwrap(),
        hx(&st["plaintext"])
    );

    let r = &v["record"];
    let off = r["off"].as_u64().unwrap();
    let rec = hx(&r["record"]);
    let k_r: [u8; 32] = hx(&r["k_r"]).try_into().unwrap();
    assert_eq!(hx(&r["key_hash"]), svc::key_hash(&k_r));
    assert_eq!(
        svc::reveal(fs.fs_id, k.id(), &conn, off, &rec, &k_r).unwrap(),
        hx(&r["raw"])
    );
    assert_eq!(*svc::open_escrow(&k, &conn, off, &rec).unwrap(), k_r);

    let f = &v["fin_record"];
    let fin = hx(&f["record"]);
    let fin_key: [u8; 32] = hx(&f["k_r"]).try_into().unwrap();
    assert_eq!(svc::parse_record(&fin).unwrap().flags, svc::FIN);
    let fin_off = f["off"].as_u64().unwrap();
    assert!(
        svc::reveal(fs.fs_id, k.id(), &conn, fin_off, &fin, &fin_key)
            .unwrap()
            .is_empty()
    );

    let p = &v["plain_record"];
    let plain = hx(&p["record"]);
    let parsed = svc::parse_record(&plain).unwrap();
    assert_eq!(parsed.body, &hx(&p["raw"])[..]);

    let i = &v["inbox"];
    let seed: [u8; 32] = hx(&i["seed"]).try_into().unwrap();
    assert_eq!(svc::inbox_public(&seed), hx(&i["public"]));
    let key = svc::inbox_decaps(&seed, &hx(&i["ct"]), fs.fs_id, k.id(), &conn).unwrap();
    let mut next = 0;
    for c in i["chunks"].as_array().unwrap() {
        let off = c["off"].as_u64().unwrap();
        assert_eq!(off, next, "chunks are contiguous");
        let data = key.open_chunk(off, &hx(&c["sealed"])).unwrap();
        assert_eq!(data, hx(&c["data"]));
        next = off + data.len() as u64;
    }
}

#[test]
fn integrity_vectors_verify_from_files() {
    use zen_core::fs::{StateNode, TreeState, state_leaf};
    use zen_core::header::{SignedHeader, validate_header_successor};
    use zen_core::integrity::{
        CheckpointBody, checkpoint_hash, open_checkpoint, verify_checkpoint,
    };
    use zen_core::sig::DevicePublic;

    let v = load("integrity.json");
    let fs = fs_from_file();

    // Checkpoint: open, verify with the device's public keys, hash.
    let c = &v["checkpoint"];
    let dev = DevicePublic::decode(&hx(&c["device_public"])).unwrap();
    let fp: [u8; 32] = hx(&c["device_fingerprint"]).try_into().unwrap();
    assert_eq!(dev.fingerprint(), fp);
    let body = CheckpointBody::decode(&hx(&c["body"])).unwrap();
    let signed = open_checkpoint(&fs, &fp, body.seq, &hx(&c["sealed"])).unwrap();
    assert_eq!(signed, hx(&c["signed"]));
    assert_eq!(verify_checkpoint(&dev, &signed).unwrap(), body);
    assert_eq!(checkpoint_hash(&hx(&c["body"])).to_vec(), hx(&c["hash"]));
    assert!(open_checkpoint(&fs, &fp, body.seq + 1, &hx(&c["sealed"])).is_err());

    // Signed header: v1 over the unsigned header, then v2.
    let sh = &v["signed_header"];
    let user = PublicIdentity::decode(&hx(&sh["user_public"])).unwrap();
    let admin = |f: &[u8; 32]| (*f == user.fingerprint()).then(|| user.clone());
    let (plain, v1, v2) = (
        hx(&sh["header"]),
        hx(&sh["v1"]["signed"]),
        hx(&sh["v2"]["signed"]),
    );
    validate_header_successor(fs.fs_id, Some(&plain), &v1, admin).unwrap();
    validate_header_successor(fs.fs_id, Some(&v1), &v2, admin).unwrap();
    let d1 = SignedHeader::decode(&v1).unwrap();
    assert_eq!(d1.msg(), hx(&sh["v1"]["msg"]));
    assert_eq!(d1.hash().to_vec(), hx(&sh["v1"]["hash"]));
    assert_eq!(d1.header, plain);

    // Event chain: each body links to the previous one's hash.
    let mut prev = [0u8; 32];
    for (i, e) in v["event_chain"]["events"]
        .as_array()
        .unwrap()
        .iter()
        .enumerate()
    {
        let bytes = hx(&e["body"]);
        let b = seal::EventBody::decode(&bytes).unwrap();
        let chain = b.chain.unwrap();
        assert_eq!((chain.dseq, chain.dprev), (i as u64 + 1, prev));
        prev = seal::event_chain_hash(&bytes);
        assert_eq!(prev.to_vec(), hx(&e["hash"]));
    }

    // Exported pins: decode to the listed fields, and encode back.
    let p = &v["pins"];
    let pins = zen_core::integrity::ExportedPins::decode(&hx(&p["bytes"])).unwrap();
    assert_eq!(u64::from(pins.fs), p["fs"].as_u64().unwrap());
    for (got, want) in [(&pins.acl, &p["acl"]), (&pins.header, &p["header"])] {
        assert_eq!(got.version, want["version"].as_u64().unwrap());
        assert_eq!(got.hash.to_vec(), hx(&want["hash"]));
    }
    let devs = p["devices"].as_array().unwrap();
    assert_eq!(pins.devices.len(), devs.len());
    for (d, w) in pins.devices.iter().zip(devs) {
        assert_eq!(d.device.to_vec(), hx(&w["device"]));
        assert_eq!(d.seq, w["seq"].as_u64().unwrap());
        assert_eq!(d.hash.to_vec(), hx(&w["hash"]));
    }
    assert_eq!(pins.encode().unwrap(), hx(&p["bytes"]));

    // Tree states from the listed node states.
    let ts = &v["tree_state"];
    let arr = |x: &Value| -> [u8; 32] { hx(x).try_into().unwrap() };
    let nodes: Vec<StateNode> = ts["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| {
            let s = StateNode {
                node: hx(&n["node"]).try_into().unwrap(),
                parent: n["parent"]
                    .as_str()
                    .map(|_| hx(&n["parent"]).try_into().unwrap()),
                move_hlc: n["move_hlc"].as_str().unwrap().parse().unwrap(),
                move_device: arr(&n["move_device"]),
                meta: n["meta"].as_str().map(|_| {
                    (
                        hx(&n["meta"]),
                        n["meta_hlc"].as_str().unwrap().parse().unwrap(),
                        arr(&n["meta_device"]),
                    )
                }),
                versions: n["versions"].as_u64().unwrap() as u32,
                changed: hx(&n["changed"]).try_into().unwrap(),
            };
            assert_eq!(state_leaf(&s), arr(&n["leaf"]));
            s
        })
        .collect();
    let mut t = TreeState::new();
    assert_eq!(t.root(), arr(&ts["empty"]));
    t.set(nodes[0].node, state_leaf(&nodes[0]));
    assert_eq!(t.root(), arr(&ts["first"]));
    for n in &nodes[1..] {
        t.set(n.node, state_leaf(n));
    }
    assert_eq!(t.root(), arr(&ts["all"]));
    t.remove(&nodes[1].node);
    assert_eq!(t.root(), arr(&ts["without_second"]));
}

#[test]
fn blk_vectors_verify_from_files() {
    use std::collections::BTreeMap;
    use zen_core::blk::{self, RootRecord};
    let v = load("blk.json");
    let fs = fs_from_file();
    let keys = blk::VolumeKeys::new(&fs, v["volume"].as_str().unwrap().as_bytes());
    assert_eq!(keys.vol_id(), hx(&v["vol_id"]));
    assert_eq!(keys.root(), hx(&v["root_key"]));
    assert_eq!(keys.lease(), hx(&v["lease"]));
    let blocks = v["blocks"].as_u64().unwrap();
    assert_eq!(u64::from(blk::depth(blocks)), v["depth"].as_u64().unwrap());

    // Each block opens under its key, and its leaf is over the sealed bytes.
    let mut leaves = BTreeMap::new();
    for b in v["stored_blocks"].as_array().unwrap() {
        let i = b["index"].as_u64().unwrap();
        assert_eq!(keys.block(i), hx(&b["key"]));
        let sealed = hx(&b["sealed"]);
        assert_eq!(
            seal::open_value(&fs, &keys.block(i), &sealed).unwrap(),
            hx(&b["plaintext"])
        );
        assert!(seal::open_value(&fs, &keys.block(i + 1), &sealed).is_err());
        let l = blk::leaf(i, &sealed);
        assert_eq!(l.to_vec(), hx(&b["leaf"]));
        leaves.insert(i, l);
    }
    // The nodes, rebuilt from their listed slots, hash as listed and form the tree.
    let t = blk::tree(blocks, &leaves);
    let nodes = v["nodes"].as_array().unwrap();
    assert_eq!(nodes.len(), t.nodes.len());
    for n in nodes {
        let (l, j) = (
            n["level"].as_u64().unwrap() as u8,
            n["index"].as_u64().unwrap(),
        );
        assert_eq!(keys.node(l, j), hx(&n["key"]));
        let mut node = [blk::ZERO; blk::FANOUT];
        for s in n["slots"].as_array().unwrap() {
            node[s[0].as_u64().unwrap() as usize].copy_from_slice(&hx(&s[1]));
        }
        assert_eq!(blk::node_hash(l, j, &node).to_vec(), hx(&n["hash"]));
        assert_eq!(t.nodes[&(l, j)], node);
    }
    assert_eq!(t.root.to_vec(), hx(&v["root"]));

    // Root records: decode to the listed fields, hash, and chain.
    let mut prev = [0u8; 32];
    for r in v["records"].as_array().unwrap() {
        let rec = RootRecord::decode(&hx(&r["cbor"])).unwrap();
        assert_eq!(rec.seq, r["seq"].as_u64().unwrap());
        assert_eq!(rec.prev, prev);
        assert_eq!(rec.blocks, blocks);
        assert_eq!(rec.root.to_vec(), hx(&r["root"]));
        assert_eq!(rec.writer.to_vec(), hx(&r["writer"]));
        assert_eq!(rec.hlc.to_string(), r["hlc"].as_str().unwrap());
        assert_eq!(rec.hash().to_vec(), hx(&r["hash"]));
        prev = rec.hash();
    }
    assert_eq!(prev.to_vec(), hx(&v["records"][1]["hash"]));
    assert_eq!(v["records"][1]["root"], v["root"]);
}
