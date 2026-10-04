# Formats (format_version 1, suite 1 `modern`)

Conventions: see README. Byte sizes are exact. Vectors for every section are in `test-vectors/`.

## 1. Identifiers

* **fs_id**: `u32`. `0` is reserved for plaintext `/unencrypted`, and creating keys for fs 0 is an error. In FoundationDB the fs_id is the first tuple element of every key, so small ids cost ~2 bytes.
* **commit_id**: 16 random bytes per commit (API.md).
* **device id / user id**: the 32-byte fingerprint of the encoded public keys (§7).

## 2. Key hierarchy of an fs (GAPS G2)

```
NK    (32 B, random, long-lived)          MK_e  (32 B, random per epoch)
 │                                          │
 ├─ KDF("zen/v1/kv-name",    NK, u32(fs))   ├─ KDF("zen/v1/kv-data",     MK_e, u32(fs)‖u32(e))  → KV AEAD key
 └─ KDF("zen/v1/topic-name", NK, u32(fs))   ├─ KDF("zen/v1/epoch-chain", MK_e, u32(fs)‖u32(e))  → chain key
                                            └─ KDF("zen/v1/topic-data",  MK_e, u32(fs)‖u32(e))  → topic data root
```

* **Naming keys are never rotated by a revocation.** So stored keys, topic ids, ACL prefixes, cursors and subscriptions survive epoch changes. `key_epoch` appears in sealed objects only, **never** inside a token.
* **Revocation** (`rotate`): pick a fresh `MK_{e+1}`, keep NK, and write an **epoch-chain record**: a sealed object (§4) of kind 3 with `key_epoch = e+1`, plaintext `MK_e` (32 B), key = the chain key of epoch e+1, and AAD context `u32(fs)`. A holder of epoch e+1 can therefore walk back to every earlier epoch.
* **Key bundle** (keyslot payload, 72 B): `u32(fs_id) ‖ u32(epoch) ‖ NK ‖ MK_e`.

## 3. Tokens

### 3.1 Naming chain

For a root key `N_0` and elements `e_1 … e_n` (arbitrary bytes):

```
t_i = PRF16(N_{i-1}, lp(e_i))
N_i = KDF("zen/v1/name-chain", N_{i-1}, lp(e_i))
token(e_1 … e_n) = t_1 ‖ … ‖ t_n            (16·n bytes)
```

* `token(prefix)` is a byte prefix of `token(prefix ‖ more)`, so the server can range-scan and enforce prefix ACLs.
* Holding `N_i` lets a client derive every descendant token, but nothing above or beside it.
* `lp` makes `("ab","c")` and `("a","bc")` distinct.

### 3.2 KV stored keys

`stored_key = token(e_1 … e_n)` with `N_0 = KDF("zen/v1/kv-name", NK, u32(fs))`. The FoundationDB key is the tuple `(fs_id, stored_key)`. A logical tuple is e.g. `(namespace, table, primary_key)`. The PRF is one-way, so the logical key must also be stored inside the encrypted value if scans need it back.

### 3.3 Topics

A topic path `s_1/…/s_n` has two parallel chains:

```
id   = token(s_1 … s_n)  with N_0 = KDF("zen/v1/topic-name", NK, u32(fs))     (naming, stable)
D_0  = KDF("zen/v1/topic-data", MK_e, u32(fs)‖u32(e))
D_i  = KDF("zen/v1/topic-data-chain", D_{i-1}, lp(s_i))                         (data, per epoch)
```

* Giving someone `(N_i, D_i)` for a prefix delegates exactly that subtree. Topic read filters are therefore cryptographic.
* Topics belong to an fs. ACL topic grants are per fs and topic-id prefix.
* **Event AEAD key** = `KDF("zen/v1/event-aead", D_topic, "")`.
* **Event key token** (DESIGN-4 §1.1) = `PRF16(KDF("zen/v1/event-key", N_topic, ""), lp(key))`. It's scoped per topic, so the same entity can't be linked across topics.

## 4. Sealed objects (KV values, events, epoch-chain records)

```
off  len  field
  0    1  format_version = 1
  1    1  suite          = 1
  2    1  kind           1 = KV value, 2 = event, 3 = epoch-chain record
  3    1  reserved       = 0
  4    4  key_epoch      u32
  8   24  nonce          random per seal
 32    n  ciphertext ‖ tag (16)
```

Total = 48 + plaintext length.

`AAD = label ‖ 0x00 ‖ bytes[0..32] ‖ context`. The header is authenticated, so neither the epoch nor the kind can be changed undetected.

| kind | label | context | key |
|---|---|---|---|
| 1 KV value | `zen/v1/aad/kv` | `u32(fs) ‖ lp(stored_key)` | KV AEAD key of `key_epoch` |
| 2 event | `zen/v1/aad/event` | `u32(fs) ‖ lp(topic_id) ‖ lp(event_key_token or empty)` | event AEAD key of `key_epoch` |
| 3 epoch chain | `zen/v1/aad/epoch-chain` | `u32(fs)` | chain key of `key_epoch` |

