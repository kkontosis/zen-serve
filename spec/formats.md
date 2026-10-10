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
                                            ├─ KDF("zen/v1/topic-data",  MK_e, u32(fs)‖u32(e))  → topic data root
                                            └─ KDF("zen/v1/fs-data",     MK_e, u32(fs)‖u32(e))  → filesystem AEAD key (§11)
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

## 4. Sealed objects (KV values, events, epoch-chain records, filesystem objects)

```
off  len  field
  0    1  format_version = 1
  1    1  suite          = 1
  2    1  kind           1 = KV value, 2 = event, 3 = epoch-chain record,
                         4 = fs node meta, 5 = fs manifest, 6 = fs chunk,
                         7 = CRDT value (zendb.md §19)
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
| 4 fs node meta | `zen/v1/aad/fs-meta` | `u32(fs) ‖ tree(16) ‖ node(16)` | filesystem AEAD key of `key_epoch` |
| 5 fs manifest | `zen/v1/aad/fs-manifest` | `u32(fs) ‖ tree(16) ‖ node(16)` | filesystem AEAD key of `key_epoch` |
| 6 fs chunk | `zen/v1/aad/fs-chunk` | `u32(fs) ‖ chunk_id(16)` | filesystem AEAD key of `key_epoch` |
| 7 CRDT value | `zen/v1/aad/crdt-value` | `u32(fs) ‖ lp(object) ‖ field(16) ‖ elem(16)` (zendb.md §19.2) | KV AEAD key of `key_epoch` |

* A value moved to another key or fs, or an event moved to another topic or event key, fails authentication.
* Clients read `key_epoch` from the header to pick the right epoch keys before decrypting.
* **Native block-device blocks** are KV values (DESIGN-3 §1). There's no separate block format, and the layout in DESIGN.md §2.4 is superseded.

## 5. Event body (plaintext of a kind-2 object)

```
u8 body_version = 1 ‖ sender_fp[32] ‖ u64(hlc) ‖ lp(causation_id) ‖ payload
```

* `sender_fp` is the sending device's id.
* `hlc` is a hybrid logical clock.
* `causation_id` is empty when there's no causing event. Otherwise the transaction layer fills it with the consumed event's id, `topic_id ‖ offset` (zendb.md §10.2).

## 6. Keyslots

```
off  len   field
  0    1   format_version = 1
  1    1   suite          = 1
  2    1   slot_type      1 = passphrase, 2 = recovery key, 3 = X-Wing device, 4 = WebAuthn PRF,
                          5 = OPAQUE export key
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
| 4 WebAuthn PRF | `credential_id[32] ‖ prf_salt[32]` | the passkey's 32-byte PRF output for `prf_salt` | 196 B |
| 5 OPAQUE export key | `credential_id[32]` | `BLAKE3.derive_key("zen/v1/opaque-keyslot", export_key[64])` | 164 B |

* **Argon2id parameters** are authenticated in the AAD and feed the KEK, so altering them breaks the slot.
* **Creation** requires m ≥ 65536 KiB (64 MiB), t ≥ 1 and 1 ≤ p ≤ 4. Recommended: native 1 GiB/t=4, browser 256 MiB/t=3, p=1. **Opening** enforces a ceiling of m ≤ 4194304 KiB (4 GiB), 1 ≤ t ≤ 16 and 1 ≤ p ≤ 4, because the stored parameters come from the untrusted server and would otherwise let it make unlocking hang or run out of memory. Within the ceiling, any stored parameters are accepted (lower ones only weaken the user's own slot, and they're authenticated).
* **Device slots** wrap only to a device key verified by certificate and out-of-band fingerprint (G1). `recipient_fp` lets a device find its own slot.
* **WebAuthn PRF slots** open with a passkey (auth.md §7.7). `credential_id` is the passkey's id in the server's credential store, `BLAKE3.derive_key("zen/v1/passkey-id", WebAuthn credential id)` (auth.md §4.1), which is also the `device_fp` of its sessions; it lets a client find the slot of a passkey. `prf_salt` is 32 random bytes chosen for the slot, passed to the authenticator as the PRF input (`extensions.prf.eval.first`, or per credential in `evalByCredential`); the secret is the 32-byte result (`prf.results.first`). WebAuthn already domain-separates the PRF input (the browser hashes it with the context `"WebAuthn PRF"`), and the result is specific to the credential and the salt, so the slot needs no label of its own. The server never sees the PRF result: it is a client extension output, not part of the signed authenticator data. A passkey whose authenticator doesn't support PRF can't have a slot; its user keeps another one.
* **OPAQUE export-key slots** open with the password of an OPAQUE credential (sign-in method 3, auth.md §8.6). `credential_id` is the credential's id in the server's credential store, which is also the `device_fp` of its sessions; it lets a client find the slot of the credential it signed in with. `export_key` is the 64-byte export key that OPAQUE gives the client at registration and at every sign-in with that credential (RFC 9807 §6, `export_key`); the server never sees it. A new registration, which a password change is, gives a new export key even for the same password, so the client re-wraps the slot then. The slot's secret is a derivation of its own, so the export key can serve other uses.
* The **recovery key** is 32 random bytes. Its human-readable encoding (word list or grouped base32) is defined by the client UI spec in a later milestone.

