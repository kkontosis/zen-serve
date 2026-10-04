//! Behavioural tests: round trips, tamper detection, key separation.

use zen_core::Error;
use zen_core::keys::FsKeys;
use zen_core::keyslot::{self, Argon2Params, Unlock};
use zen_core::rng::{DetRng, OsRng};
use zen_core::seal::{self, EventBody};
use zen_core::sig::{self, DeviceSecret, SigningIdentity};
use zen_core::token::{TopicKeys, kv_key};
use zen_core::{labels, vectors};

fn fs() -> FsKeys {
    vectors::fixture_fs()
}

#[test]
fn value_round_trip_with_os_rng() {
    let fs = FsKeys::generate(3, &mut OsRng).unwrap();
    let key = kv_key(&fs, &["ns", "t", "pk"]);
    let sealed = seal::seal_value(&fs, &key, b"row", &mut OsRng).unwrap();
    assert_eq!(seal::open_value(&fs, &key, &sealed).unwrap(), b"row");
    // Fresh nonce every time: identical plaintext gives unrelated ciphertext.
    assert_ne!(
        sealed,
        seal::seal_value(&fs, &key, b"row", &mut OsRng).unwrap()
    );
}

#[test]
fn fs_id_zero_is_reserved() {
    assert_eq!(FsKeys::generate(0, &mut OsRng).err(), Some(Error::Param));
}

#[test]
fn value_tampering_is_detected() {
    let fs = fs();
    let key = kv_key(&fs, &["a", "b"]);
    let sealed = seal::seal_value(&fs, &key, b"secret", &mut DetRng::new("t")).unwrap();
    // Any flipped byte (header, nonce, ciphertext, tag) is rejected.
    for i in 0..sealed.len() {
        let mut bad = sealed.clone();
        bad[i] ^= 0x01;
        assert!(
            seal::open_value(&fs, &key, &bad).is_err(),
            "flip at {i} accepted"
        );
    }
    // Truncation.
    assert!(seal::open_value(&fs, &key, &sealed[..sealed.len() - 1]).is_err());
}

#[test]
fn value_is_bound_to_key_and_fs() {
    let fs = fs();
    let key = kv_key(&fs, &["a", "b"]);
    let sealed = seal::seal_value(&fs, &key, b"secret", &mut DetRng::new("t")).unwrap();
    // Swapped to another key.
    let other = kv_key(&fs, &["a", "c"]);
    assert_eq!(
        seal::open_value(&fs, &other, &sealed).err(),
        Some(Error::Decrypt)
    );
    // Moved to another fs with the same raw keys but a different fs_id.
    let b = fs.to_bundle();
    let moved = FsKeys::from_parts(
        8,
        0,
        b[8..40].try_into().unwrap(),
        b[40..72].try_into().unwrap(),
    );
    assert_eq!(
        seal::open_value(&moved, &key, &sealed).err(),
        Some(Error::Decrypt)
    );
    // A different fs entirely.
    let unrelated = FsKeys::generate(7, &mut DetRng::new("other")).unwrap();
    assert_eq!(
        seal::open_value(&unrelated, &key, &sealed).err(),
        Some(Error::Decrypt)
    );
}

#[test]
fn event_is_bound_to_topic_and_key_token() {
    let fs = fs();
    let topic = TopicKeys::new(&fs, &["chat", "a"]);
    let tok = topic.event_key_token(b"k1");
    let body = EventBody {
        sender: [1; 32],
        hlc: 9,
        causation: vec![],
        payload: b"hi".to_vec(),
    };
    let sealed = seal::seal_event(&fs, &topic, Some(&tok), &body, &mut DetRng::new("e")).unwrap();
    assert_eq!(
        seal::open_event(&fs, &topic, Some(&tok), &sealed).unwrap(),
        body
    );
    // Wrong key token, missing key token, wrong topic.
    let other_tok = topic.event_key_token(b"k2");
    assert!(seal::open_event(&fs, &topic, Some(&other_tok), &sealed).is_err());
    assert!(seal::open_event(&fs, &topic, None, &sealed).is_err());
    let other_topic = TopicKeys::new(&fs, &["chat", "b"]);
    assert!(seal::open_event(&fs, &other_topic, Some(&tok), &sealed).is_err());
    // A value cannot be opened as an event or vice versa (kind byte is authenticated).
    assert_eq!(
        seal::open_value(&fs, topic.id(), &sealed).err(),
        Some(Error::Format)
    );
}

#[test]
fn event_key_tokens_are_per_topic() {
    let fs = fs();
    let a = TopicKeys::new(&fs, &["orders"]);
    let b = TopicKeys::new(&fs, &["invoices"]);
    assert_ne!(a.event_key_token(b"cust-42"), b.event_key_token(b"cust-42"));
}

