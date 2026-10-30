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
* **File keys** (§11.3, §11.4): a file's chunks under a version-2 manifest are sealed under `KDF("zen/v1/fs-file", file_key, u32(fs))`, where `file_key` is 32 random bytes kept in the file's sealed manifest. It is outside the epoch tree, so one file can be handed over without the fs keys (milestone 11).

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
                         7 = CRDT value (zendb.md §19),
                         8 = local record (zendb.md §9.3),
                         9 = svc state, 10 = svc output key (§13),
                         11 = device checkpoint (§14)
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
| 6 fs chunk | `zen/v1/aad/fs-chunk` | `u32(fs) ‖ chunk_id(16)` | filesystem AEAD key of `key_epoch` (a version-1 manifest), or the file's chunk key (a version-2 manifest, §11.4) |
| 7 CRDT value | `zen/v1/aad/crdt-value` | `u32(fs) ‖ lp(object) ‖ field(16) ‖ elem(16)` (zendb.md §19.2) | KV AEAD key of `key_epoch` |
| 8 local record | `zen/v1/aad/local` | `u32(fs) ‖ lp(store) ‖ lp(record_key)` (zendb.md §9.3) | KV AEAD key of `key_epoch` |
| 9 svc state | `zen/v1/aad/svc-state` | `u32(fs) ‖ lp(svc) ‖ conn(16) ‖ u64(in_acked) ‖ u64(out_head)`, the cursors after the step that wrote it (svc.md §4) | service key `K_svc` of `key_epoch` (§13) |
| 10 svc output key | `zen/v1/aad/svc-key` | `u32(fs) ‖ lp(svc) ‖ conn(16) ‖ u64(off)`, the record's output offset | service key `K_svc` of `key_epoch` (§13) |
| 11 device checkpoint | `zen/v1/aad/checkpoint` | `u32(fs) ‖ device(32) ‖ u64(seq)` (§14) | KV AEAD key of `key_epoch` |

* A value moved to another key or fs, or an event moved to another topic or event key, fails authentication. So does a local record moved to another store, record key or fs.
* A state (kind 9) paired with other cursors, or an output key (kind 10) moved to another record, connection or service, fails authentication.
* A checkpoint (kind 11) moved to another device or sequence number fails authentication.
* Local records (kind 8) never reach the server: they are the fs layer of a device's local store (zendb.md §9.3).
* Clients read `key_epoch` from the header to pick the right epoch keys before decrypting.
* **Native block-device blocks** are KV values (DESIGN-3 §1). There's no separate block format, and the layout in DESIGN.md §2.4 is superseded.

## 5. Event body (plaintext of a kind-2 object)

```
version 1:  u8 body_version = 1 ‖ sender_fp[32] ‖ u64(hlc) ‖ lp(causation_id) ‖ payload
version 2:  u8 body_version = 2 ‖ sender_fp[32] ‖ u64(hlc) ‖ lp(causation_id)
                                 ‖ chain_id[12] ‖ u64(dseq) ‖ dprev[32] ‖ payload
```

* `sender_fp` is the sending device's id.
* `hlc` is a hybrid logical clock.
* `causation_id` is empty when there's no causing event. Otherwise the transaction layer fills it with the consumed event's id, `topic_id ‖ offset` (zendb.md §10.2).
* In an ephemeral message (api.md §9), which no event causes, `causation_id` holds the publishing stream's id instead: 12 random bytes chosen once per stream (a connection, kept across reconnects). Receivers use it to tell apart two sessions of one device (api.md §9.2). An empty one is valid (older clients).

### 5.1 Event chains (version 2)

A version-2 body is a link of a **chain**: the events one writer appends to one topic, `(sender_fp, chain_id, topic)`.

```
dprev = HE(body of the chain's previous event), 32 zero bytes when dseq = 1
HE(b) = BLAKE3.derive_key("zen/v1/event-chain", b)          b: the whole plaintext body
```