Keyslots are stored in the fs header (§12).

Vectors: `test-vectors/keyslots.json` (types 1–3), `test-vectors/prf_keyslot.json` (type 4), `test-vectors/opaque_keyslot.json` (type 5).

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

The membership log that distributes certificates (G1) is the ACL chain (§9).

### 7.5 Password-derived key (sign-in method 6, auth.md §11)

A client derives a hybrid signing key from a password. Only the client ever sees the password.

```
root     = Argon2id(password, salt[32], m_cost_kib, t_cost, p_cost, out = 32)
seed     = KDF("zen/v1/password-sig", root, "")
identity = the identity of `seed` (§7.1)
```

* `password` is the UTF-8 bytes the user typed, unchanged. The login name is not an input: it only finds the account (auth.md §11.1).
* `salt` is 32 random bytes chosen by the client at registration. The server stores the salt, the parameters and the **public identity** (§7.2), never the password or `root`.
* **Argon2id parameters** follow the passphrase keyslot (§6): **registration** requires m ≥ 65536 KiB, t ≥ 1 and 1 ≤ p ≤ 4, and **sign-in** enforces only the ceiling m ≤ 4194304 KiB, 1 ≤ t ≤ 16, 1 ≤ p ≤ 4, because the server supplies the parameters.
* To sign in, the client signs the session message of §10 with purpose `zen/v1/sig/password-session`. A device signature (`zen/v1/sig/session`) never verifies as a password signature, or the reverse.
* The public identity is a **verifier**: whoever holds it, the server included, can test password guesses offline at the cost of one Argon2id run each, exactly as with a passphrase keyslot.

Vectors: `test-vectors/pwkey.json`.

## 8. Not yet specified

These later-milestone formats are out of scope here:
* commit records and signed roots
* event checkpoints and filesystem tree checkpoints
* the authenticated (Merkle) integrity tier
* CRDT objects other than the filesystem (§11)

They'll reuse §4 and §7 and the labels already registered. zen-db's catalog, row, index and message encodings are in [zendb.md](zendb.md).

## 9. Signed ACL and membership log (G1)

The server ACL is a plaintext document containing only opaque ids. The server parses it and enforces it. Every version is kept, and each one names the hash of the previous one, so **the ACL chain is the membership log**: it distributes public identities and device certificates.

### 9.1 Document

Deterministic CBOR (RFC 8949 §4.2.1: definite lengths, shortest integers, map keys sorted bytewise by their encoding). Field names in that sorted order:

```
AclDoc = {
  admins:    [bytes(32)],          // user_fp of each admin; each must be a member
  grants:    [Grant],
  limits:    [{fs: u32, max_keys: u64?, max_bytes: u64?}],
  members:   [{devices: [bytes], identity: bytes}],   // device certs (§7.4), public identity (§7.2)
  origins?:  [text],               // origins the admins vouch for (auth.md §5.3); omitted when empty
  version:   u64,                  // 1, 2, 3, …
  prev_hash: bytes(32),            // H(previous doc bytes), 32 zero bytes for version 1
}
Grant = {fs: u32, topic: bytes?, rights: [text], subject: bytes(32)}
```

