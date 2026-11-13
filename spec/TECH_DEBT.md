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
* **Context:** In the default (ring) build, the `X25519MLKEM768` hybrid group is zen-serve's own (`crates/zen-tls-rustcrypto`), and its X25519 half runs on `x25519-dalek`, while ring's own X25519 group serves plain `X25519`. The hybrid could compute its X25519 share and secret with ring instead, leaving only ML-KEM and the share/secret concatenation as zen-serve's code in that build.
* **Why deferred:** The hybrid works and is tested against itself and, through the shared X25519 group, against ring; moving its X25519 half is a refinement that narrows `TD-TLS-PROVIDER-AUDIT` rather than a fix.
* **What it would take:** A ring-build variant of the hybrid group that drives `rustls::crypto::ring::kx_group::X25519` (`start`/`complete`) for the classical half and `ml-kem` for the rest, the same share and secret layout (draft-ietf-tls-ecdhe-mlkem), handshake tests between the two builds' hybrids, and narrowing `TD-TLS-PROVIDER-AUDIT` accordingly.

## TD-TLS-PROVIDER-AUDIT

* **Status:** open
* **Scope:** narrowed by the commit "spec: the ring TLS provider, on by default, and RSA server keys (operations.md §8, auth.md §10.1)" to (1) the **pure-Rust build**'s provider, and (2) zen-serve's own glue in the **default build**: the `X25519MLKEM768` hybrid and the RSA policy wrapped around ring's verification and key loading (`tls/ring.rs`). Everything else in the default build is rustls's ring provider.
* **Context:** `crates/zen-tls-rustcrypto` (`tls::rustcrypto` in zen-server) connects rustls to RustCrypto: the AEAD record layer, HKDF and HMAC, the key exchanges (including the X25519MLKEM768 hybrid, which the default build uses too), signature verification (ECDSA, Ed25519, RSA) and signing. zen-server's `tls/ring.rs` and the crate's `rsakey.rs` add the RSA key policy to ring's. Mistakes there could break TLS's confidentiality or authentication for native TLS and for mTLS sign-in. Its tests are known-answer tests for HMAC and HKDF, for HKDF-Expand-Label and for records (RFC 8448, and vectors from an independent implementation), round trips and tamper checks, handshakes on one provider, and, in the default build, handshakes between the two providers (every shared group and suite, every key type), which check the pure-Rust provider against ring's. Since milestone 5.5 step 7 the provider also has TLS 1.2 suites (feature `tls12`) and a byte-level record API, both used by zen-svc's TLS engine, which is tested against OpenSSL 3.0's `s_client` for every 1.3 and 1.2 suite on X25519 and P-256. The hybrid has no independent peer in the tests: both sides are zen-serve's, and OpenSSL 3.0 has no ML-KEM.
* **Why deferred:** An independent review needs people outside this work. Deployments that need assurance meanwhile can use the default build, or terminate TLS at a proxy (operations.md §8.3).
* **What it would take:** A security review of `crates/zen-tls-rustcrypto` (for the default build, its hybrid group only; `src/lib.rs` and `src/rsakey.rs`) and zen-server's `tls/ring.rs` against rustls's own providers and the RFCs (8446 record protection and key schedule inputs, draft-ietf-tls-ecdhe-mlkem share and secret layout, RFC 8017 verification, key and point validation); interoperability tests against independent implementations (for example OpenSSL 3.5 or BoringSSL clients and servers, with the hybrid, every suite and each signature algorithm, in CI); TLS 1.3 test vectors (RFC 8448) where the provider API allows.

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

## TD-LOG-RETENTION

* **Status:** open
* **Context:** Event-log appends (`log`, `lk`, `gl`, `lp` keys) are never deleted. A busy topic grows without bound; nothing in zen-serve frees it (docs/STATS.md §2–3).
* **Why deferred:** Retention interacts with consumer cursors (a group behind the retention point must not lose events silently), per-key ready lists, DLQs and the partition index, so it needs its own design.
* **Note (milestone 4.5):** zen-db's change events, request/reply inboxes and message-body parts (spec/zendb.md §10.3, §12) add to the log and make this more pressing.
* **What it would take:** Per-topic retention (age or size) in the group-aware sense: trimming only below the slowest group's cursor unless forced, a `retained_from` offset per topic that readers below it get a clear error for, the sweeper trimming `log`/`lk`/`gl`/`lp` and quota counters together, an admin trim command, and tests with lagging consumers.

## TD-FS-LARGE-FILES

* **Status:** resolved, fully, by milestone 8 (docs/MILESTONE-8.md). Milestone 5 steps 7f and 7g paged files past the inline limit (immutable chunk-id pages listed with their digests in a sealed version-2 manifest, about 640 GiB at the defaults) and made per-file keys a policy. Milestone 8 added the rest:
  * content-defined chunks (manifest version 3, page form 2, the keyed chunker; spec/formats.md §11.3, §11.6, §11.7), the default policy, so an insertion re-uploads a chunk or two (step 1, 3)
  * upload sessions past `chunk_grace_secs` (api.md §12.1; steps 2, 3), used by the replica's sender (step 6) and Loro (step 5)
  * edits and appends that cost the pages they touch (`Tree.edit`), pipelined commits, `readStream` read-ahead, and a compact resumable upload state (step 3)
  * the replica's file handles in bounded memory, edited by page, with binary per-page index records (step 4), and its memory bounds (step 6b)
  * repacking as the fill rule (`edit` repacks when pages would overflow), instead of a background repack: pages are never left fragmented by appends
  * multi-GiB measurements (docs/STATS.md §7), which found and fixed the embedded store's large values, the fixed chunk size and the server's memory (step 7)
  * CDC files past about 196 GiB are TD-FS-CDC-HUGE.
* **Context:** A file version's manifest and chunk list must fit one value: 32 B per chunk within `max_value_bytes` (90,000 by default), so a version holds at most ~2,800 chunks of 64 KiB, about **175 MiB** (docs/STATS.md §1.3). Raising `max_value_bytes` helps only up to FoundationDB's 100 KB value limit (~195 MiB).
* **Why deferred:** The server-side CRDT filesystem (milestone 3.5) targets family-scale files; large media and disk images need a different version layout.
* **What it would take:** Chunk lists split over several keys (e.g. `pack("tf", fs, tree, node, dot, i)` pages, or a tree of manifests), the write op carrying a list reference instead of the whole list, refcounting and GC over the pages, a matching formats.md change for the sealed manifest, and tests with multi-GiB synthetic files.
* **Progress:** milestone 5 step 7f built the server side: pages, the paged `write`, version-2 manifests with file keys (spec/formats.md §11.3–11.6, fs.md §4). Step 7g moved the client onto them, and made file keys a policy of the fs.
* **Plan (milestone 5):** steps 7f/7g build two-level pages (immutable, server-visible chunk-id pages listed with their digests in a sealed v2 manifest, about 640 GiB at the defaults) and per-file content keys; the rest (upload sessions, repacking) is milestone 8 (docs/MILESTONE-8.md).

## TD-FS-SPARSE

