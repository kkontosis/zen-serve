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