* **Grants.** A grant without `topic` is an **fs grant**, with rights in {`read`, `write`}. A grant with `topic` is a **topic grant** covering every topic id with that byte prefix, with rights in {`read`, `append`, `consume`}. An empty `topic` covers all topics of the fs.
* **Admins.** Being an admin carries the `admin` right: it can change the ACL and fs headers. It grants no data access by itself.
* **Origins.** `origins` lists server origins (`scheme://host[:port]`, auth.md §5) that the admins sign for. A server uses them only when its `acl_origins` setting is on (auth.md §5.3). The field is **omitted when empty**, so a document without origins has exactly the bytes, and the hash, it had before the field existed. Readers that don't know the field ignore it.

### 9.2 Signing and hashing

```
SignedAcl = {doc: bytes, sig: bytes, signer: bytes(32)}        (CBOR)
sig       = hybrid signature, purpose "zen/v1/sig/acl", msg = doc
H(doc)    = BLAKE3.derive_key("zen/v1/acl-chain", doc)
```

Verifiers hash and verify the **received bytes of `doc`**. They never re-encode it.

### 9.3 Acceptance rules (server, and clients walking the chain)

1. `version` is the head version + 1 (1 if there is no head), and `prev_hash` is H(head doc), or zeros for version 1.
2. `signer` is an admin of the **head** doc. For version 1 it must be an admin of the new doc, and the claim token is required (api.md §4.1). The signer's identity is taken from that doc's `members`, and `sig` must verify.
3. The new doc is well formed:
   * at least one admin, and every admin is a member
   * member identities are unique, and every device certificate verifies against its member's identity
   * every grant subject is a member, and every right is valid for its grant kind
   * grant topics are 16·n bytes with n ≤ 16
   * every grant and limit `fs` is a configured, non-zero fs_id
   * `origins` holds at most 16 distinct entries, each a valid origin (auth.md §5): `http` or `https`, a lowercase host, an optional port, and no path or trailing slash

Clients pin the head they've verified, and refuse a chain that doesn't extend it.

## 10. Session signature

To sign in (api.md §3), a device signs with its device identity (§7.1):

```
purpose = "zen/v1/sig/session"
msg     = lp(challenge) ‖ lp(origin)
```

`origin` is the UTF-8 `scheme://host[:port]` of the server, as the client sees it.

A password-derived key (§7.5) signs the same `msg` with purpose `zen/v1/sig/password-session`.

## 11. Filesystem objects (spec/fs.md)

Every integer is big-endian. `tree`, `node` and `chunk_id` are 16 bytes. `ROOT = 00…00` and `TRASH = FF…FF` (16 bytes each).

### 11.1 Hybrid logical clock

```
hlc = u64( unix_ms << 16 | counter )      counter: u16
```

* A client generates `last = max(wall_ms << 16, last + 1)` per operation, and observes server timestamps with `last = max(last, seen)` (fs.md §2).
* The server compares `hlc >> 16` with its clock for the skew and horizon checks.
* `hlc` must be at most `2^63 − 1`.

### 11.2 Node meta (plaintext of a kind-4 object)

```
u8 meta_version = 1 ‖ u8 type ‖ lp(name) ‖ u32 mode ‖ u64 mtime_ms ‖ lp(xattrs)
```

* `type`: 1 = directory, 2 = file, 3 = symlink. A symlink's target is its content.
* `name`: UTF-8 without `/` or NUL, 1–255 bytes.
* `mode`: POSIX permission bits.
* `mtime_ms`: unix milliseconds.
* `xattrs`: CBOR map (may be empty, length 0).

### 11.3 Manifest (plaintext of a kind-5 object)

```
u8 manifest_version = 1 ‖ u64 size ‖ u32 chunk_size ‖ u32 n ‖ n × chunk_id(16)
```

* `size` is the exact file size in bytes. Every chunk holds `chunk_size` plaintext bytes except the last.
* The chunk list must equal the `chunks` of the `write` that carries the manifest (fs.md §4).