* **`chain_id`** is 12 random bytes a writer picks when it starts (a process, a tab). A restart starts a new chain at `dseq = 1`, so a writer stores no counter.
* **`dseq`** counts the chain's events from 1, in offset order. Several appends to one topic in one commit take consecutive values in `append` order.
* **Advancing.** A writer advances its chain only once the commit that carries an event is known to have applied. A refused or conflicting commit leaves it where it was, so the rebuilt commit reuses the same `dseq`. A commit whose outcome stays unknown (`commit_unknown`, never resolved) ends the chain: the writer starts a new one. A chain has at most one commit in flight.
* **Readers of a whole topic** (log reads and subscriptions without a key filter) keep each chain's last `(dseq, HE(body))`. The next event of a chain must have `dseq + 1` and that `dprev`. A skipped `dseq` is a **gap** (an event withheld), and a different body at a `dseq` already seen is a **rewrite**.
* **Filtered readers** (`key_token` reads, `partitioned`, `per_key` and `single_key` groups) see a subset of each chain, so they check only for rewrites.
* The body is sealed (kind 2), so the server can't read `dseq` or compute `dprev`; it can only withhold events, which gaps and checkpoints (§14) reveal.
* Ephemeral messages stay version 1, and so may events of writers that don't chain. Version 1 still decodes.

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
* the authenticated (Merkle) integrity tier of zen-db (zendb.md §14) and block volumes (milestone 6)
* CRDT objects other than the filesystem (§11)

They'll reuse §4, §7 and §14 and the labels already registered. Device checkpoints, which carry event, tree and connection heads, are §14. zen-db's catalog, row, index and message encodings are in [zendb.md](zendb.md).

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
* `xattrs`: CBOR map (may be empty, length 0), from text names to byte strings.
  * Names starting with `zen.` are reserved for zen-serve; others are the app's (zen-mount shows them in the `user.` namespace).
  * `zen.content_type`: the node's media type (`text/html`, …), UTF-8 text, as served over HTTP (milestone 9).

### 11.3 Manifest (plaintext of a kind-5 object)

Version 1, read but no longer written by clients from milestone 5 (step 7g) on:

```
u8 manifest_version = 1 ‖ u64 size ‖ u32 chunk_size ‖ u32 n ‖ n × chunk_id(16)
```

* `size` is the exact file size in bytes. Every chunk holds `chunk_size` plaintext bytes except the last.
* The chunk list must equal the `chunks` of the `write` that carries the manifest (fs.md §4).
* Its chunks are sealed under the filesystem AEAD key of their `key_epoch`.

Version 2:

```
u8 manifest_version = 2 ‖ u64 size ‖ u32 chunk_size ‖ u64 n ‖ u8 key_mode ‖ keys ‖ u8 form ‖ body
key_mode 0:      keys = (empty)
key_mode 1:      keys = u32 key_epoch ‖ file_key(32)
form 1, inline:  body = n × chunk_id(16)
form 2, paged:   body = u32 page_size ‖ u32 m ‖ m × (page_id(16) ‖ page_digest(32))
```

* `n` is the number of chunks; `size` and `chunk_size` are as in version 1.
* `key_mode` says how the chunks are sealed (§11.4):
  * **0, fs keys:** under the filesystem AEAD key of each chunk's own header `key_epoch`, as in version 1.
  * **1, a file key:** under the file's chunk key from `file_key`; `key_epoch` is the epoch the file key was made in.
  * A decoder refuses any other value. Which mode a writer picks is the fs policy (§12, fs.md §4.2); a reader needs no setting.
* **Inline:** the chunk list must equal the `chunks` of the `write` that carries the manifest.
* **Paged:** the chunk list is split into pages (§11.6) of `page_size ≥ 1` ids each, every page full but the last, so `m = ceil(n / page_size)` (and `m = 0` when `n = 0`). Chunk `i` is in page `i / page_size` at slot `i % page_size`. The page ids must equal the `pages` of the `write`, in order. A reader checks each page against its `page_digest` and its expected count when it fetches it, so a page that was swapped, reordered or truncated is detected.
* A decoder refuses any other `form`, a paged `m` other than `ceil(n / page_size)`, `page_size = 0` with `n > 0`, and trailing bytes.
* At the defaults (`max_value_bytes` 90,000) a paged version holds about 1,870 pages of 5,624 chunks, about 640 GiB at 64 KiB per chunk.

