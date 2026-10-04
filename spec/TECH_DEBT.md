# Technical debt

Work that was knowingly deferred. Each entry has a **stable reference name**, `TD-<AREA>-<SLUG>`, that code comments, specs and issues can cite. An entry is never renumbered or renamed; when it is done, its status becomes `resolved` with a pointer to the change, and the entry stays.

Fields: **Status** (`open`, `in progress`, `resolved`), **Context**, **Why deferred**, **What it would take**.

## TD-AUTH-INVITES

* **Status:** open
* **Context:** Adding a member is an admin-signed ACL change (formats.md §9): the admin needs the new user's public identity, and for device keys a device certificate, before the user can sign in by any method. There is no invite link, no one-time enrolment code and no self-service onboarding.
* **Why deferred:** An invite flow needs a design of its own: who may invite, how an invitee proves possession of the invite, how the invitee's identity reaches an admin for signing, and how this fits the rule that the signed ACL is the only source of membership. The multi-method sign-in work (auth.md) did not need it.
* **Decision until then:** a user who will sign in only with a password needs an existing session of their own to register the first password (auth.md §11.2, and §8.2 for OPAQUE), for example a device-key session, which needs a device certificate in the ACL. There is no session-less first registration.
* **Intended solution: one-time enrolment codes.** An admin or an existing member issues a single-use code for a member; the new member presents it, without a prior session, to register their first credential (for example a password-derived key, or a passkey). The code is stored only hashed, expires, and is spent by the registration.
* **What it would take:** An invite or enrolment record in the keyspace (hashed one-time secret, expiry, issuer, the member or intended grants); an unauthenticated endpoint where the invitee presents the code with their first credential, or with their public identity when they are not a member yet; for new members, a pending-members list that admins read and sign into the next ACL version; spec in auth.md and api.md; tests for expiry, reuse and revocation.

## TD-AUTH-WEBAUTHN-PRF-KEYSLOT