#[test]
fn token_prefix_property() {
    let fs = fs();
    let parent = kv_key(&fs, &["app", "users"]);
    let child = kv_key(&fs, &["app", "users", "42"]);
    assert_eq!(parent.len(), 32);
    assert_eq!(child.len(), 48);
    assert!(child.starts_with(&parent));
    let t_parent = TopicKeys::new(&fs, &["chat"]);
    let t_child = TopicKeys::new(&fs, &["chat", "alice-bob"]);
    assert!(t_child.id().starts_with(t_parent.id()));
    // Delegation: a holder of the parent derives the same child.
    assert_eq!(t_parent.child(b"alice-bob").id(), t_child.id());
    // Element encoding is length-prefixed, so ("ab","c") != ("a","bc").
    assert_ne!(kv_key(&fs, &["ab", "c"]), kv_key(&fs, &["a", "bc"]));
    // KV and topic namespaces are separate.
    assert_ne!(kv_key(&fs, &["chat"]), t_parent.id());
}

#[test]
fn rotation_keeps_names_and_changes_data_keys() {
    let fs = fs();
    let (next, record) = fs.rotate(&mut DetRng::new("r")).unwrap();
    assert_eq!(next.epoch, 1);
    // G2: tokens and topic ids are stable across epochs.
    assert_eq!(kv_key(&fs, &["x"]), kv_key(&next, &["x"]));
    assert_eq!(
        TopicKeys::new(&fs, &["t"]).id(),
        TopicKeys::new(&next, &["t"]).id()
    );
    // Data keys change.
    assert_ne!(*fs.kv_data_key(), *next.kv_data_key());
    // Old values are readable via the backward chain, and record their epoch.
    let key = kv_key(&fs, &["x"]);
    let old = seal::seal_value(&fs, &key, b"v0", &mut DetRng::new("v")).unwrap();
    assert_eq!(seal::peek(&old).unwrap().1, 0);
    assert!(seal::open_value(&next, &key, &old).is_err());
    let prev = next.previous(&record).unwrap();
    assert_eq!(seal::open_value(&prev, &key, &old).unwrap(), b"v0");
    // A chain record from another rotation does not open.
    let (_, other_record) = fs.rotate(&mut DetRng::new("r2")).unwrap();
    assert!(next.previous(&other_record).is_err());
}

#[test]
fn keyslots_reject_wrong_secrets() {
    let fs = fs();
    let params = vectors::FIXTURE_ARGON2;
    let slot = keyslot::create_passphrase(&fs, b"right", params, &mut DetRng::new("p")).unwrap();
    assert_eq!(
        keyslot::open(&slot, Unlock::Passphrase(b"wrong")).err(),
        Some(Error::Decrypt)
    );
    assert_eq!(
        *keyslot::open(&slot, Unlock::Passphrase(b"right"))
            .unwrap()
            .to_bundle(),
        *fs.to_bundle()
    );
    // Tampering with the stored salt or (bounded) params breaks the AAD/KEK.
    let mut bad = slot.clone();
    bad[40] ^= 1; // inside the salt
    assert_eq!(
        keyslot::open(&bad, Unlock::Passphrase(b"right")).err(),
        Some(Error::Decrypt)
    );
    let mut bad = slot.clone();
    bad[27] = 2; // t_cost 1 -> 2: within limits, still rejected
    assert_eq!(
        keyslot::open(&bad, Unlock::Passphrase(b"right")).err(),
        Some(Error::Decrypt)
    );
    // A server inflating the cost beyond the ceiling is refused before any work.
    let mut bad = slot.clone();
    bad[25] ^= 1; // t_cost 1 -> 65537
    assert_eq!(
        keyslot::open(&bad, Unlock::Passphrase(b"right")).err(),
        Some(Error::Param)
    );
    let mut bad = slot.clone();
    bad[20] = 0xff; // m_cost far above 4 GiB
    assert_eq!(
        keyslot::open(&bad, Unlock::Passphrase(b"right")).err(),
        Some(Error::Param)
    );
    // Wrong unlock method for the slot type.
    assert_eq!(
        keyslot::open(&slot, Unlock::Recovery(&[0; 32])).err(),
        Some(Error::Param)
    );

    let (rslot, rkey) = keyslot::create_recovery(&fs, &mut DetRng::new("r")).unwrap();
    let mut wrong = *rkey;
    wrong[0] ^= 1;
    assert_eq!(
        keyslot::open(&rslot, Unlock::Recovery(&wrong)).err(),
        Some(Error::Decrypt)
    );

    let dev = vectors::fixture_device();
    let other = DeviceSecret::generate(&mut DetRng::new("other-device"))
        .unwrap()
        .0;
    let dslot = keyslot::create_device(&fs, &dev.public(), &mut DetRng::new("d")).unwrap();
    assert_eq!(
        keyslot::open(&dslot, Unlock::Device(&other)).err(),
        Some(Error::Decrypt)
    );
    assert!(keyslot::open(&dslot, Unlock::Device(&dev)).is_ok());
}

#[test]
fn argon2_floor_enforced_on_create() {
    let weak = Argon2Params {
        m_cost_kib: 1024,
        t_cost: 1,
        p_cost: 1,
    };
    assert_eq!(
        keyslot::create_passphrase(&fs(), b"pw", weak, &mut OsRng).err(),
        Some(Error::Param)
    );
}

