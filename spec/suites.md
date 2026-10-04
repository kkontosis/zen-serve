# Algorithm suites

Every stored object records a one-byte suite id right after `format_version`. A deployment can hold objects of several suites. A reader rejects suites it doesn't implement.

## Suite 1: `modern` (implemented)

| Purpose | Algorithm | Notes |
|---|---|---|
| AEAD | XChaCha20-Poly1305 | 32-byte key, 24-byte random nonce, 16-byte tag |
| KDF | BLAKE3 `derive_key` | `KDF(label, key, info) = BLAKE3.derive_key(label, key ‖ info)`, `key` always 32 bytes |
| PRF | BLAKE3 keyed hash | `PRF16(key, data) = BLAKE3.keyed_hash(key, data)[0..16]` |
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
