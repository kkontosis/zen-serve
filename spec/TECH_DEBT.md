# Technical debt

Work that was knowingly deferred. Each entry has a **stable reference name**, `TD-<AREA>-<SLUG>`, that code comments, specs and issues can cite. An entry is never renumbered or renamed; when it is done, its status becomes `resolved` with a pointer to the change, and the entry stays.

Fields: **Status** (`open`, `in progress`, `resolved`), **Context**, **Why deferred**, **What it would take**.

## TD-AUTH-INVITES

* **Status:** open
* **Context:** Adding a member is an admin-signed ACL change (formats.md §9): the admin needs the new user's public identity, and for device keys a device certificate, before the user can sign in by any method. There is no invite link, no one-time enrolment code and no self-service onboarding.
* **Why deferred:** An invite flow needs a design of its own: who may invite, how an invitee proves possession of the invite, how the invitee's identity reaches an admin for signing, and how this fits the rule that the signed ACL is the only source of membership. The multi-method sign-in work (auth.md) did not need it.
* **Decision until then:** a user who will sign in only with a password needs an existing session of their own to register the first password (auth.md §11.2), for example a device-key session, which needs a device certificate in the ACL. There is no session-less first registration.
* **Intended solution: one-time enrolment codes.** An admin or an existing member issues a single-use code for a member; the new member presents it, without a prior session, to register their first credential (for example a password-derived key, or a passkey). The code is stored only hashed, expires, and is spent by the registration.
* **What it would take:** An invite or enrolment record in the keyspace (hashed one-time secret, expiry, issuer, the member or intended grants); an unauthenticated endpoint where the invitee presents the code with their first credential, or with their public identity when they are not a member yet; for new members, a pending-members list that admins read and sign into the next ACL version; spec in auth.md and api.md; tests for expiry, reuse and revocation.

## TD-AUTH-WEBAUTHN-PRF-KEYSLOT

* **Status:** resolved, by the commit "spec: WebAuthn PRF keyslot (formats.md §6, type 4)": keyslot type 4 in formats.md §6 and zen-core (`keyslot::create_webauthn_prf`), the flows in auth.md §7.7, vectors in `spec/test-vectors/prf_keyslot.json`.
* **Context:** Passkeys (auth.md §7) only give server access. The WebAuthn PRF extension can return a per-credential secret on the client, which could unlock data keys the way a passphrase does.
* **Why deferred:** It needs a new keyslot type (formats.md §6) and passkeys are not implemented yet.
* **What it would take:** A keyslot type 4 whose secret is the PRF output for a fixed, per-slot salt; the slot stores the credential id and the PRF salt; new labels; vectors; and a fallback when the authenticator has no PRF support. (Done without new labels: WebAuthn already domain-separates the PRF input. The fallback is the user's other keyslots.)

## TD-AUTH-WEBAUTHN-RS256

* **Status:** open
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
* **Context:** The failed-sign-in limiter of method 6 (auth.md §11.3) is in each node's memory. A cluster of n nodes allows n times the configured failures, and a restart clears the counts.
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

* **Status:** open
* **Context:** Native TLS (operations.md §8) runs on a rustls crypto provider of zen-serve's own on RustCrypto. It signs and verifies ECDSA (P-256, P-384) and Ed25519 only. A server key on RSA can't be loaded; a client certificate signed by an RSA CA, or a client with an RSA key, can't sign in natively (auth.md §10.1). Many organisations' client CAs are RSA.
* **Why deferred:** The same as `TD-AUTH-WEBAUTHN-RS256`: the pure-Rust `rsa` crate for this RustCrypto generation is only a release candidate, and ring or aws-lc-rs would bring C and assembly. The trusted-proxy mode (auth.md §10.2) accepts RSA certificates, since the proxy verifies them.
* **What it would take:** Once `rsa` 0.10 is released: RSA-PSS and PKCS#1 v1.5 verification (SHA-256/384/512) in `tls::provider`'s `WebPkiSupportedAlgorithms`, with a floor of 2048 bits; RSA server keys (PKCS#1 and PKCS#8) with RSA-PSS signing; tests with generated RSA CAs and keys.

## TD-TLS-ACME

* **Status:** open
* **Context:** Native TLS reads `[tls] cert` and `key` once, at start-up (operations.md §8.1). Renewing a certificate needs a restart, and obtaining one is up to the operator.
* **Why deferred:** ACME (RFC 8555) needs an HTTP-01 or TLS-ALPN-01 responder, account keys, storage of certificates shared by a cluster's nodes, and renewal scheduling: a feature of its own. A restart after renewal, or a reverse proxy that does ACME, covers the need meanwhile.
* **What it would take:** First, reloading: a `ResolvesServerCert` that re-reads the files on change (or on SIGHUP), keeping the old certificate when the new files don't load. Then ACME with TLS-ALPN-01 on the API port, account key and certificates in the keyspace so every node serves the same one, one node renewing under a lease, and tests against a local ACME test server (Pebble).