* **Status:** open
* **Context:** zen-fs files have no holes. A file grown by `truncate` (or a write past its end) is written with its zeros as content: a replica's handle keeps the gap as a zero extent in memory (milestone 8 step 4), but its flush seals and uploads the zeros, so a sparse 1 GiB image file costs 1 GiB sealed, sent and stored. Under CDC, long zero runs are cut at the chunker's `max`. Measured in milestone 8 step 6 (docs/BLK.md §8), where it is why an image kept as a file stores its whole size and a volume only what was written.
* **Why deferred:** A hole is a format change (manifests, readers, the writer, the replica), out of place in a step about zen-ublk.
* **What it would take:** a zero-extent entry in v3 manifests and pages (a length with no chunk, formats.md §11.3 and §11.6), which readers fill with zeros and the server never references; the writer emitting one for whole zero chunks (detected when cutting); the replica's handle passing zero extents through (`Edit.zeros`) as holes; `SEEK_HOLE`/`SEEK_DATA` in zen-mount; vectors; and a leakage note (the server sees where the holes are and how long they are).

## TD-FS-CDC-HUGE

* **Status:** open
* **Context:** A version-3 (content-defined) manifest lists its pages in one sealed value: 56 B per page within `max_value_bytes`, so 1,248 pages of up to 4,499 chunks, about **196 GiB** at the measured mean chunk (36.5 KiB), and 98 GiB at the fill rule's worst (docs/STATS.md §1.3). Version 2 (fixed chunks) reaches about 640 GiB, and the writer picks it for a larger file whose size it knows; a stream of unknown size past the CDC capacity fails `too_large`.
* **Why deferred:** Files that size are rare in the target uses (family-scale media, disk images, which want fixed chunks anyway: spec/OPS.md §7), and two levels of pages are a format change across the writer, the reader, the replica's records and the server's GC.
* **What it would take:** a manifest form whose pages list pages (`id ‖ digest ‖ u64 bytes ‖ u32 count` per child), so a version holds 1,248 × ~1,700 pages; the server counting references through both levels; the writer's fill rule and repack per level; `edit` touching one page per level; vectors and tests at small `max_value_bytes`.

## TD-MEMORY

* **Status:** open, a standing list. RAM use is a first-class cost (spec/OPS.md §6), and this entry tracks where memory goes, what bounds it, and what isn't measured yet. Fixes go in by step; the list stays.
* **Bounded, with a setting and a test:**
  * a replica's file handle: `dirtyBytes`, 64 MiB (milestone 8 step 4)
  * the replica's records waiting to be written: `pendingBytes`, 32 MiB (step 6b)
  * the client's page memo: `pageMemoIds`, 65,536 ids, about 12 MB, and `pageMemoPages`, 256 (step 6b)
  * pinned content fetched in windows of 256 chunks (step 6b); content fetched for reads isn't kept past `pendingBytes`
  * `readStream` read-ahead and pipelined uploads: a few windows or commits each
  * **a written file's chunk index** (step 8): packed, about 52 B a chunk; it was about 390, and all of the client's live growth under a large file (11 MiB per GiB). `chunkindex.test.ts` bounds it at 120 B
  * **the server** (step 7): the embedded backend's page cache (`[storage] cache_bytes`, 256 MiB), commit bodies in flight (`limits.commit_buffer_bytes`, 64 MiB), 2 malloc arenas, and one copy of each value in a transaction. Measured: 587–637 MiB under one 4 GiB writer and 691–712 MiB under 16 (1.9–3.1 GiB before; docs/STATS.md §7.3)