### 11.4 Chunk (kind 6)

The plaintext is the chunk's bytes. A chunk is bound to its fs and id, so the server can't substitute one chunk for another.

* **Under a version-1 manifest, or a version-2 manifest with `key_mode 0`,** it is sealed under the filesystem AEAD key of its header's `key_epoch`. The chunks of one file may carry different epochs (a chunk reused from before a rotation keeps its own).
* **Under a version-2 manifest with `key_mode 1`** it is sealed under the file's chunk key `KDF("zen/v1/fs-file", file_key, u32(fs))` (§2), and its header's `key_epoch` is the manifest's `key_epoch`. A reader opens it with the key the manifest gives, and checks the header epoch against the manifest's.
* **One file key per node**, kept across the node's versions so a new version reuses unchanged chunks. After a rotation, the fs policy decides when a writer replaces it (fs.md §4.2). A copy to another node may keep the key and reuse the chunks.
* Chunks are reused across versions only under the same key: the same file key, or both under fs keys.

### 11.5 Operation encoding and op chain

Each operation has a canonical byte form, used for the per-tree chain:

```
move   = 0x01 ‖ u32(fs) ‖ tree ‖ node ‖ parent ‖ u64(hlc) ‖ lp(meta or empty)
meta   = 0x02 ‖ u32(fs) ‖ tree ‖ node ‖ u64(hlc) ‖ lp(meta)
write  = 0x03 ‖ u32(fs) ‖ tree ‖ node ‖ u32(r) ‖ r × dot(12)
              ‖ u32(n) ‖ n × chunk_id ‖ lp(manifest)
write_paged
       = 0x04 ‖ u32(fs) ‖ tree ‖ node ‖ u32(r) ‖ r × dot(12)
              ‖ u32(m) ‖ m × page_id ‖ lp(manifest)
```

* `meta` and `manifest` are the sealed objects, byte for byte.
* In `write` and `write_paged`, the `r` dots are `replaces`. The new version's own dot is not part of the chain: it is only known once the commit commits.
* A `write` with `pages` (fs.md §4) is encoded as `write_paged`; every other `write`, an empty one included, as `write`.

The chain starts at 32 zero bytes. Each operation accepted by the server extends it, in arrival order:

```
chain_n = BLAKE3.derive_key("zen/v1/tree-op-chain", chain_{n−1} ‖ lp(op_bytes) ‖ device_fp(32))
```

**Purge records.** Each trash-purge round that removes nodes (fs.md §6) extends the chain too, in the same transaction as the removal, so `(ops, chain)` names every change of a tree's node set (fs.md §9):

```
purge  = 0x05 ‖ u32(fs) ‖ tree ‖ u32(n) ‖ n × node      (the round's nodes, in the order removed)
```

* Its device is 32 zero bytes (the server), and `ops` counts it like an operation.
* Clients never build one: they compare `(ops, chain)` with what the server and checkpoints say. A client that replays the chain from operations it holds can do so only up to a tree's first purge.


### 11.6 Page

A page holds part of a paged version's chunk list (§11.3). It is stored and read by the server in the clear, like the inline `chunks` of a `write`:

```
page = u32 n ‖ n × chunk_id(16)          1 ≤ n, 4 + 16n ≤ max_value_bytes
page_digest = BLAKE3.derive_key("zen/v1/fs-page", page)
```

* A page id is 16 random bytes chosen by the client. A page is immutable: an id never holds other bytes (fs.md §4.1).
* At the defaults a page holds up to 5,624 chunk ids, about 351 MiB at 64 KiB per chunk.

## 12. fs header

The fs header holds an fs's keyslots (§6) and its epoch chain (§2, §4 kind 3). The server stores it as opaque bytes, with a version for compare-and-set (api.md §4.4); only an admin may write it.

```
u8  header_version = 1 or 2
u8  suite          = 1
u32 fs
u32 current_epoch                  the epoch new data is sealed under
u16 n_slots       ‖ n_slots × lp(keyslot)            (§6), at most 256
u16 n_chain       ‖ n_chain × lp(epoch-chain record) (kind 3, §4)
lp(policy)                         version 2 only
```

