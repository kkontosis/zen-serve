# Algorithm suites

Every stored object records a one-byte suite id right after `format_version`. A deployment can hold objects of several suites. A reader rejects suites it doesn't implement.

## Suite 1: `modern` (implemented)

| Purpose | Algorithm | Notes |
|---|---|---|
| AEAD | XChaCha20-Poly1305 | 32-byte key, 24-byte random nonce, 16-byte tag |
| KDF | BLAKE3 `derive_key` | `KDF(label, key, info) = BLAKE3.derive_key(label, key ‖ info)`, `key` always 32 bytes |
| PRF | BLAKE3 keyed hash | `PRF16(key, data) = BLAKE3.keyed_hash(key, data)[0..16]`; `PRF32(key, data) = BLAKE3.keyed_hash(key, data)` |
| Fingerprint | BLAKE3 `derive_key` | `FP(x) = BLAKE3.derive_key("zen/v1/fingerprint", x)` |
| Password KDF | Argon2id v1.3 | 32-byte output; params stored per keyslot; creation floor m ≥ 64 MiB (G25) |
| KEM | X-Wing (ML-KEM-768 + X25519) | IETF draft-connolly-cfrg-xwing-kem **-06**; 32-byte decapsulation seed |
| Signature | Ed25519 + ML-DSA-65 (FIPS 204) | hybrid; both deterministic; **both must verify** |

**Why BLAKE3 `derive_key` instead of HKDF:** one primitive serves as KDF, PRF and hash. Its context string is a natural slot for the domain-separation labels. It's fast in WASM. Fixed-length 32-byte input keys make `key ‖ info` unambiguous. HKDF-SHA-384 belongs to the `fips` suite.

**Why deterministic signatures:** Ed25519 is deterministic by design. ML-DSA uses the FIPS 204 deterministic variant with an empty context string; the purpose label is inside the signed message instead (formats.md §7). Determinism makes test vectors reproducible and removes the RNG from signing.

## Suite 2: `fips` (reserved, not implemented)

| Purpose | Algorithm |
|---|---|
| AEAD | XAES-256-GCM (C2SP), same 24-byte nonce and object layouts |
| KDF / PRF / hash | HKDF-SHA-384 / HMAC-SHA-384 (truncated) / SHA-384 |
| Password KDF | PBKDF2-HMAC-SHA-512 |
| KEM | ML-KEM-1024 + ECDH P-384 hybrid |
| Signature | ML-DSA-87 + ECDSA P-384 |

This suite is realistic only for native clients using a FIPS-validated module (e.g. `aws-lc-rs` in FIPS mode). Readers must currently reject suite id 2.

## Post-quantum status

Which surfaces resist a large quantum computer, and with what. A **hybrid** pairs a post-quantum algorithm with a classical one, so an attacker must break both; "harvest now, decrypt later" means traffic recorded today and decrypted once such a computer exists.

| Surface | Protected by | Post-quantum |
|---|---|---|
| Keys at rest: keyslots, device and inbox keys | X-Wing (ML-KEM-768 + X25519, suite 1) | yes, hybrid |
| Identities and signatures: devices, ACL, ops, method 6's password key | Ed25519 + ML-DSA-65 (suite 1) | yes, hybrid; both must verify |
| Sign-in: methods 1 (device keys) and 6 (password-derived keys) | the identity's hybrid signature (auth.md §11) | yes |
| TLS key exchange, native TLS (operations.md §8) | `X25519MLKEM768` | yes, hybrid, for clients that offer it (current browsers, Node, zen clients); others fall back to `X25519` |
| SSH key exchange (svc.md §7.2) | `mlkem768x25519-sha256`, `sntrup761x25519-sha512` | yes, hybrid, for OpenSSH 9.0 and later; older clients get `curve25519-sha256` |
| SSH host keys and user keys (svc.md §7.2) | `ssh-mldsa44-ed25519` | yes, hybrid, where the service holds one and the client asks for it (OpenSSH 10.6 and later), and for user keys of that type; `ssh-ed25519` and `ssh-rsa` stay classical |

**What stays classical, and why:**
* **Passkeys** (method 2): WebAuthn authenticators sign with ES256, Ed25519 or RSA; there is no post-quantum authenticator algorithm to ask for (auth.md §7.6).
* **OPAQUE** (method 3): ristretto255 is a classical group. A recorded sign-in also needs its TLS session broken first, which native TLS's hybrid prevents; behind a proxy that terminates TLS classically, that protection is gone (auth.md §8.7).
* **TLS client certificates** (method 5): the certificate's key and the handshake signature are classical (auth.md §10.5).
* **TLS server certificates**: the WebPKI issues classical certificates. A forged certificate needs a quantum computer at the time of the attack (an active attacker), not a recording, so this is no harvest-now risk.
* **SSH clients without the hybrids**: OpenSSH before 9.0, and other clients that offer only `curve25519-sha256`; and SSH sessions authenticated by classical host or user keys, where an active attacker with a quantum computer could forge the signatures (the key exchange still protects recorded traffic).
