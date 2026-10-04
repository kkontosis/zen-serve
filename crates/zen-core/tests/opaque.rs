//! The OPAQUE suite and client (spec/auth.md §8) against a server built
//! from the same `opaque-ke` types.

use zen_core::Error;
use zen_core::keyslot::{self, Argon2Params, Unlock};
use zen_core::opaque::opaque_ke::{
    CredentialFinalization, CredentialRequest, RegistrationRequest, RegistrationUpload,
    ServerLogin, ServerLoginParameters, ServerRegistration, ServerSetup,
};
use zen_core::opaque::*;
use zen_core::rng::OsRng;
use zen_core::vectors::{FIXTURE_ARGON2, fixture_fs};

const NAME_ID: &[u8] = b"credential identifier";
const ORIGIN: &str = "https://zen.example.org";

fn setup() -> ServerSetup<Suite> {
    ServerSetup::new(&mut RandCore(&mut OsRng))
}

fn register(s: &ServerSetup<Suite>, pw: &[u8]) -> (Vec<u8>, Finished) {
    let (reg, req) = Registration::start(pw, &mut OsRng).unwrap();
    assert_eq!(req.len(), REGISTRATION_REQUEST_LEN);
    let resp = ServerRegistration::<Suite>::start(
        s,
        RegistrationRequest::deserialize(&req).unwrap(),
        NAME_ID,
    )
    .unwrap()
    .message
    .serialize()
    .to_vec();
    assert_eq!(resp.len(), REGISTRATION_RESPONSE_LEN);
    let done = reg.finish(pw, &resp, FIXTURE_ARGON2, &mut OsRng).unwrap();
    assert_eq!(done.message.len(), REGISTRATION_UPLOAD_LEN);
    let record = ServerRegistration::finish(
        RegistrationUpload::<Suite>::deserialize(&done.message).unwrap(),
    )
    .serialize()
    .to_vec();
    (record, done)
}

/// One sign-in: the client's result, or its error, and whether the server
/// accepted the finalization.
fn login(
    s: &ServerSetup<Suite>,
    record: Option<&[u8]>,
    pw: &[u8],
    client_origin: &str,
    server_origin: &str,
) -> Result<Finished, Error> {
    let (login, req) = Login::start(pw, &mut OsRng).unwrap();
    assert_eq!(req.len(), CREDENTIAL_REQUEST_LEN);
    let ctx = context(server_origin);
    let params = || ServerLoginParameters {
        context: Some(&ctx),
        ..Default::default()
    };
    let started = ServerLogin::start(
        &mut RandCore(&mut OsRng),
        s,
        record.map(|r| ServerRegistration::deserialize(r).unwrap()),
        CredentialRequest::deserialize(&req).unwrap(),
        NAME_ID,
        params(),
    )
    .unwrap();
    let resp = started.message.serialize().to_vec();
    assert_eq!(resp.len(), CREDENTIAL_RESPONSE_LEN);
    let done = login.finish(pw, &resp, FIXTURE_ARGON2, client_origin, &mut OsRng)?;
    assert_eq!(done.message.len(), CREDENTIAL_FINALIZATION_LEN);
    started
        .state
        .finish(
            CredentialFinalization::deserialize(&done.message).unwrap(),
            params(),
        )
        .map_err(|_| Error::Signature)?;
    Ok(done)
}

#[test]
fn register_and_sign_in() {
    let s = setup();
    let (record, reg) = register(&s, b"pw");
    let done = login(&s, Some(&record), b"pw", ORIGIN, ORIGIN).unwrap();
    assert_eq!(*done.export_key, *reg.export_key, "the same export key");
    assert_eq!(done.server_public_key, reg.server_public_key);
    assert_eq!(
        &reg.server_public_key[..],
        &s.keypair().public().serialize()[..]
    );

    // The export key opens a type-5 keyslot.
    let fs = fixture_fs();
    let slot = keyslot::create_opaque_export(&fs, &[1; 32], &reg.export_key, &mut OsRng).unwrap();
    let opened = keyslot::open(&slot, Unlock::OpaqueExport(&done.export_key)).unwrap();
    assert_eq!(*opened.to_bundle(), *fs.to_bundle());

    // A new registration with the same password has a new export key.
    let (record2, reg2) = register(&s, b"pw");
    assert_ne!(*reg2.export_key, *reg.export_key);
    assert_ne!(record2, record);
}

#[test]
fn failures_look_alike_to_the_client() {
    let s = setup();
    let (record, _) = register(&s, b"pw");
    // A wrong password, an unknown name (the dummy record) and a server
    // on another origin: the client can't finish, and has nothing to send.
    let wrong = login(&s, Some(&record), b"other", ORIGIN, ORIGIN);
    let unknown = login(&s, None, b"pw", ORIGIN, ORIGIN);
    let relayed = login(&s, Some(&record), b"pw", "https://relay.example", ORIGIN);
    for r in [wrong, unknown, relayed] {
        assert_eq!(r.err(), Some(Error::Decrypt));
    }
}

#[test]
fn ksf_is_argon2id_with_a_zero_salt() {
    use zen_core::opaque::opaque_ke::generic_array::GenericArray;
    use zen_core::opaque::opaque_ke::generic_array::typenum::U64;
    use zen_core::opaque::opaque_ke::ksf::Ksf as _;
    let p = Argon2Params {
        m_cost_kib: 64,
        t_cost: 1,
        p_cost: 1,
    };
    let input = GenericArray::<u8, U64>::from([7u8; 64]);
    let out = Ksf(p).hash(input).unwrap();
    let mut want = [0u8; 64];
    argon2::Argon2::new(
        argon2::Algorithm::Argon2id,
        argon2::Version::V0x13,
        argon2::Params::new(64, 1, 1, Some(64)).unwrap(),
    )
    .hash_password_into(&[7u8; 64], &[0u8; 16], &mut want)
    .unwrap();
    assert_eq!(out.as_slice(), &want[..]);
    assert_eq!(
        context("https://a.example"),
        b"zen/v1/opaque\0https://a.example"
    );
}

#[test]
fn parameters_and_sizes_are_checked() {
    let s = setup();
    let weak = Argon2Params {
        m_cost_kib: 1024,
        t_cost: 1,
        p_cost: 1,
    };
    let (reg, req) = Registration::start(b"pw", &mut OsRng).unwrap();
    let resp = ServerRegistration::<Suite>::start(
        &s,
        RegistrationRequest::deserialize(&req).unwrap(),
        NAME_ID,
    )
    .unwrap()
    .message
    .serialize()
    .to_vec();
    // Below the registration floor.
    assert_eq!(
        Registration::start(b"pw", &mut OsRng)
            .unwrap()
            .0
            .finish(b"pw", &resp, weak, &mut OsRng)
            .err(),
        Some(Error::Param)
    );
    // A response of the wrong size.
    assert_eq!(
        reg.finish(b"pw", &resp[..63], FIXTURE_ARGON2, &mut OsRng)
            .err(),
        Some(Error::Format)
    );
    // Sign-in parameters above the ceiling are refused before any work.
    let (login, _) = Login::start(b"pw", &mut OsRng).unwrap();
    let huge = Argon2Params {
        m_cost_kib: Argon2Params::MAX_M_COST_KIB + 1,
        ..FIXTURE_ARGON2
    };
    assert_eq!(
        login
            .finish(
                b"pw",
                &[0; CREDENTIAL_RESPONSE_LEN],
                huge,
                ORIGIN,
                &mut OsRng
            )
            .err(),
        Some(Error::Param)
    );
}