* **The policy** is a deterministic CBOR map (zendb.md §1) with unsigned-integer keys:

  | Key | Name | Values | Default |
  |---|---|---|---|
  | 1 | `file_keys` | 0 off, 1 on: new files get a file key of their own (§11.3 `key_mode 1`) | 0 |
  | 2 | `rekey_after_rotate` | 0 `rewrite`, 1 `eager`, 2 `keep` (fs.md §4.2) | 0 |

  * A version-1 header has the default policy.
  * A writer writes version 1 while the policy is the default and holds no unknown keys, so older clients still read it; otherwise version 2.
  * Unknown keys are kept on re-encode and otherwise ignored. A known key with a value the client doesn't know makes the header undecodable: a new value comes with a new header version.
  * The policy is advice to writers. Readers never need it, since each manifest names its `key_mode`.

* **The chain.** `chain[i]` is the record written when the fs rotated to epoch `i + 1`: the keys of epoch `i + 1` open it to the keys of epoch `i`. So `n_chain = current_epoch`, and the sealed header of `chain[i]` names `key_epoch = i + 1`. A client holding epoch `e` reads any epoch `≤ e` by walking the chain back.
* **Slots.** Each slot starts with the §6 prefix (`format_version`, `suite`, `slot_type`, `reserved = 0`, `slot_id`). Clients keep slots of types they don't know, unchanged, and never try to open them. A client finds its slot by type and by what identifies it: `recipient_fp` (device), `credential_id` (passkey PRF, OPAQUE export key), or by trying each passphrase or recovery slot.
* **Rotation (revocation).** The admin opens a slot, rotates (`chain` gains a record, `current_epoch` += 1), re-wraps the bundle of the new epoch for every slot that keeps access, removes the others, and writes the header with `expect` set to the version read. A slot wrapping an older epoch than `current_epoch` is **stale**: it still reads old data, but a client that opens it must not write, and an admin should re-wrap or remove it.
* **Checks on decode.** Exact length (no trailing bytes), known header version and suite, a policy that decodes (version 2), `n_chain = current_epoch`, every chain record of kind 3 with the right `key_epoch`, every slot with a valid prefix. A client also checks that a bundle it unwraps names the header's `fs`.
* **Writing a slot for oneself.** Header writes need admin. A member who wants a slot for a new passkey or password prepares the slot (the bundle never leaves the client unwrapped) and hands it to an admin, who adds it (`TD-FS-HEADER-SELF-SLOT`).

**Leakage and rollback.** The server sees the number of slots, their types and ids, the credential ids and device fingerprints in their parameters, the Argon2id parameters, the number of epochs, and the policy. It can't open a slot or forge one that opens to real keys. Of an **unsigned** header it can:
* drop slots or the whole header (denial of service; detectable only out of band),
* serve an older header: a client then sees an older `current_epoch`, and notices when it meets data sealed under a newer `key_epoch` than its keys can reach. It refuses to write under an epoch older than one it has seen (clients remember the highest epoch per fs),
* serve another policy. That changes only how new files are keyed: both key modes are end-to-end encrypted, and only sharing a single file depends on file keys,
* nothing else: a slot or chain record that was tampered with fails to open.

A **signed** header (§12.1) leaves it only the first point and serving an older version, which a client that pinned a newer one, or that verified a device checkpoint naming one (§14), detects.

### 12.1 Signed header

An admin signs each header version, chained like the ACL (§9):

```
signed header = u8 0x80 ‖ u8 suite = 1 ‖ u32 fs ‖ u64 version ‖ prev(32) ‖ signer(32) ‖ lp(header) ‖ sig
msg           = every byte before sig
sig           = hybrid signature (§7.3), purpose "zen/v1/sig/fs-header", of msg
HH            = BLAKE3.derive_key("zen/v1/fs-header-chain", msg)
```