* **Status:** resolved, by the commit "spec: WebAuthn PRF keyslot (formats.md §6, type 4)": keyslot type 4 in formats.md §6 and zen-core (`keyslot::create_webauthn_prf`), the flows in auth.md §7.7, vectors in `spec/test-vectors/prf_keyslot.json`.
* **Context:** Passkeys (auth.md §7) only give server access. The WebAuthn PRF extension can return a per-credential secret on the client, which could unlock data keys the way a passphrase does.
* **Why deferred:** It needs a new keyslot type (formats.md §6) and passkeys are not implemented yet.
* **What it would take:** A keyslot type 4 whose secret is the PRF output for a fixed, per-slot salt; the slot stores the credential id and the PRF salt; new labels; vectors; and a fallback when the authenticator has no PRF support. (Done without new labels: WebAuthn already domain-separates the PRF input. The fallback is the user's other keyslots.)

## TD-AUTH-WEBAUTHN-RS256

* **Status:** resolved, by the commit "spec: RS256 passkeys (auth.md §7)": the user accepted the release-candidate dependency. RS256 verification uses `rsa` pinned to `=0.10.0-rc.18`, with the key policy of auth.md §7 (2048–4096-bit modulus, odd exponent from 65537 to 2³² − 1). Follow-up: move to the `rsa` 0.10 release once it is out, and re-run the passkey tests.
* **Context:** The passkey verifier (auth.md §7) supports EdDSA (Ed25519) and ES256 (P-256). An authenticator that can only sign with RS256 (COSE −257), such as some older Windows Hello TPM configurations, can't register.
* **Why deferred:** The verifier is pure Rust, without OpenSSL. The pure-Rust `rsa` crate for the RustCrypto generation this workspace uses (`signature` 3, `sha2` 0.11) is only a release candidate, and writing RSA verification by hand means a big-integer implementation of our own. Current platform authenticators and security keys all offer ES256.
* **What it would take:** Once `rsa` 0.10 is released: parse COSE RSA keys (kty 3, `n`, `e`, a floor of 2048 bits), verify RSASSA-PKCS1-v1_5 with SHA-256, add −257 last in `pubKeyCredParams` and `/v1/info`, and unit tests with generated keys. Only public-key operations are needed, so the crate's timing advisory on decryption doesn't apply.

## TD-AUTH-WEBAUTHN-ATTESTATION

* **Status:** open
* **Context:** Passkey registration (auth.md §7.2) doesn't verify attestation statements: `fmt` `"none"` is required to be empty, and every other format's statement is ignored. The server can't restrict passkeys to certain authenticator models, or tell a hardware key from a software authenticator.
* **Why deferred:** Verifying attestation needs X.509 path validation, the formats' own rules (`packed`, `tpm`, `android-key`, `apple`, `fido-u2f`) and a maintained set of roots, such as the FIDO Metadata Service. Synced passkeys mostly send `"none"` anyway, so it only helps deployments that mandate security keys.
* **What it would take:** An `[auth] passkey_attestation` policy (`none`, `verify`, `require`) and a configured roots directory or MDS blob; a pure-Rust X.509 verifier; storing the AAGUID and the verified format in the credential record; per-format tests with real attestation samples.

## TD-AUTH-UNICODE-LOGIN

* **Status:** open
* **Context:** Login names (auth.md §4.2) are limited to ASCII letters, digits and `. _ - @ +`, normalized by trimming and ASCII lowercasing.
* **Why deferred:** Unicode names need a normalization form (NFC or NFKC), case folding and confusable handling, all pinned to a Unicode version so the stored hashes never change meaning. That is a dependency and a spec of its own, and email-style names cover the common case.
* **What it would take:** Choose a profile (for example the PRECIS `UsernameCaseMapped` class of RFC 8265), add a pure-Rust implementation to zen-proto (wasm-compatible), version the normalization in the credential record so old hashes stay valid, and add vectors.

## TD-AUTH-LIMITER-CLUSTER

* **Status:** open
* **Context:** The failed-sign-in limiter of methods 6 and 3 (auth.md §11.3, §8.5), which counts each method separately, is in each node's memory. A cluster of n nodes allows n times the configured failures, and a restart clears the counts. (A successful OPAQUE sign-in records its time in the credential, so the node of the next `start` forgets failures before it; that only keeps alternating nodes from locking users out.)
* **Why deferred:** A shared limiter means a write on every failed sign-in, which unauthenticated callers could use to load storage. The per-node limiter, with Argon2id on the client, already makes online guessing slow.
* **What it would take:** A keyspace counter per login-name hash with a time bucket, written with an atomic add (snapshot reads, like the quota counters), swept like sessions; a cap on writes per source address; tests on FoundationDB with several nodes.

## TD-AUTH-TOKEN-SCOPES

* **Status:** open
* **Context:** An API token (auth.md §9) carries all the rights of its member, admin included, except managing sign-in. There is no way to limit a token to some filesystems, topics or rights.
* **Why deferred:** Rights are granted to members by the signed ACL, the single source of truth. Scoping a token below its member would add a second, server-side rights system. A dedicated member per service, with narrow grants, already gives least privilege.
* **What it would take:** Either an optional `scope` in the token record that intersects the member's rights (fs ids, topic prefixes, a rights mask), enforced in `Caller::require_*`; or admin-signed scopes in the ACL. Plus spec, tests, and a decision on which of the two keeps the ACL authoritative.

## TD-AUTH-SESSION-LIST

* **Status:** open
* **Context:** A user can list and remove stored credentials (auth.md §4), which ends the sessions they created, but can't list their individual sessions or end one session other than the current one (logout). Device sessions end only through logout, expiry or an ACL change.
* **Why deferred:** Sessions are keyed by the hash of their token, with no index by user, so listing them needs a new index written on every sign-in. Removing the credential already covers "sign out everywhere" for every method except device keys.
* **What it would take:** An index `pack("sessu", user_fp, H(token))`, written with each session and swept with it; `/v1/auth/sessions/list` and `/remove` (own, or admin for any member) returning method, credential id, creation and expiry; spec and tests, including cross-node cache expiry.

## TD-TLS-RSA

* **Status:** resolved for verification, by the commit "spec: RSA client certificates and CAs in native TLS (auth.md §10.1, operations.md §8)": the user accepted the release-candidate dependency. `tls::provider` (since renamed `tls::rustcrypto`) verifies RSA PKCS#1 v1.5 and PSS signatures (SHA-256/384/512) with `rsa` pinned to `=0.10.0-rc.18`, under the passkey key policy (2048–4096 bits, odd exponent from 65537 to 2³² − 1, `rsakey`). RSA **server keys** (signing) stay open as `TD-TLS-RSA-SERVER-KEY`. Follow-up: the `rsa` 0.10 release, as `TD-AUTH-WEBAUTHN-RS256`.
* **Context:** Native TLS (operations.md §8) runs on a rustls crypto provider of zen-serve's own on RustCrypto. It signs and verifies ECDSA (P-256, P-384) and Ed25519 only. A server key on RSA can't be loaded; a client certificate signed by an RSA CA, or a client with an RSA key, can't sign in natively (auth.md §10.1). Many organisations' client CAs are RSA.
* **Why deferred:** The same as `TD-AUTH-WEBAUTHN-RS256`: the pure-Rust `rsa` crate for this RustCrypto generation is only a release candidate, and ring or aws-lc-rs would bring C and assembly. The trusted-proxy mode (auth.md §10.2) accepts RSA certificates, since the proxy verifies them.
* **What it would take:** Once `rsa` 0.10 is released: RSA-PSS and PKCS#1 v1.5 verification (SHA-256/384/512) in `tls::provider`'s `WebPkiSupportedAlgorithms`, with a floor of 2048 bits; RSA server keys (PKCS#1 and PKCS#8) with RSA-PSS signing; tests with generated RSA CAs and keys.

## TD-TLS-ACME

* **Status:** open
* **Context:** Native TLS reads `[tls] cert` and `key` once, at start-up (operations.md §8.1). Renewing a certificate needs a restart, and obtaining one is up to the operator.
* **Why deferred:** ACME (RFC 8555) needs an HTTP-01 or TLS-ALPN-01 responder, account keys, storage of certificates shared by a cluster's nodes, and renewal scheduling: a feature of its own. A restart after renewal, or a reverse proxy that does ACME, covers the need meanwhile.
* **What it would take:** First, reloading: a `ResolvesServerCert` that re-reads the files on change (or on SIGHUP), keeping the old certificate when the new files don't load. Then ACME with TLS-ALPN-01 on the API port, account key and certificates in the keyspace so every node serves the same one, one node renewing under a lease, and tests against a local ACME test server (Pebble).

## TD-AUTH-MTLS-REVOCATION

* **Status:** open
* **Context:** Native mTLS (auth.md §10.1) checks the chain to `client_ca`, the dates and the key usage, but not revocation: no CRLs, no OCSP.
* **Why deferred:** Every certificate must also be registered to a member, and removing the registration revokes it for zen-serve at once (and ends its sessions). CRL distribution and OCSP fetching need network access, caching and a failure policy (fail open or closed) that a small deployment rarely wants.
* **What it would take:** `[tls] client_crl` files, passed to the client verifier (`with_crls` on rustls's `WebPkiClientVerifier` builder), re-read on change; optionally OCSP for the client chain with a cache and a configurable failure policy; tests with a revoked certificate. In the proxy mode, revocation stays the proxy's job.

## TD-AUTH-MTLS-SUBJECT-MAPPING

* **Status:** open
* **Context:** A client certificate signs in only after it is registered to a member, by the member from a connection that presents it or by an admin (auth.md §10.3). There is no way to map certificates to members by their subject or subject alternative name, for example "any certificate of this CA whose SAN email is alice@example.org is Alice".
* **Why deferred:** A mapping by name makes the CA, not the signed ACL, decide who is who, and needs a policy for name formats, multiple matches and CAs shared with other services. Registration by key is explicit and keeps the ACL authoritative.
* **What it would take:** An admin-set mapping per member (a SAN email or URI, or a subject DN, matched exactly) in the credential store or in the signed ACL; a lookup by the presented certificate's names when its key isn't registered, natively only, or with the proxy forwarding the names; a decision whether a match registers the key automatically; spec and tests.

## TD-TLS-RSA-SERVER-KEY

* **Status:** resolved for the default build, by the commit "spec: the ring TLS provider, on by default, and RSA server keys (operations.md §8, auth.md §10.1)": with the `ring` feature (the default), RSA server keys (PKCS#1, PKCS#8; 2048–4096 bits, odd exponent from 65537 to 2³² − 1) are loaded and signed with by ring, whose RSA signing is constant-time (`tls::ring`). **Open for the pure-Rust build** (`--no-default-features`), which still refuses them as below.
* **Context:** The pure-Rust build refuses an RSA server key at start-up, saying it was built without the `ring` feature (operations.md §8.1). Deployments whose CA only issues RSA server certificates must use the default build, get an ECDSA certificate, or terminate TLS at a proxy.
* **Why deferred:** Signing with an RSA key is a private-key operation, and the pure-Rust `rsa` crate's are not constant-time. Its own README for `0.10.0-rc.18` says the crate "is vulnerable to the Marvin Attack which could enable private key recovery by a network attacker" (RUSTSEC-2023-0071), with mitigation tracked in RustCrypto/RSA#390; the private-key path still reduces with variable-time arithmetic (`rem_vartime` in `rsa_decrypt`), and random blinding masks but doesn't remove the leak. A TLS server signs on demand for anyone who connects, which is the setting the attack needs. ECDSA and Ed25519 server certificates are available from every public CA.
* **What it would take (pure-Rust build):** An `rsa` release whose changelog states constant-time private-key operations (and the advisory marked fixed), then: PKCS#1 and PKCS#8 RSA key loading in `tls::rustcrypto`'s `KeyProvider`, a `SigningKey` offering `rsa_pss_rsae_sha256/384/512` only (TLS 1.3), PSS signing with the OS RNG, the same 2048–4096-bit policy (`rsakey::check_spki_der`), and the RSA server-key tests of `tests/tls.rs` run in that build too.

## TD-TLS-RING-PROVIDER

* **Status:** resolved, by the commit "spec: the ring TLS provider, on by default, and RSA server keys (operations.md §8, auth.md §10.1)", and the code commit "zen-server: a ring TLS provider, feature ring, on by default". The user chose a `ring` feature **on by default**, accepting the C compiler it needs; `--no-default-features` keeps the pure-Rust build. With it, rustls's ring provider (cipher suites, X25519, P-256, P-384, verification through webpki, key loading and signing, randomness) runs with zen-serve's `X25519MLKEM768` first among the groups; RSA keys pass the `rsakey` policy in front of ring; the start-up log names the provider; CI tests both builds (the `pure-rust` job), and the default build's TLS tests run the two providers against each other. aws-lc-rs was not added.
* **Context:** Native TLS uses only zen-serve's own rustls provider on RustCrypto (`tls::provider`). rustls's own providers, ring and aws-lc-rs, are far more widely deployed and tested, and sign with RSA in constant time, but neither is pure Rust: both build C and assembly.
* **Why deferred:** The project keeps its dependencies pure Rust. The custom provider is small glue over RustCrypto primitives, and covers what the server needs, apart from RSA server keys.
* **What it would take:** An optional cargo feature (off by default) that selects `rustls::crypto::ring::default_provider()` (or aws-lc-rs, with its `prefer-post-quantum` hybrid) instead of `tls::provider`, everywhere a provider is built (`tls::server_config`, the client verifier); a start-up log line naming the provider; CI running the TLS and mTLS tests with each provider; and a note in operations.md §8 on what the feature brings in (C, assembly, `unsafe`).

## TD-TLS-RING-X25519

* **Status:** open (user decision: a follow-up)
* **Context:** In the default (ring) build, the `X25519MLKEM768` hybrid group is zen-serve's own (`tls/rustcrypto.rs`), and its X25519 half runs on `x25519-dalek`, while ring's own X25519 group serves plain `X25519`. The hybrid could compute its X25519 share and secret with ring instead, leaving only ML-KEM and the share/secret concatenation as zen-serve's code in that build.
* **Why deferred:** The hybrid works and is tested against itself and, through the shared X25519 group, against ring; moving its X25519 half is a refinement that narrows `TD-TLS-PROVIDER-AUDIT` rather than a fix.
* **What it would take:** A ring-build variant of the hybrid group that drives `rustls::crypto::ring::kx_group::X25519` (`start`/`complete`) for the classical half and `ml-kem` for the rest, the same share and secret layout (draft-ietf-tls-ecdhe-mlkem), handshake tests between the two builds' hybrids, and narrowing `TD-TLS-PROVIDER-AUDIT` accordingly.

## TD-TLS-PROVIDER-AUDIT

* **Status:** open
* **Scope:** narrowed by the commit "spec: the ring TLS provider, on by default, and RSA server keys (operations.md §8, auth.md §10.1)" to (1) the **pure-Rust build**'s provider, and (2) zen-serve's own glue in the **default build**: the `X25519MLKEM768` hybrid and the RSA policy wrapped around ring's verification and key loading (`tls/ring.rs`). Everything else in the default build is rustls's ring provider.
* **Context:** `tls/rustcrypto.rs` connects rustls to RustCrypto: the AEAD record layer, HKDF and HMAC, the key exchanges (including the X25519MLKEM768 hybrid, which the default build uses too), signature verification (ECDSA, Ed25519, RSA) and signing. `tls/ring.rs` and `rsakey.rs` add the RSA key policy to ring's. Mistakes there could break TLS's confidentiality or authentication for native TLS and for mTLS sign-in. Its tests are known-answer tests for HMAC and HKDF, round trips and tamper checks, handshakes on one provider, and, in the default build, handshakes between the two providers (every shared group and suite, every key type), which check the pure-Rust provider against ring's. The hybrid has no independent peer in the tests: both sides are zen-serve's.
* **Why deferred:** An independent review needs people outside this work. Deployments that need assurance meanwhile can use the default build, or terminate TLS at a proxy (operations.md §8.3).
* **What it would take:** A security review of `tls/rustcrypto.rs` (for the default build, its hybrid group only), `tls/ring.rs` and `rsakey.rs` against rustls's own providers and the RFCs (8446 record protection and key schedule inputs, draft-ietf-tls-ecdhe-mlkem share and secret layout, RFC 8017 verification, key and point validation); interoperability tests against independent implementations (for example OpenSSL 3.5 or BoringSSL clients and servers, with the hybrid, every suite and each signature algorithm, in CI); TLS 1.3 test vectors (RFC 8448) where the provider API allows.

## TD-AUTH-OPAQUE-SETUP-ROTATION

* **Status:** open
* **Context:** The OPAQUE server setup (auth.md §8.1), the OPRF seed and the server's AKE key pair, is created once per cluster and never replaced. Every OPAQUE record, and every keyslot opened by an OPAQUE export key, depends on it. There is no way to rotate it, for example after a backup that holds it leaked, without every OPAQUE user registering again.
* **Why deferred:** A record can't be moved to a new OPRF seed without the user's password: the client must run a new registration. Rotation therefore means keeping two setups during a migration window and re-registering each user at their next sign-in, which needs a protocol and client support of its own. The stated recovery (users register again from another method's session) works meanwhile.
* **What it would take:** Setups versioned in the keyspace (`pack("auth_key", "opaque", n)`), the setup version in each credential record, sign-in with the record's version, an admin command that creates a new current version, a `/v1/auth/opaque/login/finish` response flag that asks the client to re-register (it holds the password at that moment), the export-key keyslot re-wrapped by the client at the same time (auth.md §8.6), dropping an old version once no record uses it, and tests across the switch.

## TD-AUTH-OPAQUE-VECTORS

* **Status:** open
* **Context:** zen-serve's OPAQUE suite (auth.md §8.1) is RFC 9807's ristretto255-SHA512 configuration, but with an Argon2id KSF at the credential's parameters and a context of its own (`"zen/v1/opaque" ‖ 0x00 ‖ origin`). `opaque-ke` checks itself against the RFC's vectors, which use the identity KSF. There are no vectors for the zen-specific combination, so a client written without zen-core (for example in JavaScript) can only be checked against a running server. The export-key keyslot (formats.md §6, type 5) has vectors.
* **Why deferred:** Deterministic transcripts depend on how the library draws its random scalars and nonces from the RNG, so vectors generated through zen-core would partly describe `opaque-ke`'s internals rather than the protocol. Interoperability tests against a second implementation are the useful check.
* **What it would take:** Vectors in the RFC's format (fixed blinds, nonces, ephemeral keys and server setup, injected through the library's test hooks or a second implementation), with the Argon2id KSF at the formats.md §6 floor and a fixed origin, covering registration, a sign-in, the export key and a type-5 slot made from it; and a check of an independent client implementation against them.