* A value moved to another key or fs, or an event moved to another topic or event key, fails authentication.
* Clients read `key_epoch` from the header to pick the right epoch keys before decrypting.
* **Native block-device blocks** are KV values (DESIGN-3 §1). There's no separate block format, and the layout in DESIGN.md §2.4 is superseded.

## 5. Event body (plaintext of a kind-2 object)

```
u8 body_version = 1 ‖ sender_fp[32] ‖ u64(hlc) ‖ lp(causation_id) ‖ payload
```

* `sender_fp` is the sending device's id.
* `hlc` is a hybrid logical clock.
* `causation_id` is empty when there's no causing event. Otherwise the transaction layer fills it with the consumed event's id (API.md).

## 6. Keyslots

```
off  len   field
  0    1   format_version = 1
  1    1   suite          = 1
  2    1   slot_type      1 = passphrase, 2 = recovery key, 3 = X-Wing device
  3    1   reserved       = 0
  4   16   slot_id        random
 20    …   type params
  …   24   nonce
  …   88   XChaCha20-Poly1305(KEK, key bundle (72 B)) incl. tag
```

`AAD = "zen/v1/aad/keyslot" ‖ 0x00 ‖ all bytes before the nonce`. `KEK = KDF("zen/v1/keyslot-kek", secret, slot_id)`.

| slot_type | params | secret | total size |
|---|---|---|---|
| 1 passphrase | `u32 m_cost_kib ‖ u32 t_cost ‖ u32 p_cost ‖ salt[32]` | `Argon2id(passphrase, salt, m, t, p, out=32)` | 176 B |
| 2 recovery | (none) | the 32-byte recovery key | 132 B |
| 3 device | `recipient_fp[32] ‖ xwing_ct[1120]` | X-Wing shared secret | 1284 B |

* **Argon2id parameters** are authenticated in the AAD and feed the KEK, so altering them breaks the slot.
* **Creation** requires m ≥ 65536 KiB (64 MiB), t ≥ 1 and 1 ≤ p ≤ 4. Recommended: native 1 GiB/t=4, browser 256 MiB/t=3, p=1. **Opening** enforces a ceiling of m ≤ 4194304 KiB (4 GiB), 1 ≤ t ≤ 16 and 1 ≤ p ≤ 4, because the stored parameters come from the untrusted server and would otherwise let it make unlocking hang or run out of memory. Within the ceiling, any stored parameters are accepted (lower ones only weaken the user's own slot, and they're authenticated).
* **Device slots** wrap only to a device key verified by certificate and out-of-band fingerprint (G1). `recipient_fp` lets a device find its own slot.
* The **recovery key** is 32 random bytes. Its human-readable encoding (word list or grouped base32) is defined by the client UI spec in a later milestone.

## 7. Identities and signatures

### 7.1 Identity derivation

From a 32-byte seed:

```
ed25519 secret = KDF("zen/v1/sig-ed25519",   seed, "")
ml-dsa-65 ξ    = KDF("zen/v1/sig-ml-dsa-65", seed, "")    → ML-DSA.KeyGen_internal(ξ)
```

A **device secret** (32 B) derives two keys:

```
identity seed = KDF("zen/v1/device-sig", secret, "")
X-Wing seed   = KDF("zen/v1/device-kem", secret, "")      (the X-Wing decapsulation key)
```

### 7.2 Encodings

| Object | Layout | Size |
|---|---|---|
| public identity | `format ‖ suite ‖ ed25519_pk[32] ‖ ml-dsa-65_vk[1952]` | 1986 B |
| device public | `public identity ‖ xwing_ek[1216]` | 3202 B |
| hybrid signature | `format ‖ suite ‖ ed25519_sig[64] ‖ ml-dsa-65_sig[3309]` | 3375 B |

Fingerprints: `user_fp = FP(public identity)`, `device_fp = FP(device public)`.

### 7.3 Signing

```
SM  = "zen/v1/sig" ‖ 0x00 ‖ lp(purpose) ‖ msg
sig = Ed25519.Sign(SM) ‖ ML-DSA-65.Sign_deterministic(SM, ctx = "")
```

* `purpose` must be one of the signature purposes in labels.md.
* Verification uses Ed25519 strict verification and ML-DSA verification with an empty context. **It succeeds only if both halves verify.**

### 7.4 Device certificate (G1)

```
body = format ‖ suite ‖ user_fp[32] ‖ device_public[3202] ‖ u64(created_unix)     (3244 B)
cert = body ‖ hybrid_signature("zen/v1/sig/device-cert", body)                   (6619 B)
```

The verifier checks the signature with the expected user identity, **and** that `user_fp` in the body is that identity's fingerprint.

The membership log that distributes certificates (G1) is specified in a later milestone.

## 8. Not yet specified

These later-milestone formats are out of scope here:
* commit records and signed roots
* event checkpoints
* the ACL document
* the membership log
* the authenticated (Merkle) integrity tier
* CRDT op encodings
* zen-db catalog and row encodings

They'll reuse §4 and §7 and the labels already registered.