* `header` is the header of §12, byte for byte. The first byte `0x80` can't be a header version, so a reader tells the two forms apart by it.
* `version` counts signed versions from 1. `prev` is `HH` of the previous signed version, or 32 zero bytes for version 1. `signer` is the admin's `user_fp`.
* **Acceptance** (the server on `/v1/fs/header/put`, api.md §4.4, and clients):
  1. `fs` is the header's fs, and `header` decodes far enough to name the same `fs`.
  2. `version` is the stored signed version + 1, or 1 when the stored header is unsigned or absent; `prev` is `HH` of the stored one, or zeros for version 1.
  3. `signer` is an admin of the current ACL, and `sig` verifies with its identity from the ACL's `members`.
  4. Once a header is signed, an unsigned one is refused.
* **Clients** pin `(version, HH)` per fs. They accept an unsigned header only while they hold no pin, and refuse a lower version than the pin, or the pinned version with another `HH`. A higher version needs only a valid signature from an admin of the ACL they have verified; past versions are not kept.
* A device checkpoint (§14) names the `(version, HH)` its device verified, so a server that keeps serving one client an older version is caught by the clients that see that checkpoint.

Vectors: `test-vectors/header.json`; the signed form is in `test-vectors/integrity.json`. The ACL encoding (§9) has vectors too: `test-vectors/acl.json`.

## 13. Relayed services (svc.md)

`svc` is the service id (a topic id), `conn` the 16-byte connection id. The **service key** of epoch `e` is

```
K_svc = KDF("zen/v1/svc-aead", D_svc, "")        D_svc: the topic data key of svc at epoch e (§3.3)
```

It seals the state (kind 9, §4) and the output-key escrows (kind 10). The state's plaintext is defined in svc.md §7.

### 13.1 Output record

What a step appends to a connection's output (api.md §14.5), stored verbatim at its output offset `off`:

```
sealed record (flags & 4 = 0):  u8 version = 1 ‖ u8 flags ‖ key_hash(32) ‖ escrow(80) ‖ body
plain record  (flags & 4 = 4):  u8 version = 1 ‖ u8 flags ‖ raw

key_hash = BLAKE3.derive_key("zen/v1/svc-reveal", k_r)
escrow   = kind-10 sealed object (§4) of k_r, under K_svc
body     = ChaCha20-Poly1305(k_r, nonce = 12 zero bytes,
                             aad = "zen/v1/aad/svc-out" ‖ 0x00 ‖ u32(fs) ‖ lp(svc) ‖ conn ‖ u64(off) ‖ u8(flags),
                             raw)                                   (raw ‖ 16-byte tag)
```

* **Flags:** 1 FIN after this record, 2 close after it, 4 plain, 8 abort when the writer reaches it. Other bits are zero. At most one of FIN, close and abort is set.
* **The record key `k_r`** is 32 random bytes, fresh for every record of every commit attempt. It encrypts exactly one message, which is why the nonce can be zero. It is never derived from the offset: a rebuilt step must get new keys (svc.md §5).
* **Raw length.** A sealed record's raw length is `len − 130`, and a plain record's is `len − 2`. It advances `out_head`, and the next record's `off` is this `off` plus it.
* **Empty records.** A record whose raw length is 0 carries FIN, close or abort.
* **Plain records** belong only to services with `resume: false`, and sealed records only to the others.

### 13.2 Reveal

To release a sealed record, a handler sends `k_r` (api.md §14.7). The server accepts it only if both hold:
1. `BLAKE3.derive_key("zen/v1/svc-reveal", k_r) = key_hash`
2. the body opens under `k_r` with the AAD above