### 11.4 Chunk (kind 6)

The plaintext is the chunk's bytes. A chunk is bound to its fs and id, so the server can't substitute one chunk for another.

### 11.5 Operation encoding and op chain

Each operation has a canonical byte form, used for the per-tree chain:

```
move   = 0x01 ‖ u32(fs) ‖ tree ‖ node ‖ parent ‖ u64(hlc) ‖ lp(meta or empty)
meta   = 0x02 ‖ u32(fs) ‖ tree ‖ node ‖ u64(hlc) ‖ lp(meta)
write  = 0x03 ‖ u32(fs) ‖ tree ‖ node ‖ u32(r) ‖ r × dot(12)
              ‖ u32(n) ‖ n × chunk_id ‖ lp(manifest)
```

* `meta` and `manifest` are the sealed objects, byte for byte.
* In `write`, the `r` dots are `replaces`. The new version's own dot is not part of the chain: it is only known once the commit commits.

The chain starts at 32 zero bytes. Each operation accepted by the server extends it, in arrival order:

```
chain_n = BLAKE3.derive_key("zen/v1/tree-op-chain", chain_{n−1} ‖ lp(op_bytes) ‖ device_fp(32))
```


## 12. fs header

The fs header holds an fs's keyslots (§6) and its epoch chain (§2, §4 kind 3). The server stores it as opaque bytes, with a version for compare-and-set (api.md §4.4); only an admin may write it.

```
u8  header_version = 1
u8  suite          = 1
u32 fs
u32 current_epoch                  the epoch new data is sealed under
u16 n_slots       ‖ n_slots × lp(keyslot)            (§6), at most 256
u16 n_chain       ‖ n_chain × lp(epoch-chain record) (kind 3, §4)
```

* **The chain.** `chain[i]` is the record written when the fs rotated to epoch `i + 1`: the keys of epoch `i + 1` open it to the keys of epoch `i`. So `n_chain = current_epoch`, and the sealed header of `chain[i]` names `key_epoch = i + 1`. A client holding epoch `e` reads any epoch `≤ e` by walking the chain back.
* **Slots.** Each slot starts with the §6 prefix (`format_version`, `suite`, `slot_type`, `reserved = 0`, `slot_id`). Clients keep slots of types they don't know, unchanged, and never try to open them. A client finds its slot by type and by what identifies it: `recipient_fp` (device), `credential_id` (passkey PRF, OPAQUE export key), or by trying each passphrase or recovery slot.
* **Rotation (revocation).** The admin opens a slot, rotates (`chain` gains a record, `current_epoch` += 1), re-wraps the bundle of the new epoch for every slot that keeps access, removes the others, and writes the header with `expect` set to the version read. A slot wrapping an older epoch than `current_epoch` is **stale**: it still reads old data, but a client that opens it must not write, and an admin should re-wrap or remove it.
* **Checks on decode.** Exact length (no trailing bytes), known header version and suite, `n_chain = current_epoch`, every chain record of kind 3 with the right `key_epoch`, every slot with a valid prefix. A client also checks that a bundle it unwraps names the header's `fs`.
* **Writing a slot for oneself.** Header writes need admin. A member who wants a slot for a new passkey or password prepares the slot (the bundle never leaves the client unwrapped) and hands it to an admin, who adds it (`TD-FS-HEADER-SELF-SLOT`).

**Leakage and rollback.** The server sees the number of slots, their types and ids, the credential ids and device fingerprints in their parameters, the Argon2id parameters, and the number of epochs. It can't open a slot or forge one that opens to real keys. It can:
* drop slots or the whole header (denial of service; detectable only out of band),
* serve an older header: a client then sees an older `current_epoch`, and notices when it meets data sealed under a newer `key_epoch` than its keys can reach. It refuses to write under an epoch older than one it has seen (clients remember the highest epoch per fs),
* nothing else: a slot or chain record that was tampered with fails to open.

A signed header (by an admin, chained like the ACL) is planned with the authenticated tier (milestone 6).

Vectors: `test-vectors/header.json`. The ACL encoding (§9) has vectors too: `test-vectors/acl.json`.