* **Not bounded, or not measured yet:**
  * **Decoded pages as objects:** a v2 page decodes to 5,624 separate `Uint8Array` ids (wasm's `decodeAnyPage`), about 1 MB of heap per 90 KB page, and every edit of a large file decodes and builds a few of them. A compact form (one buffer of ids, lengths in a typed array) would cut that about 10×.
  * **wasm linear memory** never shrinks: one large operation sets the floor for the process's life.
  * **zen-mount under an image workload:** after step 6b, the peak is what the benches show (docs/BLK.md §8); the JS heap's churn from page decoding is most of what is left. A large file through it peaks at 465 MiB writing and 296 MiB reading (STATS.md §7.3).
  * **The server, what's left:** a signed-in client that stalls a commit's body holds its share of `commit_buffer_bytes` until the connection drops (no body read timeout), so enough of them make the node's commits wait; relayed connections' windows (STATS.md §2.1); FoundationDB's client library, measured at 1,091 MiB in zen-serve under 16 writers (206–324 MiB under one; STATS.md §7.1), with no setting of ours. The cache default is a hedge: large-file throughput was the same from 64 MiB up, and metadata-heavy loads weren't measured.
  * **The client's buffers under many uploads:** 3 commits of up to `max_commit_bytes` in flight per upload, so 16 uploads in one process held 1.4 GiB of buffers (STATS.md §7.3). A process-wide budget would bound it.
  * **The Loro kept state:** a document's whole snapshot is one record in memory (zendb.md §20).
  * **zen-ublk under an image workload:** 373 MiB peak RSS on CI's runner, against 56 MiB for the FUSE frontend under the same load (docs/BLK.md §8; measured only there, since the development container has no ublk). Not explained yet. To check first: the ublk queues' I/O buffers (queue depth × the largest I/O, per queue) and the write batch's buffer.
  * **The browser:** IndexedDB-backed replicas under the same workloads.
* **What it would take:** memory figures for every frontend and the server as part of each milestone's benches (milestone 8 step 7 was the first: STATS.md §7.3), a bound and a setting for each item above, a body read timeout (or a per-caller share of the budget), and the compact page form.

## TD-STORE-COMPACTION

* **Status:** open
* **Context:** The embedded backend's `zen.redb` file never shrinks: redb reuses freed pages but zen-serve never calls its compaction, so the file stays at its peak size after large deletions.
* **Why deferred:** Compaction needs exclusive access to the database file (no open read transactions), which conflicts with the MVCC read window the embedded backend keeps open while serving.
* **What it would take:** An offline `zen-serve compact` command (servers stopped) calling redb's compaction, or an online path that briefly drains read transactions; a note in operations.md; a test that deletes data and checks the file shrinks.

## TD-FS-HEADER-SELF-SLOT

* **Status:** open
* **Context:** Only an admin may write the fs header (api.md §4.4), so a member can't add a keyslot for their own new passkey or password, nor re-wrap their OPAQUE slot after a password change. The client prepares the slot and an admin adds it (formats.md §12).
* **Why deferred:** Letting members write the header needs a rule the server can enforce on opaque bytes: for example, a member may append or replace slots whose credential id is one of their own credentials, and nothing else. Milestone 4 (the client library) did not need it.
* **What it would take:** Either a structured header endpoint (`fs/slots/put {fs, slot}`, `fs/slots/remove`) where the server checks that the slot's credential id or recipient fingerprint belongs to the caller, or a per-member header section; spec in api.md and formats.md §12; tests for a member adding, replacing and failing to remove another member's slot.

## TD-CLIENT-WASM-OPT

* **Status:** open
* **Context:** The WASM module of `@zen/client` is 1.86 MB (525 KB gzipped), built with the release profile and not post-processed.
* **Why deferred:** `wasm-opt` (binaryen) isn't part of the build, and size wasn't a milestone-4 goal.
* **What it would take:** Run `wasm-opt -Oz` in `scripts/build-wasm.sh` when present (or a size profile with `opt-level = "z"` for zen-wasm), measure the size and the Argon2id and ML-DSA timings before and after, and update docs/STATS.md §4.1. Consider splitting OPAQUE into a second module loaded on demand. The same applies to the second module, `@zen/svc-wasm` (the TLS and SSH engines, 1.23 MB, 465 KB gzipped, milestone 5.5 step 10), with the TLS and SSH throughput measured before and after.

## TD-FUSE-REPLICA

* **Status:** resolved, by the commit "zen-mount on @zen/fs (milestone 5 step 8c)": `zen-mount` mounts a replica (`@zen/fs`): offline use, a sealed `--cache-dir`, pins, symlinks, xattrs, writes of changed chunks only, and the vault for sign-in. Left as documented limits (docs/CLIENT.md §9): no hard links, no `mmap` coherence across devices, inode numbers from the kernel.
* **Context:** `zen-mount` (packages/fuse) has no local replica: it lists directories from the server (cached until the change feed reports a change), reads file contents by chunk range, and writes a whole new version when a file is closed or synced. It is not POSIX-complete: no hard links, symlinks or xattrs; `mmap` isn't coherent across devices; there is no offline use and no persistent chunk cache (`--cache-dir` is reserved).
* **Why deferred:** The local replica and the POSIX layer over it are the zen-fs client of milestone 5; milestone 4 needed a working mount on the operations API.
* **What it would take:** The milestone-5 replica (node table and chunk store on disk, synced through the change feed), then `zen-mount` on top of it, with symlinks (the `symlink` node type exists), xattrs in the sealed meta, and writes of changed chunks only.
* **Progress:** the replica is built (`@zen/fs`, milestone 5 step 8a: the node table, content on demand, pins, offline reads, the three stores) and takes local writes (8b: queued offline, file handles writing changed chunks only, symlinks, xattrs); `zen-mount` moves onto it in 8c.

## TD-BROKER-SERVER-TIMERS

* **Status:** open
* **Context:** The server has no timers or scheduled delivery. zen-db's delayed messages (spec/zendb.md §12.4) are rows in a `$sched` table that a scheduler leader inside an app polls and emits, so a message waits while no instance runs a scheduler, and is late by the poll interval plus any leader takeover.
* **Why deferred:** Milestone 4.5 builds the broker from the existing primitives only; the polling leader is correct (exactly once, never early) and enough for family-sized apps.
* **What it would take:** A `deliver_at` on appends (or a per-fs delay index) that the server makes visible to readers and groups only once due, driven by the commit-version clock (DESIGN-3 §3.1) so no wall clock is trusted; wake-ups through the existing watches; api.md and keyspace.md changes; tests across nodes and a failover. The zen-db scheduler would then become a thin wrapper.

## TD-CONSUME-COMPETING

* **Status:** open
* **Context:** Unkeyed events can be spread over any number of workers only by giving each message its own key in a `per_key` group (spec/zendb.md §12.2). Every such key leaves a `kc` entry (keyspace.md §3.3) that is never deleted, so a busy queue grows server state per message. `partitioned(N)` avoids that but caps parallelism at N.
* **Why deferred:** DESIGN-2 §2.4 deferred competing consumers; the per-key workaround and partitions cover family-scale queues.
* **What it would take:** Either a `shared` group mode (claims on individual events, a low watermark, no per-key cursor), or garbage collection of `kc` entries whose key has no pending events and whose claim token is no longer needed for fencing; api.md §8 and keyspace.md changes; tests with many workers and lost claims.

## TD-CONSUME-PUSH

* **Status:** open
* **Context:** DESIGN-4 §1.4 sketched `WS /v1/stream {consume: group}`, with the server pushing ready events and claim tokens. It was not built: consumers long-poll `/v1/consume/next` (`wait_ms` up to 30 s, api.md §8.3), so each idle consumer holds an HTTP request open.
* **Why deferred:** Long-polling gives the same delivery semantics, and the stream's subscription machinery covers only reads.
* **What it would take:** A `cons` stream operation that leases or claims and pushes deliveries, with the same tokens and gate as `next`; flow control (`max_inflight`); reconnect semantics that never hand out an event twice to live holders; api.md §9 changes and tests next to the `next` ones.

## TD-CRDT-ROWS-SERVER

* **Status:** resolved (milestone 5 step 3): `crates/zen-server/src/crdt.rs`, the operations and reads of api.md §13, the keyspace of keyspace.md §3.8, the sweeper purge, and the property test of `crates/zen-server/tests/crdt.rs` on both backends.
* **Context:** zen-db CRDT tables (spec/zendb.md §19) need server-merged row objects: last-writer-wins row and field registers, per-device counters and add-wins sets, with the `row`, `lww`, `ctr`, `add` and `rem` operations in `crdt_ops`, the `/v1/crdt/get` and `/v1/crdt/range` reads, and a `"crdt_rows"` feature flag. The server implements only the filesystem's CRDT objects (spec/fs.md).
* **Why deferred:** Milestone 4.5 is specs only. Scheduled for milestone 5 (DESIGN-3 §6), moved forward from 7, so that the offline variants of the app examples can be demonstrated with zen-db.
* **What it would take:** The operations and their merge rules inside the commit transaction, the keyspace of zendb.md §19.5 (moved into keyspace.md), the sweeper purge of deleted objects past the horizon, the reads, api.md §6 and §12-style sections, zen-proto types, and the property test of §19.9 against an in-memory reference, on both backends.

## TD-DB-PERSISTENT-CACHE

* **Status:** resolved (milestone 5 step 7e)
* **Context:** zen-db's caches (catalog records, rows, private-index nodes, sealed blobs, CRDT states) were kept in memory per `Db` instance (spec/zendb.md §9.3). A new page load started cold, and nothing opened offline.
* **Resolution:** the sealed local store (step 7d: IndexedDB, Node disk, memory; a device layer under a non-extractable key, an optional fs layer) holds the queue of CRDT writes and, since step 7e, every cache, written behind and checked on use by version or content (§9.3, §7.7). `unlock({local})` and `Db.open` work offline from the kept header, info, DbRecord and catalog (§3.1). `persist: false` keeps the caches in memory; `Db.wipe` and `Fs.forget` clear them at sign-out.

## TD-DB-TAB-PROXY

* **Status:** resolved in milestone 5 step 6c (`packages/db/src/proxy.ts`, spec/zendb.md §9.2): the owner tab holds the only stream and forwards subscriptions (log events sealed, ephemeral messages opened) over a `BroadcastChannel`; consumers, `serve`, the scheduler and saga orchestrators run in the owner. Transactions stay local in every tab, as decided, so no transaction protocol was needed.
* **Context:** zen-db elects one owner tab per database with Web Locks (spec/zendb.md §9.2, `packages/db/src/tabs.ts`), but other tabs still run their own transactions and will run their own subscriptions instead of sending them to the owner over a `BroadcastChannel`.
* **Why deferred:** Transactions are serializable on the server, so running them in every tab is correct. Forwarding only saves connections and duplicate work, and it matters once subscriptions exist (milestone 5 step 6).
* **What it would take:** A message protocol over `BroadcastChannel` (request, result, error, subscription events, cancel), which carries transaction functions as named operations because closures can't cross tabs; a fallback to running locally while no owner answers; and tests with simulated tabs. Resolved in step 6 if it lands there.

## TD-DB-OBLIVIOUS-INDEX

* **Status:** open
* **Context:** spec/zendb.md §5.8 specifies an `oblivious` index (a B+tree inside Path ORAM) that hides access patterns. `createIndex` refuses `kind: "oblivious"` with `format`.
* **Why deferred:** Milestone 5 builds the unique, fast, private and sealed kinds. ORAM is the costliest one (every access, even a read, rewrites paths) and needs its own tests of stash bounds.
* **What it would take:** The bucket, Block, head and stash encodings of §5.8 with their vectors (§17), the access with eviction, a B+tree over ORAM blocks, the stash bound under long random runs, and the property test that it answers like `private`.

## TD-DB-SHAREDWORKER

* **Status:** open
* **Context:** spec/zendb.md §9.2 says that where `SharedWorker` exists, the owner is a shared worker instead of a tab. zen-db elects a tab.
* **Why deferred:** It needs the tab proxy (TD-DB-TAB-PROXY) first, plus a worker bundle with the wasm module.
* **What it would take:** A worker entry point that opens the `Db` and serves the proxy protocol, a page-side client that prefers it when `SharedWorker` exists, key handling (unlock material passed to the worker once), and browser tests.


## TD-LORO-LARGE-SNAPSHOT

* **Status:** resolved, by the commits "spec: Loro snapshots in zen-fs files past half a commit (zendb.md §20)" and "loro: snapshots larger than half a commit go to zen-fs files (milestone 8 step 5)": a snapshot past half of `max_commit_bytes` is a file in a tree of the document's own, and the `$loro` row names its version by the hash of its sealed manifest (zendb.md §20.1, §20.2, §20.5); tests in `packages/loro/test/loro.test.ts`.
* **Context:** A Loro document's snapshot is a row of the `$loro` system table (spec/zendb.md §20.5). A row is split into parts, but it must fit one commit (`max_commit_bytes`, 8 MB by default). A larger snapshot keeps the old one, and the client stops compacting that document, so loads read ever more of the log.
* **Why deferred:** Documents of the milestone 5 apps (card texts) stay far below the limit, and a file-backed snapshot needs the replica's write path, which landed only in step 8.
* **What it would take:** Store a large snapshot as a zen-fs file (paged versions, uploaded in commits of its own), with the `$loro` row holding the file's node and version instead of the bytes; compaction then replaces the file's version; loads stream it.

## TD-EPHEMERAL-SAME-DEVICE

* **Status:** resolved, by the commits "spec: ephemeral replay checks per publishing stream" and "client, db: closes fence work in flight, ephemeral replays per stream, `on` from the start" (milestone 5 step 11): an ephemeral message carries its stream's id in the causation field, and receivers check replays per `(topic, sender, stream)` (api.md §9.2); test in `packages/client/test/stream.test.ts`.
* **Context:** The stream drops an ephemeral message as a replay unless its HLC is newer than the last one seen from its sender device on that topic (`packages/client/src/stream.ts`, `openEphemeral`). Two sessions of one device keep separate clocks, so they can drop each other's ephemeral messages: presence replies and typing indicators get lost. Found by the chat app (milestone 5 step 10), whose test uses one device per instance.
* **Why deferred:** Found while integrating the apps; one device per running instance is the normal case.
* **What it would take:** Key the replay filter by sender session (an id in the ephemeral envelope), or share one clock store per device across its sessions, with a test of two sessions of one device exchanging ephemeral messages.

## TD-SAGA-HOOKS

* **Status:** open
* **Context:** The checkout app (milestone 5 step 10) needed more from `db.saga` than it offers:
  * a transition hook (`onTransition(row, tx)`), so the orchestrator writes app rows (the order's status) in the same commit. The app copies the state with a second consumer group instead, one commit behind.
  * a participant-side check that its command is still current and its timeout pending, and an exported reply helper. The app reads `$sagas` and `$sched` and builds the reply itself; without the check, a step done after its timeout fired is never compensated.
  * consumer options (`waitMs`, `maxAttempts`, `maxInflight`) passed through to the orchestrator's groups.
* **Why deferred:** The app works around each; the API shape wants a second user before it is fixed.
* **What it would take:** `SagaOptions.onTransition`, `saga.isCurrent(tx, msg)`, `tx.reply(msg, body, {error})` exported for consumers, `SagaOptions.consume`, and tests in `packages/db/test/saga.test.ts`; then the checkout app moves onto them.

## TD-CONSUME-CLAIM-WAKE

* **Status:** open
* **Context:** A consumer waiting in a long poll (`next` / `deliveries`, `waitMs` default 25 s) isn't woken when a killed consumer's claims expire, so those messages wait for the poll to end. The checkout app sets `waitMs: 2000` on its groups.
* **Why deferred:** Found while integrating the apps; a shorter `waitMs` works around it.
* **What it would take:** The server wakes waiting polls of a group when a claim it holds expires (a watch on the claim index, or a timer at the earliest expiry), or the client defaults to a shorter wait; a test with a killed consumer and a waiting one.

## TD-SCHED-HOTSPOT

* **Status:** open
* **Context:** `$sched.by_at` is one private index with one shard, and every schedule and cancel writes it. Every saga transition schedules or cancels a timeout, so concurrent sagas conflict on it (seen in the checkout app's 100-order test).
* **Why deferred:** Correct as is; it costs retries under load.
* **What it would take:** Shard `by_at` (`shards: K`, a merge in the scheduler's due-rows query, which already reads through the planner), measured in `docs/STATS.md`.

## TD-DB-ON-FROM-START

* **Status:** resolved, by the commit "client, db: closes fence work in flight, ephemeral replays per stream, `on` from the start" (milestone 5 step 11): `OnOptions.start: 'now' | 'earliest'` (docs/DB.md §11); the ledger app uses it.
* **Context:** A durable `on(..., {cursor})` with a new cursor starts at new events. To read a topic from its beginning (the ledger's audit view), an app passes `after: offsetAfterVersion(0n)` from `@zen/client`.
* **Why deferred:** Minor; the workaround is one line.
* **What it would take:** An `OnOptions.start: 'now' | 'earliest'` as consumer groups have, documented in docs/DB.md §11.

## TD-DB-CLOSE-INFLIGHT

* **Status:** resolved, by the commit "client, db: closes fence work in flight, ephemeral replays per stream, `on` from the start" (milestone 5 step 11): `Db.close()` waits (at most 5 s) for its running transactions and manual commits, which fail `DbError('closed')` at their next read or commit; calls after it fail `closed`; `UnlockedFs` keys throw `ZenError(0, 'closed')` after `close()`. Tests in `packages/db/test/txn.test.ts` and `packages/client/test/unlock.test.ts`.
* **Context:** `Db.close()` and `UnlockedFs.close()` don't fence transactions in flight. A transaction still running after its fs is closed uses freed WASM keys and fails with WASM's `null pointer passed to rust` rather than a clear `closed` error. Seen in CI on milestone 5 step 10 (a chat test closed an fs while receipt writes it had started were still running; the test now waits for them).
* **Why deferred:** Callers that await their own work never hit it; found while integrating the apps.
* **What it would take:** `Db.close()` waits for (or rejects with `DbError('closed')`) its open transactions and handler runs; `UnlockedFs` checks a `closed` flag before using its keys and throws `ZenError(0, 'closed')`; a test closing during a transaction.

## TD-SVC-STATE-ROLLBACK

* **Status:** closed (milestone 6, step 5), but for the checkpoint window
* **Context:** A relayed connection's state (formats.md §13, kind 9) holds a TLS or SSH engine's keys and sequence numbers. Its authenticated data binds the connection and the cursors, so the server can't pair it with other cursors. But a malicious server can serve an older, consistent snapshot of the whole connection and the handler's database (zendb.md §14). The engine then reuses record sequence numbers; if its new output differs (timestamps, randomness), the server sees two records under one nonce (svc.md §12).
* **Why deferred:** Detecting a consistent rollback needs state outside the server: signed, gossiped checkpoints. That is milestone 6's authenticated tier. Honest failures can't cause reuse, because uncommitted output is never revealed (svc.md §5).
* **Mitigations until then:** A handler aborts a connection when a claim shows cursors lower than it has seen (svc.md §6). TLS 1.2 AES-GCM explicit nonces carry a 32-bit prefix drawn at random for each batch of records (svc.md §7.1), so a rolled-back sequence number repeats a nonce only with probability 2^-32 per batch.
* **What it would take:** Put `(conn, out_head)` in the handler-signed checkpoint of milestone 6 (zendb.md §14), and have a successor refuse a state older than the newest checkpoint it can verify.
* **Narrowed (milestone 6, step 4):** handlers name each connection's `(in_acked, out_head)` in their device checkpoints, checkpoint before a release, and a successor syncs and refuses a claim below the newest verified entry (svc.md §12, docs/SVC.md §6). Left: the handler's database, which can still be rolled back together with a connection whose cursors are within the window, until authenticated namespaces (milestone 6, step 5); and the window itself (steps since a crashed handler's last checkpoint; `sigEvery: 1` closes it).
* **Closed (milestone 6, step 5):** a handler's database opened `integrity: "authenticated"` (zendb.md §14) is checked on every read against its NsRoot, whose floor rides in the same device checkpoints; a step that finds it rolled back gives the connection up (svc.md §12, docs/SVC.md §6). What remains is the window, documented in svc.md §12: steps since a crashed handler's last checkpoint. `sigEvery: 1` closes it.

## TD-SVC-TLS-HANDSHAKE-RESUME

* **Status:** open
* **Context:** The TLS engine (svc.md §7.1) has no state until the handshake is done: rustls's handshake state can't be serialized. A handler that dies mid-handshake leaves a connection that isn't resumable, so the server aborts it (svc.md §6), and the client retries. A handshake step isn't idempotent either: its retry must re-send the same output, and an unknown outcome aborts the connection.
* **Why deferred:** A handshake takes one or two round trips, so the window is small, and TLS clients reconnect. Serializing the handshake means running it in our own code (as the SSH engine does) or rustls exposing its handshake state.
* **What it would take:** A handshake whose state serializes after each flight: either our own TLS server handshake (key schedule, transcript hash, ServerHello to Finished, both versions) checked against rustls and OpenSSL, or an upstream rustls API. Then `resumable` from the first step, as for SSH.

## TD-SVC-SSH-SCOPE

* **Status:** open
* **Context:** The SSH engine (svc.md §7.2) is the subset that OpenSSH clients and git need: `publickey` authentication with Ed25519 and RSA (SHA-2) user keys, `session` channels with `exec`, `shell` and `subsystem`, and one Ed25519 host key. It has no password or keyboard-interactive authentication, no PTY (`pty-req` is refused, so interactive shells get no terminal), no port or agent forwarding, no X11, no ECDSA or security-key (`sk-…`) user keys, no certificates, no `publickey-hostbound`, no host-key rotation (`hostkeys-00@openssh.com`), and no `sntrup761x25519-sha512`.
* **Why deferred:** Each one is more protocol and state to get right and keep resumable, and none is needed for git over SSH or command services. Password authentication would also put a secret through the application on every login.
* **What it would take:** Per feature: the messages, their state fields (svc.md §7.2's layout gains them), and OpenSSH tests. A PTY needs `pty-req` and `window-change` to reach the application, and an application that emulates a terminal.

## TD-SVC-SSH-PQ-CI

* **Status:** open
* **Context:** The runners' OpenSSH (Ubuntu 24.04: 9.6) has no `mlkem768x25519-sha256`, which came with 9.9. CI's `openssh-pq` job builds OpenSSH 10.6p1 from source (`scripts/build-openssh.sh`) and runs the SSH engine's OpenSSH tests against it, the hybrid's included (`crates/zen-svc/tests/openssh_pq.rs`). It ran on every push of milestone 5.5 step 9's PR and passed, and now runs only when started by hand (`workflow_dispatch`), so ordinary builds don't spend a minute building OpenSSH. Every build still tests the hybrid against the crate's own test client (`tests/ssh.rs`), which shares the engine's code.
* **Why deferred:** Building OpenSSH on every push costs time for a check that changes only when the engine's key exchange does.
* **What it would take:** Install the runners' `openssh-client` in the Rust jobs and drop the `#[ignore]` once `ubuntu-latest` ships OpenSSH 9.9 or later; until then, start the job by hand after changing `crates/zen-svc/src/ssh/kex.rs` or `cipher.rs`.

## TD-SVC-CONNECT-BACKPRESSURE

* **Status:** open
* **Context:** A `tcp-auth` connector (`Session.svcConnect`, `zen-connect`) reads the service's output from a standard WebSocket, which can't pause reading. When the local reader is slower than the service (a slow local client behind `zen-connect --listen`, a full stdout pipe), what arrives is buffered in the connector's memory, without bound. The other direction is bounded: the connector pauses its local source while more than 1 MiB waits in the WebSocket, and the server reads no more than the service's `window`.
* **Why deferred:** The protocols in view bound it themselves: SSH sends no more than the client's channel windows, and request-response protocols wait for the client. Node 22's built-in WebSocket (undici) and the browser's have no read pause.
* **What it would take:** Flow control in the protocol: the connector grants the server credit (a text frame `"credit <n>"`, api.md §14.9) as its local side drains, and the server writes no more than the credit given; or `WebSocketStream` where it exists. Then a test with a stalled local reader and a fast service.

## TD-SVC-INBOX-ROTATION

* **Status:** open
* **Context:** A service's sealed inbox (svc.md §9) is one X-Wing key pair: the public key in the service's `inbox` setting, the seed with the handlers (the echo app keeps it in a sealed row). Each connection is encapsulated once, at accept, under the key configured then. Changing the key in the configuration affects new connections only; one opened before needs the old seed until it ends. `serve()` takes one seed, so during a change the handlers can't serve both.
* **Why deferred:** Connections are short next to a key's lifetime, and an admin can change the key at a quiet moment, or restart the handlers once the old connections are gone. Nothing in milestone 5.5 needs a change while connections are open.
* **What it would take:** `serve({inbox})` taking several seeds, tried by the claim's ciphertext (X-Wing decapsulation doesn't fail on a wrong key, so a key id: the first bytes of a hash of the public key, stored with `inbox_ct` in `rc.inbox`, keyspace.md §3.9). Then a helper that adds a new seed to the app's row, and a test that changes the key with a connection open.

## TD-SVC-ACME

* **Status:** open
* **Context:** A relayed service that terminates TLS (`tls()`, `starttls()`) uses a certificate and key that the admin obtained and stored in the fs, where the handler reads them before `serve()` (docs/SVC.md §4). Nothing obtains or renews them: an expired certificate is replaced by hand, and the handlers restarted. This is separate from TD-TLS-ACME, which is about zen-serve's own listener: the server never holds a relayed service's key.
* **Why deferred:** Milestone 5.5 builds the relay and its engines; certificates come from outside, as they do for most servers behind a separate ACME client. The apps' tests make their own.
* **What it would take:** An ACME client (RFC 8555) run by a handler, so the account key and the certificate's key stay with the members. The challenge is answered through a relayed service: HTTP-01 through a `tcp` service on port 80, or TLS-ALPN-01 (RFC 8737) by the `tls()` layer on the service's own port, answering `acme-tls/1` with the challenge certificate. Renewal runs under a leader lease (one handler of the fs renews), writes the new pair to the app's row, and `serve()` picks it up for new connections without a restart (a layer that re-reads its certificate). Tested against Pebble.

## TD-SVC-UDP

* **Status:** open
* **Context:** Relayed services are TCP only (svc.md §2: `tcp` and `tcp-auth`). DNS, QUIC (HTTP/3), WireGuard, SIP over UDP and other datagram protocols can't be served.
* **Why deferred:** The relay's model is an ordered byte stream per connection with acks at protocol boundaries, a durable queue and a takeover at a step. Datagrams have no connection, no order and no retransmission by the transport, and their protocols expect replies within milliseconds, below what a commit per step costs (STATS §5.5).
* **What it would take:** A datagram mode with its own model: a flow keyed by the peer's address and port, datagrams stored as records with a bounded queue and dropped (not acked) when it is full, replies released without the key reveal (as `resume: false`), and an idle timeout per flow. QUIC would need its engine in the handler, with connection ids rather than addresses keying the flow. Measure first whether a step's latency suits the protocol.

## TD-MAIL-SCOPE

* **Status:** open
* **Context:** The mail app (`apps/mail`, milestone 5.5 step 13) receives mail for its `mailboxes` and stores it in zen-db. It is inbound only: it checks no SPF, DKIM or DMARC, delivers nothing outbound, has no submission port or SMTP AUTH (members can't send through it), and offers neither `CHUNKING`/`BDAT` nor `SMTPUTF8`. Every member of the fs can read every mailbox: mail is as private as the fs, not per user. Members can also read the app passwords' hashes; the passwords are 160 random bits, so the hashes don't reveal them, but a member could make one for another user. IMAP (step 13b) is a subset: INBOX only (no other mailboxes, `APPEND` or `COPY`); no `ENVELOPE`, `BODYSTRUCTURE` or MIME part sections in FETCH; no keywords, only the system flags; no CONDSTORE/QRESYNC, UIDPLUS or MOVE; SEARCH without `BODY`, `TEXT`, `HEADER`, `BCC` or the `SENT…` keys, matching header fields as sent (no RFC 2047 decoding); no STARTTLS on port 143, only implicit TLS; no keepalive during long IDLEs. A session's view of the mailbox is kept as uid ranges in its state, which is bounded at 32 KiB: a mailbox fragmented into more than about 2,000 runs of uids ends the session with `BYE [ALERT]`. A message expunged by another session in the middle of a FETCH's literal resets the connection, since the literal can't be finished.
* **Why deferred:** The app shows a relayed protocol end to end, with exactly-once storage across handler kills; a mail provider is out of the milestone's scope.
* **What it would take:** Sender checks on receipt (SPF and DMARC from DNS, DKIM signatures over the stored bytes) recorded in a header; for IMAP, a MIME parser over the stored parts (ENVELOPE, BODYSTRUCTURE, part sections, BODY/TEXT search), mailboxes as a table with `APPEND`, `COPY` and `MOVE`, keywords, CONDSTORE (the modseq is there already), and a view kept outside the connection state; a submission service (port 587 with STARTTLS and AUTH against app passwords) and an outbound queue (a consumer group that delivers by MX, with retries and bounces); `BDAT` and `SMTPUTF8` in the handler; and per-user mailboxes as sub-filesystems or per-user keys once zen-db has row-level read rights.

## TD-BLK-SNAPSHOTS

* **Status:** open
* **Context:** A block volume (blk.md) has a fixed size and one history. There are no snapshots or clones (DESIGN.md §5), it can't be resized, and it can't be deleted. A read-only opener holds the root it verified at open: once the writer moves the root, its reads of changed blocks fail with `volume_changed` rather than going on at the old root.
* **Why deferred:** Milestone 6 builds the engine and its freshness; the volume operations around it don't change the formats of blk.md, only add to them.
* **What it would take:**
  * **Snapshots and clones:** a root record that names a parent volume and seq, with copy-on-write of blocks and nodes (a block absent in the clone reads through to the parent at that seq), and a reference count or a sweep before the parent's old values can go.
  * **Resize:** a root record whose `blocks` differs from its predecessor's, with the tree's depth changing at powers of 128 (a new top node over the old one, or the old top's first child when shrinking) and the cut-off blocks deleted in the same commits.
  * **Delete:** paged clears of the volume's keys (a clear over more than `max_range_items` keys is refused), and a tombstone root record that stays, so a server can't resurrect a deleted volume by serving its old root.
  * **Readers at a snapshot:** a read-only opener that keeps the values of its root, which needs the snapshots above, since the writer deletes and overwrites them.

## TD-BLK-SNAPSHOT-FILES

* **Status:** open
* **Context:** `zen-ublk export` (milestone 8 step 6, docs/BLK.md §2) writes a volume to a raw image on the local disk. A volume snapshot kept in zen-fs instead, as a file of the fs, would be a server-side backup and a transport format.
* **For:**
  * Any zen-fs client can read it, without zen-ublk, and it can be mounted read-only through zen-mount and a loop device.
  * It dedupes across snapshots: a second snapshot of a mostly unchanged volume, written with the first as `previous`, stores only the chunks that changed (fixed chunks of a multiple of 4 KiB keep in-place block writes on chunk boundaries).
  * Its versions are covered by the tree's own integrity (signed checkpoints, formats.md §14).
* **Against:**
  * **Consistency:** a consistent snapshot of a live volume needs TD-BLK-SNAPSHOTS, a pinned root whose blocks the writer doesn't overwrite or delete meanwhile. Today `export` fails when the root moves.
  * **Code:** zen-client (Rust) has no tree writer. One would be needed (sealing chunks and pages and the manifest, as `@zen/client` does), or the snapshot made by a TS tool reading the volume.
  * **Storage:** it doubles until the file is removed. Blocks are sealed per stored key and chunks per file, so the two can't share bytes.
  * **Freshness:** the volume's freshness (its root `seq` in checkpoints, blk.md §6) doesn't carry over. The file would need the root it came from recorded (`vol_id`, `seq`, `RH` in its meta), and a checkpoint entry linking the two, for a reader to know it holds the volume as of that root and not an older one.
* **How it could open later:** after TD-BLK-SNAPSHOTS:
  * `zen-ublk snapshot --tree TREE PATH` pins a root and streams the blocks into a Rust `Tree::write` with fixed 64 KiB chunks, the previous snapshot as `previous`, recording `(vol_id, seq, RH)` in the file's meta.
  * `import` takes a zen-fs file as its source, which makes a restore an `import`.

## TD-WITNESS

* **Status:** open
* **Context:** Integrity (formats.md §14, docs/INTEGRITY.md) detects rollbacks and forks up to **fork consistency** (DESIGN.md §2.6): a server that keeps two groups of devices apart forever, so that no checkpoint of one group ever reaches the other, isn't caught, unless users compare their pins out of band (formats.md §14.5). The server is the only channel between devices.
* **Why deferred:** Closing it needs a second, independent channel, which is a deployment decision (who runs it, who trusts it), not a format change. Milestone 6 gives each device what it needs to be caught by one: signed, chained heads.
* **What it would take:**
  * **A witness:** a log outside the server, such as another zen-serve run by someone else, or a transparency log. After each checkpoint a client posts its device head `(fs, fp, seq, HC)` to it (signed: the checkpoint's signature covers the body, so the witness can store the signed head), and at each sync it reads the other devices' heads from it. A head the server doesn't show, or shows differently, is a fork.
  * **Optionally, waiting for it:** the witness signs the set of heads it has seen, and a client treats a sync as complete only once the witness's set covers the server's answer. That bounds a fork to the time the witness takes to answer.
  * **The fs setting** that names the witness and its key, in the signed header (formats.md §12.1), so the server can't change it.
  * **Tests:** a forked server (`TestServer.fork()`) with an honest witness, where two groups that never share a checkpoint still alarm; and a witness that is down, where syncs continue and report it.

## TD-FS-VERSIONS-INTEGRITY

* **Status:** open
* **Context:** The tree state (fs.md §9.1) commits each node's leaf to its sealed meta, parent, version **count** and change offset, but not to its version records (dots and sealed manifests). A replica fetches those separately by `(node, changed)` (`@zen/fs` `replica.ts` `versionsOf`, `@zen/client` `tree.ts` `versions`) and checks them against nothing. So a server could serve an older, still valid set of version records under a node's current change offset, and the replica would show older content. It can't forge content: every manifest and chunk is sealed.
* **Why deferred:** Found while surveying for milestone 6 step 9. The fix changes the leaf, so the tree states of fs.md §9.1 and their vectors change with it: a format change, out of place in a docs step.
* **What it would take:** a version digest in the leaf, a hash over the node's version records in dot order (each dot and the hash of its sealed manifest), as a leaf version 2 with new vectors (fs.md §9.1, `spec/test-vectors/fs.json`); the server's `NodeState` carrying what the client needs to compute it, or the replica computing it from the records it fetched; the replica checking fetched versions against the node's leaf, with a `tree` rollback alarm on a mismatch; and an adversary test that freezes a version record.

## TD-FUSE-INTEGRITY

* **Status:** resolved, in milestone 8 step 4 (docs/MILESTONE-8.md): `zen-mount --integrity` keeps the pins in the vault's store, else the cache dir's (one is required), signs with the device key file or the vault's device, exits 3 on an alarm at start, answers `EIO` to every operation after one while mounted (`MountOptions.integrity`), and writes a last checkpoint at a clean stop. Tests: an in-process mount of a rolled-back tree (reads, listings, `stat` and writes give `EIO`, where without the option the stale content reads), and the CLI on a forked server (exit 0 and the checkpoint on the original, exit 3 with a `rollback` alarm on the copy).
* **Context:** zen-mount (`packages/fuse`, docs/CLIENT.md §9) never attaches integrity, so its replica runs without tree checks and its writes add no checkpoints. Only the `apps/shared` workers (`ZEN_INTEGRITY=1`), the ledger and zen-ublk (always) attach it.
* **Why deferred:** A feature, not a format: the replica's checks (milestone 6 step 4) work for any caller that attaches integrity.
* **What it would take:** `zen-mount --integrity`, with the pins kept in the mount's store directory and the device key file as the signer; an alarm from the replica surfaced as `EIO` on the mount's files and a log line (the replica already stops its feed); `close` writing a last checkpoint on unmount; and a test with the adversary that a rolled-back tree makes reads fail.

## TD-INTEGRITY-SYNC-COST

* **Status:** open
* **Context:** A `sync()` (TS `integrity.ts` `syncNow`, Rust `zen-client` `sync_now`) costs about two round trips per known device, whether that device's head moved or not: `advance` re-reads the ACL head for each device (`session.acl.head()`, one `/v1/acl/get`), and re-fetches each pinned checkpoint to check it is still the pinned one (formats.md §14.4 rule 2), before fetching a moved device's head. With ten devices that is about 50 ms on loopback (docs/STATS.md §6.1), and it grows with the number of devices, not with what changed.
* **Why deferred:** Found while measuring for milestone 6 step 9, a docs step that changes no behavior. The checks are correct, only repeated.
* **What it would take:** read the ACL head once per sync and pass it down; have `/v1/checkpoint/heads` return each head's checkpoint hash (HC is over the plaintext body, so the server would store it beside the head, given by the putting device), so a device whose head equals its pin is compared without a fetch, and fetched only when the hash differs (to verify the fork it shows); and a `/v1/checkpoint/get` over several devices in one request. A spec change (api.md §15.1). Not a skip of the comparison: a client moved to a forked server finds the fork exactly there (another checkpoint at the pinned seq). Both clients, with a test that counts the requests of a sync where one of ten devices moved, and the existing attack tests unchanged.

## TD-TEST-OPEN-FAILURES

* **Status:** open
* **Context:** CI failures seen once each, during milestone 5.5 step 13a (PR #50), that didn't recur and weren't reproduced locally. Each test now prints what it needs to name the cause when it fails. The step's log in `docs/MILESTONE-5.5.md` has the details.
  1. ~~**The ledger sum**~~ **Resolved (milestone 6 step 3):** reproduced locally once under a full parallel run, where the diagnostic named the transfer that built on the balance before its trader's previous one. Cause: a lost update through the row cache. A trader `Db` runs three trade loops sharing its cache. A transaction read a stale cached row (expecting its version); another loop's commit and read then refreshed the cache, and the transaction's `put` read the row again from the cache, which replaced the expected version with the new one, so the commit built on the stale balance and passed. Now the first version expected of a key in a transaction stands (`Transaction.expectKey`, and zen-db's `useCached`), so such a commit conflicts and re-runs. Tests that fail without the fix: `packages/db/test/txn.test.ts` ("a row read twice while the cache changes in between") and `packages/client/test/kv.test.ts` ("the first version expected of a key stands").
  2. **`push_input`'s 1 s bound** (`crates/zen-server/tests/svc.rs`): the worst of ten pushes took 1.048 s, in the `rust` job of the push run on 50f05a9. The only 1 s wait on that path is the reader's re-check after a batch fails to store, and no failed append showed in 30 loaded runs. On failure the test prints all ten push times and the server's warnings (`storing relayed input` names a failed append). **Recurred** in the `pure-rust` job of run 42545944949 on fecdd1b (milestone 9, a docs-only commit; the other run's `pure-rust` passed on the same commit): 1.160 s, and it was **the first of the ten pushes**, right after the claim; the other nine took 7–61 ms. No `storing relayed input` warning was printed, so it wasn't the reader's re-check after a failed batch. Not reproduced here in 15 idle runs (worst 7–11 ms), 12 runs under CPU load (six busy loops on four cores: worst 8–18 ms), or 30 under disk load (a writer fsyncing 512 MiB at a time: worst 75–165 ms). Under that disk load, `leases_and_takeover` failed in 2 of 30 runs: its 300 ms lease expired before the claim (`not_leader`, `svc.rs:107`), which CI hasn't shown. **Since milestone 9 step 2, the failure message times the slowest push's path,** step by step from the test's write: the reader's socket read, its gather and its append's commit, then the push loop's wake-up (which bell fired), its next turn, the connection's read and the frames sent (`svc::testing::trace`, feature `test-utils`). A forced 1.2 s delay after the lease bell showed as exactly that gap (`7.2 ms lease bell fired`, then `1208.2 ms turn start`). The next failure names the step that stalled.
  3. **Chat, a dropped connection resumes** (`apps/chat/test/chat.test.ts`): timed out waiting for 12 messages, in the push run on 84c8c02. Not reproduced in 45 local runs. On failure it lists the sends, reconnects and arrivals in order, which shows a gap after a reconnect apart from a stall.
  4. ~~**Chat, who's online**~~ **Resolved (milestone 6 step 3):** it recurred in the pull-request run on 6ea8fa8, and its diagnostic showed a lost reply, not a slow one (a different member missing when asked again with 10 s). Cause: `@zen/db` registered a request's waiter only once the asker saw its commit's response, and dropped a reply that arrived before it; the asker answering itself is the fastest such reply. A request in a transaction also didn't wait for the inbox's ephemeral subscription. Both are fixed, with tests that fail without the fixes (`packages/db/test/patterns.test.ts`).
  5. **A leader renews while it runs** (`packages/client/test/log.test.ts`): the second leader won the lease, in the push run on 8806071. With a 300 ms TTL, one round trip over about 200 ms lets the lease lapse, as specified; the TTL is now 1.5 s. On failure the test prints when the holder renewed and when the other won.
  6. **An event that doesn't open** (`packages/db/test/patterns.test.ts`, "retry 'delay': attempts 1..max, then the class DLQ with the same id"): reading the topic's three events (the emit and two scheduled copies) threw `decrypt`, in the pull-request run on ae68e8e (milestone 6 step 4); the push run on the same commit passed. Not reproduced in 21 local runs (the test alone 15 times, the `packages/db`, booking and shop suites 6 times). An AEAD failure on events this client sealed means the stored key token, the topic keys or the epoch differ from those at sealing; reading found no path for either (topics aren't shared between `Db`s, frees zero their wrappers, the scheduler re-derives the key token from the row's key). `Topic.decode` now names the event in the error: offset, topic, kind, epoch, key token and size.
* **Why deferred:** None reproduced. A fix without a cause would be a guess, and a re-run only hides it.
* **What it would take:** When one recurs, read what it printed, find the cause, fix it with a test that fails without the fix, and remove it from this list. The entry is resolved when the list is empty.

## TD-SW-SCRIPTS

* **Status:** open, low priority: kept for later study, discussion and possible revival.
* **Context:** the service worker of milestone 9 serves fs content on a content origin with no scripts allowed (spec/sw.md §4). Content keeps the origin, which is what keeps its pages controlled by the worker. The worker's vault and replica live in that origin's IndexedDB, and no script runs there to read them. Sites that need JavaScript don't work.
  * **The spike** (docs/MILESTONE-9.md, step 9.1, Chromium) tested the obvious way to allow scripts, `sandbox allow-scripts` without `allow-same-origin`, which gives the page an opaque origin. Such a document **isn't controlled by the worker**: only its navigation reached the worker. Its stylesheet, scripts, images, iframes and media went to the server, so the page broke.
  * The isolation itself held: IndexedDB, `caches`, cookies, `localStorage` and the registration all threw `SecurityError`.
* **Why deferred:** each way to allow scripts needs either a server feature that doesn't exist (per-device scopes), more deployment (an origin per site), or a weaker guarantee. Static sites, documents and media cover the milestone's goal. Decided by the user at step 9.1.
* **The alternatives:**
  1. **An origin per site, with scripts and `allow-same-origin`.** Each mount gets an origin of its own (`<mount>.view.example`, wildcard DNS and TLS), whose worker holds a device that can do no more than read that mount's fs. Content can read that origin's vault, so the device must be scoped. That needs per-device rights on the server, intersecting the member's (cf. TD-AUTH-TOKEN-SCOPES, which has the same choice: a server-side scope record, or scopes signed into the ACL). A site could still read every file of its own fs and send it anywhere its CSP allows, so `connect-src` would stay `'self'`.
  2. **Opt-in trusted scripts per mount.** A mount flag that serves its content with scripts on the content origin. Its scripts could read the vault: the device seed, so the member's rights, and the replicas' content keys. Only for content the member wrote themselves, and only if every member who can write the tree is trusted as much. Cheap to build, and easy to misuse.
  3. **A keyless site origin bridged to the app origin.** A site origin with scripts and no credentials. Its worker asks for each file over a `MessageChannel` to a page or worker on the key-holding origin, which checks `event.origin` and serves only that site's mount. It needs a key-holding context reachable from the site's worker; a service worker can't embed one. Storage partitioning applies too: an iframe of the app origin under another site gets partitioned storage, unless the two are same-site. It doesn't work offline unless the app's context runs.
  4. **Opaque sandboxing, if browsers change.** If a browser starts letting a worker control the opaque-origin documents it answers, `sandbox allow-scripts` on one content origin would be the simplest model (spike question 1). Firefox and WebKit weren't tested.
* **What it would take:** for 1, per-device scopes in the server and ACL, a wildcard origin in `[content]`, an enrollment per site, and TLS for a wildcard name (TD-TLS-ACME). For 2, a mount flag, a CSP without the sandbox's script refusal for that mount, and docs that say what it gives away. For 3, a bridge protocol and a key-holding context, plus a design for partitioning. For 4, re-running the spike in each browser. Each also needs Playwright tests of what content can and can't reach.

## TD-SW-SESSION-HANDOVER

* **Status:** open, a possible later feature.
* **Context:** the content origin's worker signs in as a device of the member, registered in the signed ACL (spec/sw.md §8). Adding a device to the ACL is an admin-signed change. A member who isn't an admin enrolls the worker's device, and an admin must then add it. Until then the worker can't sign in, and a fresh content origin serves nothing.
* **How a handed-over session would help:** the member's own signed-in app could give the worker a session at enrollment, so viewing works at once, with no admin step. With the ACL device kept as the long-term credential, the worker would use the device once an admin has added it, and the handed-over session meanwhile.
* **How it would work:**
  * **Not the app's own token.** The server checks the origin only at sign-in (auth.md §3, §5), so the app could pass its own session token. But a session's credential id is "the device" for fencing tokens, idempotency and the fs op chain (auth.md §3), and the worker and the app would then be one device. Two clients must not share that.
  * **A delegated session instead.** A new endpoint, `POST /v1/auth/session/delegate {origin, device_public, signature}`:
    * A signed-in session of the member asks for a new session for the worker. The worker's device key signs a fresh challenge and the content origin (formats.md §10), as at device sign-in.
    * The server checks the signature and that the origin is a configured content origin.
    * It then issues a session with a new method, `delegated`, whose credential id is the worker's device fingerprint. It expires no later than the delegating session.
    * Every request re-checks it as for other methods: the member is in the ACL, and the delegating credential still exists.
  * **The worker** keeps the token in its vault (`saveSession`, which exists). `vault.signIn` already resumes a stored session first, and falls back to the device once an admin has added it.
* **Its limits:**
  * The session expires with `session_ttl_secs`. Then the worker can't sign in again until the member repeats the hand-over from the app, or an admin adds the device.
  * Offline reads of kept content work throughout.
  * Revoking means logging the session out, as any session, plus removing the keyslots.
* **What it would take:** the endpoint and the `delegated` method (spec in auth.md and api.md, limits, `[auth]` switch), a refresh flow from the app (for example on each visit to the app), the worker preferring the device when it can sign in with it, and tests: a member who isn't an admin views at once, and after expiry falls back to kept content and then to the device.