The first check makes the key commit to the record (Poly1305 alone doesn't bind the key). The second catches a malformed record before the writer reaches it. A handler that has no `k_r` in memory gets it by opening the escrow under `K_svc` of the escrow's `key_epoch`, with context `u32(fs) ‖ lp(svc) ‖ conn ‖ u64(off)`.

### 13.3 Sealed inbox

* **The inbox key pair.** The service's `inbox` key is the X-Wing (draft-connolly-cfrg-xwing-kem) key pair whose 32-byte decapsulation seed is chosen at random by the admin and kept sealed in the fs. The public key (1,216 bytes) goes in the service configuration.
* **Per connection.** The owning node encapsulates once: `(ct, ss) = X-Wing.Encaps(inbox_pk)`. It stores `ct` (1,120 bytes) in the connection record (keyspace.md §3.9) and derives

  ```
  K_in = KDF("zen/v1/svc-inbox", ss, u32(fs) ‖ lp(svc) ‖ conn)
  ```

* **A sealed chunk.** Input at plaintext offset `off` is sealed as

  ```
  ChaCha20-Poly1305(K_in, nonce = u32(0) ‖ u64(off), aad = "zen/v1/aad/svc-in" ‖ 0x00, data)
  ```

  and stored as `u8 1 ‖ ciphertext ‖ tag`. A plain chunk is `u8 0 ‖ data`. Offsets and `in_head` count plaintext bytes, and chunks don't overlap, so each nonce encrypts one chunk. A batch resent after `commit_unknown` reuses its offset only with the same bytes (svc.md §3).
* **Opening.** A handler decapsulates `ct` with its seed, derives `K_in`, and opens each chunk at its offset. It checks that consecutive chunks are contiguous, so a dropped or reordered chunk fails.

Vectors: `test-vectors/svc.json`.

## 14. Device checkpoints

A **device checkpoint** is a signed statement by one device of the newest state it has written or verified, per object: topic heads and its own event chains (§5.1), filesystem trees (fs.md §9), zen-db namespaces, block volumes and relayed connections. Each device keeps one hash-chained sequence of them per fs, stored by the server (api.md §15). Checkpoints are how clients detect a server that **rolls back** an object or **withholds** its newest changes, and, through the cross-links in `seen`, a server that shows two groups of clients two different histories (a **fork**). Up to fork consistency (DESIGN.md §2.6), nothing else is needed.

### 14.1 Body

Deterministic CBOR (zendb.md §1), map keys unsigned integers:

```
CheckpointBody = { 1: v: 1,
                   2: fs: u32,
                   3: device: bytes(32),              // device_fp of the signer
                   4: seq: u64,                       // 1, 2, 3, … per (fs, device)
                   5: prev: bytes(32),                // HC(body of seq − 1), 32 zero bytes for seq 1
                   6: hlc: u64,                       // the device's clock (§11.1), informative
                   7: acl: [version: u64, hash: bytes(32)],     // the ACL head the device has verified (§9.2 H)
                   8: header: [version: u64, hash: bytes(32)],  // the signed fs header it has verified (§12.1 HH);
                                                                //   [0, 32 zero bytes] when none
                   9: objects: [Entry],
                   10: seen: [[device: bytes(32), seq: u64, hash: bytes(32)]] }

Entry = [1, topic: bytes, head: bytes(12), chains: [[chain_id: bytes(12), dseq: u64, dhash: bytes(32)]]]
      | [2, tree: bytes(16), ops: u64, chain: bytes(32), state: bytes(32)]
      | [3, ns: bytes, seq: u64, hash: bytes(32)]
      | [4, vol: bytes, seq: u64, hash: bytes(32)]
      | [5, svc: bytes, conn: bytes(16), in_acked: u64, out_head: u64]

HC(body) = BLAKE3.derive_key("zen/v1/checkpoint-hash", body)
```

* **Topic** (type 1): `head` is the offset of the newest event of the topic the device has read through or appended (the zero offset if none). `chains` lists the device's own chains on the topic (§5.1), sorted by `chain_id`, with the newest `dseq` that committed and `dhash = HE(its body)`: the `dprev` its next event will carry.
* **Tree** (type 2): the op count and op chain of the tree (§11.5) at a point where the device's replica was caught up, and the tree state there (fs.md §9).
* **Namespace** (type 3) and **volume** (type 4): a zen-db authenticated namespace (by its stored-key prefix, the 48-byte stored key of `D`) or a block volume, with the `seq` of its newest root and the hash of that root: for a namespace, the NsRoot and its `NH` (zendb.md §14.4). Volume roots are defined with volumes (milestone 6, a later step).
* **Connection** (type 5): a relayed connection (svc.md §12), with the cursors after the newest step the device committed or verified.
* **Order.** `objects` is sorted by type, then by its id bytes (`topic`, `tree`, `ns`, `vol`, then `svc` followed by `conn`), and holds at most one entry per object. `seen` is sorted by device, holds at most one entry per device, and never the signer.
* A decoder refuses another `v`, unknown entry types, entries out of order or repeated, ids of the wrong size and trailing bytes.

### 14.2 Signing and sealing

```
signed     = u32(len(body)) ‖ body ‖ sig          sig: the device's hybrid signature (§7.3),
                                                       purpose "zen/v1/sig/checkpoint", of body
checkpoint = kind-11 sealed object (§4) of signed, under the KV AEAD key of the current epoch,
             context u32(fs) ‖ device(32) ‖ u64(seq)
```

* The device signs with the identity of its device secret (§7.1). Its certificate (§7.4) comes from the verified ACL's `members`, or, for a device enrolled outside the ACL, from the server's device registry (api.md §15.2), verified against the member identity in the ACL.
* The server stores the checkpoint as is. It sees which devices checkpoint, how often and how large, but not which objects they name.
* A checkpoint must fit `max_value_bytes`.

### 14.3 Writing

* **Cumulative.** A body carries the device's newest entry for every object it has written or verified, carried forward from its previous body, not only those that changed. When the body would no longer fit, the entries unchanged for longest are left out (they stay in the older checkpoints). So a verifier needs one body per device: the head.
* **Cadence** (informative): a device checkpoints after `sig_every` changes it made (default 64), a few seconds after its last write (default 5 s), and on close. A smaller `sig_every` narrows the window in which a rollback of its own writes goes unnoticed by other devices; `sig_every = 1` closes it.
* **One sequence per device.** All processes of a device share it. A put that loses the race for `seq` (409, api.md §15.1) re-reads the head, merges its entries into that body's, and retries with the next `seq`.
* `seen` names the head of every other device the writer has verified, with `hash = HC(that body)`.

### 14.4 Verifying

A client keeps **pins** per fs: the head `(seq, HC)` of each device it has verified, its own included, and per object the newest entry it has verified from any device or written itself. On each checkpoint `c` of device `d`:

1. It opens `c`, and verifies `sig` with `d`'s certificate, which must verify against a member of the pinned ACL. `fs` and `device` must match, and `seq` the sealed context.
2. `seq` must be at least the pinned seq of `d`. At the pinned seq, `HC` must equal the pin. At the pinned seq + 1, `prev` must equal it. Two different bodies of `d` with one `seq` are a **fork**.
3. Each `seen` entry `(e, s, h)` must agree with what the client knows of `e`: if it has pinned `e` at `s`, `h` must equal that pin; otherwise it fetches `e`'s checkpoint `s` (api.md §15.1). A different body, or a seq the server can't produce (one beyond `e`'s head or the kept window), is a **fork**.
4. Each entry in `objects` raises the client's per-object pin. It is **refused** when it is lower than the pin of the same writer, which is a fork too.

Then, for every server answer about an object, the **freshness rule**: an answer older than the object's pin is a **rollback**:
* a topic whose newest offset is below the pinned `head`, or a chain whose events end before the pinned `dseq` (a withheld tail, GAPS G26)
* a tree whose `ops` is below the pinned `ops`, or at the same `ops` with another `chain` or `state`
* a namespace or volume root with a lower `seq`, or the same `seq` and another hash
* a relayed connection whose `in_acked` or `out_head` is below the pinned one
* an ACL or signed header with a lower version than the pinned one

The client reports a rollback or fork to the application and stops trusting the server for that fs. What it does then (refuse to write, alert, compare pins out of band) is the client's policy (docs/CLIENT.md).

**What it does not catch.** A server can split clients into groups that never see each other's checkpoints, and show each group a consistent history: fork consistency. The groups detect it as soon as any checkpoint crosses over, or when they compare pins out of band. An independent witness that cross-checks heads is later work (TD-WITNESS).

Vectors: `test-vectors/integrity.json`.
