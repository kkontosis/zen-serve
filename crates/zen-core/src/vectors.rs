//! Deterministic test-vector generator (feature `test-utils`).
//!
//! `cargo run -p zen-core --example gen_vectors --features test-utils` writes
//! `spec/test-vectors/*.json`; `tests/vectors.rs` asserts the files match.

use crate::kdf::{fingerprint, kdf, prf16};
use crate::keys::FsKeys;
use crate::keyslot::{self, Argon2Params, Unlock};
use crate::pwkey::{self, PasswordKey};
use crate::rng::DetRng;
use crate::seal::{self, EventBody};
use crate::sig::{self, DeviceSecret, SigningIdentity};
use crate::token::{NameChain, TopicKeys, kv_key};
use crate::{Result, fs, labels};
use serde_json::{Value, json};

fn h(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// The fixture fs used by all vectors: fs_id 7, keys from seed "fs".
pub fn fixture_fs() -> FsKeys {
    FsKeys::generate(7, &mut DetRng::new("fs")).expect("det rng")
}

/// Generate every vector file as `(file name, JSON)`.
pub fn generate() -> Result<Vec<(&'static str, Value)>> {
    Ok(vec![
        ("kdf.json", kdf_vectors()),
        ("keys.json", key_vectors()?),
        ("tokens.json", token_vectors()),
        ("seal.json", seal_vectors()?),
        ("keyslots.json", keyslot_vectors()?),
        ("signatures.json", signature_vectors()?),
        ("fs.json", fs_vectors()?),
        ("pwkey.json", pwkey_vectors()?),
        ("prf_keyslot.json", prf_keyslot_vectors()?),
        ("opaque_keyslot.json", opaque_keyslot_vectors()?),
    ])
}

fn kdf_vectors() -> Value {
    let key: [u8; 32] = core::array::from_fn(|i| i as u8);
    json!({
        "description": "KDF(label,key,info)=BLAKE3.derive_key(label,key||info); PRF16=BLAKE3.keyed_hash(key,data)[0..16]; FP=BLAKE3.derive_key(\"zen/v1/fingerprint\",data)",
        "kdf": [
            {"label": labels::KV_DATA, "key": h(&key), "info": "", "out": h(kdf(labels::KV_DATA, &key, b"").as_ref())},
            {"label": labels::NAME_CHAIN, "key": h(&key), "info": h(b"abc"), "out": h(kdf(labels::NAME_CHAIN, &key, b"abc").as_ref())},
        ],
        "prf16": [{"key": h(&key), "data": h(b"zen"), "out": h(&prf16(&key, b"zen"))}],
        "fingerprint": [{"data": h(b"zen"), "out": h(&fingerprint(b"zen"))}],
    })
}

fn fs_json(fs: &FsKeys) -> Value {
    let b = fs.to_bundle();
    json!({
        "fs_id": fs.fs_id, "epoch": fs.epoch,
        "naming_key": h(&b[8..40]), "epoch_key": h(&b[40..72]), "bundle": h(&b),
        "kv_data_key": h(fs.kv_data_key().as_ref()),
        "kv_name_root": h(fs.kv_name_root().as_ref()),
        "topic_name_root": h(fs.topic_name_root().as_ref()),
        "topic_data_root": h(fs.topic_data_root().as_ref()),
    })
}

fn key_vectors() -> Result<Value> {
    let fs = fixture_fs();
    let (next, record) = fs.rotate(&mut DetRng::new("rotate"))?;
    Ok(json!({
        "description": "Fixture fs (fs_id 7, epoch 0) and its rotation to epoch 1. The chain record seals epoch 0's epoch_key under epoch 1.",
        "fs_epoch0": fs_json(&fs),
        "fs_epoch1": fs_json(&next),
        "epoch_chain_record": h(&record),
    }))
}

fn token_vectors() -> Value {
    let fs = fixture_fs();
    let tuple: [&[u8]; 3] = [b"app", b"users", b"42"];
    let prefixes: Vec<Value> = (0..=tuple.len())
        .map(|n| json!({"elements": tuple[..n].iter().map(|e| String::from_utf8_lossy(e)).collect::<Vec<_>>(), "token": h(&kv_key(&fs, &tuple[..n]))}))
        .collect();
    let chain = NameChain::new(fs.kv_name_root()).child(b"app");
    let topic = TopicKeys::new(&fs, &["chat", "alice-bob"]);
    let parent = TopicKeys::new(&fs, &["chat"]);
    json!({
        "description": "Hierarchical tokens of fixture fs. Elements are UTF-8 strings.",
        "kv": prefixes,
        "kv_chain_key_after_app": h(chain.key().as_ref()),
        "topics": [
            {"segments": ["chat"], "id": h(parent.id()), "event_aead_key": h(parent.event_aead_key().as_ref())},
            {"segments": ["chat", "alice-bob"], "id": h(topic.id()), "event_aead_key": h(topic.event_aead_key().as_ref()),
             "event_key_token": {"key": "cust-42", "token": h(&topic.event_key_token(b"cust-42"))}},
        ],
    })
}

/// Fixture event body.
pub fn fixture_event() -> EventBody {
    EventBody {
        sender: [0xAB; 32],
        hlc: 0x0001_0203_0405_0607,
        causation: b"evt-1".to_vec(),
        payload: b"hello family".to_vec(),
    }
}

fn seal_vectors() -> Result<Value> {
    let fs = fixture_fs();
    let stored_key = kv_key(&fs, &["app", "users", "42"]);
    let value = seal::seal_value(
        &fs,
        &stored_key,
        b"{\"name\":\"Ada\"}",
        &mut DetRng::new("value"),
    )?;
    let topic = TopicKeys::new(&fs, &["chat", "alice-bob"]);
    let key_token = topic.event_key_token(b"cust-42");
    let body = fixture_event();
    let event = seal::seal_event(
        &fs,
        &topic,
        Some(&key_token),
        &body,
        &mut DetRng::new("event"),
    )?;
    Ok(json!({
        "description": "Sealed objects for the fixture fs at epoch 0. Nonces come from DetRng(seed).",
        "kv_value": {"rng_seed": "value", "stored_key": h(&stored_key), "plaintext": h(b"{\"name\":\"Ada\"}"), "sealed": h(&value)},
        "event": {"rng_seed": "event", "topic_id": h(topic.id()), "key_token": h(&key_token),
                  "body": h(&body.encode()), "sealed": h(&event)},
    }))
}

/// Passphrase used in the keyslot vectors.
pub const FIXTURE_PASSPHRASE: &[u8] = b"correct horse battery staple";
/// Argon2id parameters used in the keyslot vectors (the creation floor).
pub const FIXTURE_ARGON2: Argon2Params = Argon2Params {
    m_cost_kib: Argon2Params::MIN_M_COST_KIB,
    t_cost: 1,
    p_cost: 1,
};

/// The fixture device, from seed "device".
pub fn fixture_device() -> DeviceSecret {
    DeviceSecret::generate(&mut DetRng::new("device"))
        .expect("det rng")
        .0
}

fn keyslot_vectors() -> Result<Value> {
    let fs = fixture_fs();
    let pass = keyslot::create_passphrase(
        &fs,
        FIXTURE_PASSPHRASE,
        FIXTURE_ARGON2,
        &mut DetRng::new("slot-pass"),
    )?;
    let (rec, rkey) = keyslot::create_recovery(&fs, &mut DetRng::new("slot-recovery"))?;
    let dev = fixture_device();
    let devslot = keyslot::create_device(&fs, &dev.public(), &mut DetRng::new("slot-device"))?;
    // Sanity: every slot opens to the same bundle.
    for (slot, unlock) in [
        (&pass, Unlock::Passphrase(FIXTURE_PASSPHRASE)),
        (&rec, Unlock::Recovery(&rkey)),
        (&devslot, Unlock::Device(&dev)),
    ] {
        assert_eq!(*keyslot::open(slot, unlock)?.to_bundle(), *fs.to_bundle());
    }
    Ok(json!({
        "description": "Keyslots wrapping the fixture fs bundle (keys.json fs_epoch0.bundle).",
        "passphrase": {"rng_seed": "slot-pass", "passphrase": String::from_utf8_lossy(FIXTURE_PASSPHRASE),
                       "m_cost_kib": FIXTURE_ARGON2.m_cost_kib, "t_cost": FIXTURE_ARGON2.t_cost, "p_cost": FIXTURE_ARGON2.p_cost,
                       "slot": h(&pass)},
        "recovery": {"rng_seed": "slot-recovery", "recovery_key": h(rkey.as_ref()), "slot": h(&rec)},
        "device": {"rng_seed": "slot-device", "device_rng_seed": "device",
                   "device_public": h(&dev.public().encode()), "slot": h(&devslot)},
    }))
}

/// The credential id, PRF salt and PRF output of the WebAuthn PRF keyslot
/// vector. The output stands in for an authenticator's.
pub fn fixture_prf() -> ([u8; 32], [u8; 32], [u8; 32]) {
    ([0xCD; 32], core::array::from_fn(|i| i as u8), [0x5A; 32])
}

fn prf_keyslot_vectors() -> Result<Value> {
    let fs = fixture_fs();
    let (cred, salt, out) = fixture_prf();
    let slot = keyslot::create_webauthn_prf(&fs, &cred, &salt, &out, &mut DetRng::new("slot-prf"))?;
    assert_eq!(
        *keyslot::open(&slot, Unlock::WebAuthnPrf(&out))?.to_bundle(),
        *fs.to_bundle()
    );
    assert_eq!(keyslot::webauthn_prf_params(&slot)?, (cred, salt));
    Ok(json!({
        "description": "A WebAuthn PRF keyslot (type 4) wrapping the fixture fs bundle (keys.json fs_epoch0.bundle). prf_output stands in for the authenticator's PRF result for prf_salt.",
        "webauthn_prf": {"rng_seed": "slot-prf", "credential_id": h(&cred), "prf_salt": h(&salt),
                         "prf_output": h(&out), "slot": h(&slot)},
    }))
}

/// The credential id and export key of the OPAQUE keyslot vector. The
/// key stands in for an OPAQUE client's export key.
pub fn fixture_opaque_export() -> ([u8; 32], [u8; keyslot::OPAQUE_EXPORT_KEY_LEN]) {
    (
        [0xE3; 32],
        core::array::from_fn(|i| (i as u8).wrapping_mul(7)),
    )
}

fn opaque_keyslot_vectors() -> Result<Value> {
    let fs = fixture_fs();
    let (cred, export_key) = fixture_opaque_export();
    let slot =
        keyslot::create_opaque_export(&fs, &cred, &export_key, &mut DetRng::new("slot-opaque"))?;
    assert_eq!(
        *keyslot::open(&slot, Unlock::OpaqueExport(&export_key))?.to_bundle(),
        *fs.to_bundle()
    );
    assert_eq!(keyslot::opaque_export_credential(&slot)?, cred);
    let secret = blake3::derive_key(labels::OPAQUE_KEYSLOT, &export_key);
    Ok(json!({
        "description": "An OPAQUE export-key keyslot (type 5) wrapping the fixture fs bundle (keys.json fs_epoch0.bundle). export_key stands in for an OPAQUE client's 64-byte export key; secret = BLAKE3.derive_key(\"zen/v1/opaque-keyslot\", export_key).",
        "opaque_export": {"rng_seed": "slot-opaque", "credential_id": h(&cred), "export_key": h(&export_key),
                          "secret": h(&secret), "slot": h(&slot)},
    }))
}

fn signature_vectors() -> Result<Value> {
    let (user, user_seed) = SigningIdentity::generate(&mut DetRng::new("user"))?;
    let msg = b"commit 42";
    let signature = user.sign(labels::SIG_COMMIT, msg)?;
    let dev = fixture_device();
    let cert = sig::issue_device_cert(&user, &dev.public(), 1_790_000_000)?;
    Ok(json!({
        "description": "Hybrid Ed25519 + ML-DSA-65 signatures (both deterministic).",
        "user": {"seed": h(user_seed.as_ref()), "public": h(&user.public().encode()), "fingerprint": h(&user.public().fingerprint())},
        "signature": {"purpose": labels::SIG_COMMIT, "message": h(msg), "signature": h(&signature)},
        "device_cert": {"device_rng_seed": "device", "device_fingerprint": h(&dev.public().fingerprint()),
                        "created_unix": 1_790_000_000u64, "cert": h(&cert)},
    }))
}

/// Fixture filesystem ids: tree, a file node, two chunks, a device.
pub fn fixture_fs_ids() -> ([u8; 16], [u8; 16], [[u8; 16]; 2], [u8; 32]) {
    ([0x11; 16], [0x22; 16], [[0x33; 16], [0x44; 16]], [0xDE; 32])
}

/// Fixture node meta.
pub fn fixture_meta() -> fs::NodeMeta {
    fs::NodeMeta {
        node_type: fs::NodeType::File,
        name: "notes.txt".into(),
        mode: 0o644,
        mtime_ms: 1_790_000_000_123,
        xattrs: Vec::new(),
    }
}

fn fs_vectors() -> Result<Value> {
    let fsk = fixture_fs();
    let (tree, node, chunk_ids, device) = fixture_fs_ids();
    let meta = fixture_meta();
    let sealed_meta = fs::seal_meta(&fsk, &tree, &node, &meta, &mut DetRng::new("fs-meta"))?;
    let manifest = fs::Manifest {
        size: 70_000,
        chunk_size: 65_536,
        chunks: chunk_ids.to_vec(),
    };
    let sealed_manifest = fs::seal_manifest(
        &fsk,
        &tree,
        &node,
        &manifest,
        &mut DetRng::new("fs-manifest"),
    )?;
    let chunk0 = vec![0x61u8; 64];
    let sealed_chunk = fs::seal_chunk(&fsk, &chunk_ids[0], &chunk0, &mut DetRng::new("fs-chunk"))?;
    let hlc = fs::hlc(1_790_000_000_123, 7);
    let ops = [
        fs::move_bytes(fsk.fs_id, &tree, &node, &fs::ROOT, hlc, &sealed_meta),
        fs::meta_bytes(fsk.fs_id, &tree, &node, hlc + 1, &sealed_meta),
        fs::write_bytes(
            fsk.fs_id,
            &tree,
            &node,
            &[[0x66; 12]],
            &chunk_ids,
            &sealed_manifest,
        ),
    ];
    let mut chain = [0u8; 32];
    let mut chains = Vec::new();
    for op in &ops {
        chain = fs::chain_next(&chain, op, &device);
        chains.push(json!({"op": h(op), "chain": h(&chain)}));
    }
    Ok(json!({
        "description": "CRDT filesystem objects (formats.md §11) for the fixture fs at epoch 0.",
        "fs_data_key": h(fsk.fs_data_key().as_ref()),
        "tree": h(&tree), "node": h(&node), "device": h(&device),
        "hlc": {"unix_ms": 1_790_000_000_123u64, "counter": 7, "hlc": hlc},
        "meta": {"rng_seed": "fs-meta", "plaintext": h(&meta.encode()?), "sealed": h(&sealed_meta)},
        "manifest": {"rng_seed": "fs-manifest", "plaintext": h(&manifest.encode()), "sealed": h(&sealed_manifest)},
        "chunk": {"rng_seed": "fs-chunk", "id": h(&chunk_ids[0]), "plaintext": h(&chunk0), "sealed": h(&sealed_chunk)},
        "op_chain": {"replaces": [h(&[0x66; 12])], "steps": chains},
    }))
}

/// Challenge signed in the password-key vectors.
pub const FIXTURE_CHALLENGE: [u8; 32] = [0x5A; 32];
/// Origin signed in the password-key vectors.
pub const FIXTURE_ORIGIN: &str = "https://zen.example.org";

fn pwkey_vectors() -> Result<Value> {
    let (key, salt) = PasswordKey::create(
        FIXTURE_PASSPHRASE,
        FIXTURE_ARGON2,
        &mut DetRng::new("pwkey"),
    )?;
    let root = pwkey::root(FIXTURE_PASSPHRASE, &salt, FIXTURE_ARGON2)?;
    let seed = pwkey::seed(&root);
    let public = key.public();
    let msg = pwkey::session_message(&FIXTURE_CHALLENGE, FIXTURE_ORIGIN);
    let signature = key.sign_session(&FIXTURE_CHALLENGE, FIXTURE_ORIGIN)?;
    // Sanity: sign-in derives the same key from the stored salt.
    let again = PasswordKey::derive(FIXTURE_PASSPHRASE, &salt, FIXTURE_ARGON2)?;
    assert_eq!(again.public(), public);
    pwkey::verify_session(&public, &FIXTURE_CHALLENGE, FIXTURE_ORIGIN, &signature)?;
    Ok(json!({
        "description": "Password-derived signing key (formats.md §7.5): root = Argon2id(password, salt), seed = KDF(\"zen/v1/password-sig\", root, \"\"), identity from seed (§7.1); sign-in signature over lp(challenge) || lp(origin).",
        "password": String::from_utf8_lossy(FIXTURE_PASSPHRASE),
        "salt_rng_seed": "pwkey", "salt": h(&salt),
        "m_cost_kib": FIXTURE_ARGON2.m_cost_kib, "t_cost": FIXTURE_ARGON2.t_cost, "p_cost": FIXTURE_ARGON2.p_cost,
        "root": h(root.as_ref()), "seed": h(seed.as_ref()),
        "public": h(&public.encode()), "fingerprint": h(&public.fingerprint()),
        "session": {"purpose": labels::SIG_PASSWORD_SESSION, "challenge": h(&FIXTURE_CHALLENGE),
                    "origin": FIXTURE_ORIGIN, "message": h(&msg), "signature": h(&signature)},
    }))
}