#[test]
fn hybrid_signature_requires_both_halves() {
    let (user, _) = SigningIdentity::generate(&mut DetRng::new("u")).unwrap();
    let public = user.public();
    let sig = user.sign(labels::SIG_COMMIT, b"m").unwrap();
    assert_eq!(sig.len(), sig::SIGNATURE_LEN);
    public.verify(labels::SIG_COMMIT, b"m", &sig).unwrap();
    // Corrupt the Ed25519 half, then the ML-DSA half.
    for idx in [2 + 10, 2 + sig::ED25519_SIG_LEN + 100] {
        let mut bad = sig.clone();
        bad[idx] ^= 1;
        assert_eq!(
            public.verify(labels::SIG_COMMIT, b"m", &bad).err(),
            Some(Error::Signature)
        );
    }
    // Wrong message, wrong purpose, unknown purpose.
    assert!(public.verify(labels::SIG_COMMIT, b"n", &sig).is_err());
    assert!(public.verify(labels::SIG_ACL, b"m", &sig).is_err());
    assert_eq!(
        user.sign("zen/v1/sig/nonsense", b"m").err(),
        Some(Error::Param)
    );
    // Another identity's key.
    let (other, _) = SigningIdentity::generate(&mut DetRng::new("o")).unwrap();
    assert!(
        other
            .public()
            .verify(labels::SIG_COMMIT, b"m", &sig)
            .is_err()
    );
}

#[test]
fn public_keys_round_trip() {
    let dev = vectors::fixture_device();
    let p = dev.public();
    assert_eq!(sig::DevicePublic::decode(&p.encode()).unwrap(), p);
    assert_eq!(p.encode().len(), sig::DEVICE_PUBLIC_LEN);
    let ident = dev.signing().public();
    assert_eq!(sig::PublicIdentity::decode(&ident.encode()).unwrap(), ident);
}

#[test]
fn device_cert_binds_user_and_device() {
    let (user, _) = SigningIdentity::generate(&mut DetRng::new("u")).unwrap();
    let dev = vectors::fixture_device();
    let cert = sig::issue_device_cert(&user, &dev.public(), 5).unwrap();
    let (d, t) = sig::verify_device_cert(&user.public(), &cert).unwrap();
    assert_eq!((d, t), (dev.public(), 5));
    let (mallory, _) = SigningIdentity::generate(&mut DetRng::new("m")).unwrap();
    assert!(sig::verify_device_cert(&mallory.public(), &cert).is_err());
    let mut bad = cert.clone();
    bad[40] ^= 1; // inside the device public key
    assert!(sig::verify_device_cert(&user.public(), &bad).is_err());
}

#[test]
fn password_key_round_trip_and_separation() {
    use zen_core::pwkey::{self, PasswordKey};
    let p = vectors::FIXTURE_ARGON2;
    let (key, salt) = PasswordKey::create(b"hunter2 hunter2", p, &mut OsRng).unwrap();
    let again = PasswordKey::derive(b"hunter2 hunter2", &salt, p).unwrap();
    assert_eq!(key.public(), again.public());
    let sig = again.sign_session(&[7; 32], "https://a.example").unwrap();
    pwkey::verify_session(&key.public(), &[7; 32], "https://a.example", &sig).unwrap();
    // Bound to the challenge and the origin.
    assert_eq!(
        pwkey::verify_session(&key.public(), &[8; 32], "https://a.example", &sig),
        Err(Error::Signature)
    );
    assert_eq!(
        pwkey::verify_session(&key.public(), &[7; 32], "https://b.example", &sig),
        Err(Error::Signature)
    );
    // Another password, or another salt, is another key.
    let wrong = PasswordKey::derive(b"hunter3 hunter3", &salt, p).unwrap();
    assert_ne!(wrong.public(), key.public());
    let other = PasswordKey::derive(b"hunter2 hunter2", &[0; 32], p).unwrap();
    assert_ne!(other.public(), key.public());
}

#[test]
fn password_key_parameter_bounds() {
    use zen_core::pwkey::PasswordKey;
    let low = Argon2Params {
        m_cost_kib: 1024,
        t_cost: 1,
        p_cost: 1,
    };
    // Creation enforces the floor; sign-in only the ceilings.
    assert!(matches!(
        PasswordKey::create(b"pw", low, &mut OsRng),
        Err(Error::Param)
    ));
    assert!(PasswordKey::derive(b"pw", &[1; 32], low).is_ok());
    let huge = Argon2Params {
        m_cost_kib: Argon2Params::MAX_M_COST_KIB + 1,
        t_cost: 1,
        p_cost: 1,
    };
    assert!(matches!(
        PasswordKey::derive(b"pw", &[1; 32], huge),
        Err(Error::Param)
    ));
    for (t, pc) in [(0, 1), (17, 1), (1, 0), (1, 5)] {
        let bad = Argon2Params {
            m_cost_kib: 1024,
            t_cost: t,
            p_cost: pc,
        };
        assert!(matches!(
            PasswordKey::derive(b"pw", &[1; 32], bad),
            Err(Error::Param)
        ));
    }
}
