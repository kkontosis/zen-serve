# Authentication: sign-in methods, credentials, origin policy

How clients sign in to zen-serve. The endpoints are in [api.md](api.md) §3, the byte formats in [formats.md](formats.md) §7 and §10, and the storage layout in [keyspace.md](keyspace.md) §3.5 and §3.7.

Signing in only gives **access to the server**: which encrypted data a member may read or write is the signed ACL ([formats.md](formats.md) §9), and reading it needs the fs keys, which the server never holds (§12).

The signed ACL stays the **source of truth for membership, grants and admins**. Every credential belongs to an ACL member. Removing a member from the ACL ends all of their sessions at once, whatever method created them, and deletes their stored credentials (§4).

## 1. Methods

| Id | Name (`/v1/info`) | Method | `[auth]` flag | Default | Signs the origin | Section |
|---|---|---|---|---|---|---|
| 1 | `device_key` | Device keys: a random per-device hybrid key, certified in the signed ACL | `device_keys` | on | yes | §6 |
| 2 | `passkey` | Passkeys (WebAuthn): a key pair held by an authenticator | `passkeys` | on | yes (WebAuthn) | §7 |
| 3 | `opaque` | Password via OPAQUE (augmented PAKE, RFC 9807) | `opaque` | off | no; bound to it by the OPAQUE context | §8 |
| 4 | `api_token` | API tokens: admin-issued bearer secrets for services and bots | `api_tokens` | off | no | §9 |
| 5 | `mtls` | TLS client certificates (native TLS or a trusted proxy) | `mtls` | on, dormant until configured | no (TLS) | §10 |
| 6 | `password_key` | Password-derived signing key: the password never leaves the client | `password_keys` | on | yes | §11 |

* The id is stored in session records and credentials; the name is used on the wire.
* **Method 6 is the primary method**: `/v1/info` names it as `auth.default` whenever it is on.
* A server **offers** a method when it implements it and its flag is on; method 5 also needs a way for client certificates to reach the server, and is **dormant** without one (§10). `/v1/info` lists exactly the offered methods (§2). A flag that is on for a method the server doesn't implement yet has no effect.

## 2. Configuration and `/v1/info`

```toml
[auth]
device_keys   = true    # method 1
passkeys      = true    # method 2
opaque        = false   # method 3
api_tokens    = false   # method 4
mtls          = true    # method 5
password_keys = true    # method 6
# Origin policy (§5): origin_pinning, origin_pinning_always, acl_origins.
# Method 2 (§7): passkey_rp_id, passkey_require_uv.
# Method 3 (§8): uses the password_* settings of method 6.
# Method 5 (§10): mtls_trusted_proxies, mtls_proxy_header; native TLS is [tls].
# Method 6 (§11): password_m_cost_kib, password_t_cost, password_p_cost,
#                 password_max_failures, password_lockout_secs.
```

`/v1/info` (api.md §2) carries:

```
auth: { methods: [text],               // offered methods, in id order (a dormant mtls is left out)
        default?: text,                // the method a client offers first
        origins?: {…},                 // the origin policy (§5.5)
        passkey?: {…},                 // method 2: relying-party id and options (§7.1)
        password_params?: {…} }        // method 6 registration parameters (§11.2)
```

`default` is the first offered method in the order `password_key`, `passkey`, `device_key`, `opaque`, `mtls`. API tokens are for services and never the default. Older servers send no `auth`; clients then assume `["device_key"]`.

**A method that is off:**
* Its endpoints refuse with 403 `method_disabled`.
* Sessions it already created are **kept but refused**: every request with such a session gets 401 `unauthorized` while the method is off. They work again if the method is turned back on before they expire. To end them for good, keep the method off until `session_ttl_secs` has passed, or remove the member's credentials (§4).
* Its credentials stay in the store, and can still be listed and removed.

The flags are read at start-up, from each node's own config. Every node of a cluster should use the same `[auth]` section; otherwise a session works on some nodes and not others.

## 3. Sessions

Every method ends in the same kind of session (api.md §3): a random 32-byte bearer token, stored hashed (keyspace.md §3.5). API tokens (§9) skip the session and are used directly as bearer tokens.

A session records the user, the **method** and a **32-byte credential id**:

| Method | Credential id |
|---|---|
| `device_key` | the device fingerprint (`device_fp`, formats.md §7.2) |
| `passkey` | the passkey's id in the credential store (§4.1) |
| `opaque` | the id of the user's OPAQUE credential (§8) |
| `password_key` | the id of the user's password credential (§11) |
| `api_token` | the token's id (§9) |
| `mtls` | the SHA-256 of the certificate's public key (§10.3) |

The rest of the server treats the credential id as **"the device"**: fencing-token holders (api.md §8.2), the device of idempotency records (api.md §6), the device in the filesystem op chain (formats.md §11.5), and the ephemeral rate limit and `sender` (api.md §9). The id is stable for as long as the credential exists, so all sessions of one password credential act as one device. A client that needs distinct devices, for example to hold separate leases from two machines, uses device keys.

The session response's `device_fp` carries the credential id, and `method` names the method.

**Every request re-checks the session:**
1. The session exists and hasn't expired.
2. Its method is on (§2).
3. The user is a member of the current ACL; for `device_key`, the device is still certified under that member.
4. For the other methods, the credential still exists in the credential store (§4).

Steps 2 and 3 apply immediately on every node. A logout or a removed credential (steps 1 and 4) takes effect at once on the node that handled it and within 10 s on the others (their session cache).

## 4. Credential store

Credentials of every method except device keys live in a **server-side credential store** in the keyspace (keyspace.md §3.7), not in the signed ACL. Each credential belongs to one user, the member who owns it, and has a random or derived **32-byte id**.

```
cred record (CBOR) = { method: u8, created_unix: u64,
                       expires_unix?: u64,          // API tokens
                       label?: text,
                       name_hash?: bytes(32),       // methods with a login name (6 and 3)
                       salt?: bytes(32), params?: {m_cost_kib, t_cost, p_cost},
                       identity?: bytes,            // method 6: the public identity
                       issued_by?: bytes(32),       // API tokens: the issuing admin; method 5: the admin who bound it
                       webauthn_id?: bytes,         // method 2: the WebAuthn credential id
                       cose_key?: bytes,            // method 2: the COSE public key
                       alg?: int,                   // method 2: its COSE algorithm
                       sign_count?: u32,            // method 2: the last signature counter
                       rp_id?: text,                // method 2: the relying-party id
                       last_used_unix?: u64,        // methods 2, 3 and 5: the last sign-in
                       opaque_record?: bytes,       // method 3: the OPAQUE registration record
                       opaque_ksf?: {m_cost_kib, t_cost, p_cost} }   // method 3: its Argon2id parameters
```

Fields a server doesn't know are ignored, so a method can add its own without breaking older readers.

* **Membership comes first.** A credential works only while its user is a member of the head ACL. An ACL version that removes a member deletes all their credentials in the same transaction, which also frees their login name. A member added back starts with none.
* **Removal.** Removing a credential ends its sessions (§3).
* **Limits.** A user holds at most 100 stored credentials.
* **Privacy.** The store never returns keys, salts, secret hashes or OPAQUE records, only metadata (api.md §3.9). In particular, the public identity of a password-derived key stays on the server: other members never see it, unlike the ACL's user identities. Each credential is readable only by its owner and by admins.

**Endpoints** (api.md §3.9). A user's own session can list and remove their own credentials, and add them through each method's registration endpoint (§7.2, §8.2, §10.3, §11.2). Admins can list and remove any member's. Sessions from API tokens can do none of this (§9).

### 4.1 Credential ids

| Method | Id |
|---|---|
| `passkey` | `BLAKE3.derive_key("zen/v1/passkey-id", WebAuthn credential id)` (§7.2): the owner index (keyspace.md §3.7) finds the user from the id an authenticator returns |
| `opaque` | random, chosen at registration; a password change gets a new one |
| `password_key` | random, chosen at registration; a password change gets a new one |
| `api_token` | `BLAKE3.derive_key("zen-serve 2026 api token", secret)` (§9) |
| `mtls` | `SHA-256(SubjectPublicKeyInfo)` of the certificate, DER (§10.3): the owner index finds the user from the certificate a connection presents |

Device keys are not in the store: their id is the device fingerprint, and the ACL holds them.

### 4.2 Login names

Methods where the user types a name, 6 and 3, find the account through a **login-name index**, with an entry per method (keyspace.md §3.7).

* **Normalization.** Surrounding whitespace is trimmed and ASCII letters are lowercased. The result must be 1–128 bytes of `a-z`, `0-9` and `. _ - @ +`: an email address fits. Other names are refused with 400. Unicode names are deferred (`TD-AUTH-UNICODE-LOGIN`).
* **Only a hash is stored**: `H(name) = BLAKE3.derive_key("zen-serve 2026 login name", normalized name)`. A dump holds no names; a guessed name can be checked against it, as with any unsalted index.
* **Unique across users, across methods.** A name belongs to at most one user, whichever method registered it: claiming a name another user holds for either method returns 409 `name_taken`. That answer tells an authenticated member that the name exists.
* **One name per method.** A user holds at most one name per method, and may use the **same name for both** (the usual case: one name, two credentials, each method holding its own), or different ones. A sign-in picks the method by its endpoint, not by the name: the client knows which method it registered. A client that offers both tries the default first (§2). Each method counts failed attempts on a name separately, with the full cap (§8.5).
* **Unknown names get fakes.** Method 3 answers an unknown name with OPAQUE's own fake record (§8.3). Method 6's parameter lookup (§11.2) answers an unknown name with the configured Argon2id parameters and a salt `BLAKE3.keyed_hash(K, "zen-serve fake password salt" ‖ 0x00 ‖ H(name))`. `K` is 32 random bytes, kept in the keyspace with the data, so every node, and a restored copy, gives the same answer. It is created on first use after the claim; before the claim no account exists, and each node uses a key of its own, so a server that was only started holds no data (operations.md §6). A name gets the same fake every time, so repeated lookups don't reveal which names exist.
  * **Residual leak.** A registered account whose parameters differ from the configured ones is distinguishable. Clients should register with the parameters `/v1/info` advertises (§11.2). A name that was registered and then removed gets a fake salt that differs from its old real one.

## 5. Origin policy

Methods 1 and 6 sign the **origin** the client sees, `scheme://host[:port]` (formats.md §10), together with a challenge. That stops a malicious server from relaying a sign-in to the real one: the relay's origin differs, so the server refuses the signature. It works only if the server knows its own origins. Three sources tell it, numbered 7a–7c after the decision that introduced them.

**Origin syntax.** An origin is `http://` or `https://`, then a lowercase ASCII host (a name, an IPv4 address or a bracketed IPv6 address), then optionally `:port` (1–65535, no leading zero), and nothing else: no path, no user info, no trailing slash. This is the form browsers serialize. Sign-ins with a malformed origin are refused (401); configured, pinned and ACL origins must be well formed.

### 5.1 7a: `public_origins`

The explicit list in the config file:

```toml
public_origins = ["https://zen.example.org"]
```

**Setting it is the hardening step.** While it is empty, the server still works, but prints a large multi-line warning at start-up on stderr (and in the log): it explains the relay risk, says whether first-contact pinning (7b) is in force, and shows how to set `public_origins`.

**Setting 7a turns 7b off**, unless `origin_pinning_always` is set (§5.2).

### 5.2 7b: pin the origin on first contact

`[auth] origin_pinning` (default **true**). While 7b is **in force**, that is, `origin_pinning` is true and either `public_origins` is empty or `origin_pinning_always` is true:

* The first origin accepted after the cluster is claimed is **pinned**: stored in the keyspace (keyspace.md §3.7). After that, only pinned origins are accepted from this source, and the `Host` header no longer vouches for any origin.
* **At the claim.** The claim request (`/v1/acl/put` for version 1, api.md §4.1) may carry `origin`, the origin as the claiming client sees it. The server pins it in the same transaction, if nothing is pinned yet. A malformed `origin` fails the claim with 400. While 7b is not in force, the field is ignored.
* **Otherwise at the first sign-in.** Without an origin in the claim, the first successful sign-in pins the origin it signed, whichever source accepted it. A cluster claimed before pinning existed is pinned at its next first sign-in. Pinning happens in the sign-in's transaction, so two first contacts through different origins conflict, and the second is refused.
* **Until the pin exists**, the origin is checked against the `Host` header, exactly as the development fallback of api.md §3.3. A relayed first contact pins the relay's origin. Admins check the pin with `/v1/admin/origins/get` and correct it with `/v1/admin/origins/set` (api.md §3.10). An empty set unpins, and the next sign-in pins again.

**`origin_pinning_always`** (default false) keeps 7b in force even with `public_origins` set: the listed origins **and** the pinned first contact are both accepted. The **first** sign-in pins its origin **whether or not** that origin is in `public_origins`: a first contact through a listed origin pins that origin, and the `Host` fallback closes with it. **This is risky**: until the first sign-in, any origin that matches the `Host` header is accepted and pinned, so a relay that gets there first is accepted permanently, next to the configured origins. Use it only while moving a deployment to a new origin, and check the pin.

The pinned set is kept, though unused, while 7b is not in force.

### 5.3 7c: origins in the signed ACL

`[auth] acl_origins` (default **false**). When on, the `origins` listed in the **head** ACL (formats.md §9.1) are accepted. They are admin-signed, so they change only with a new ACL version, and every client walking the chain sees them.

### 5.4 Precedence

The accepted set is the **union** of:
1. `public_origins` (7a),
2. the pinned origins, while 7b is in force,
3. the head ACL's `origins`, while 7c is on.

A sign-in whose origin is in the set is accepted. An origin outside it is accepted only by the **`Host` fallback**: the origin is `http://<Host>` or `https://<Host>` of the request, and either
* 7b is in force and nothing is pinned yet (the origin is then pinned), or
* 7b is not in force and the set is empty (development: no relay protection at all).

| `public_origins` | 7b `origin_pinning` | `…_always` | 7c `acl_origins` | Accepted |
|---|---|---|---|---|
| empty | on | — | off | the pin; before it, the `Host` origin (then pinned) |
| empty | off | — | off | the `Host` origin (no relay protection) |
| set | on or off | off | off | `public_origins` only |
| set | on | on | off | `public_origins` ∪ the pin; before the pin, the `Host` origin too (then pinned) |
| any | any | any | on | as above ∪ the head ACL's `origins`; with 7b out of force, listed ACL origins also close the `Host` fallback |

### 5.5 The server's own origins

`origin::own_origins` returns the accepted set in the precedence order above (7a in config order, then the pins, then the ACL's), and whether the `Host` fallback is open. The first entry is the **canonical origin**. Its host, without scheme and port, is the WebAuthn relying-party id that passkeys (§7) use. A server with no canonical origin can't offer passkeys, unless `passkey_rp_id` names the rp id (§7.1).

`/v1/info` advertises the same state, so a client can warn when it is talking to a relay or to a server without relay protection:

```
auth.origins: { origins: [text],       // accepted origins, canonical first
                pinning: bool,         // 7b is in force
                host_fallback: bool }  // other origins are accepted from the Host header
```

### 5.6 Threat notes

* The origin binding protects against a malicious server relaying a sign-in. It doesn't protect against a compromised client or a compromised server.
* With `public_origins` empty, the very first contact trusts the `Host` header. The claim, which comes from the operator holding the claim token, is the safest first contact; that is why it may carry the origin.
* Origins are compared byte for byte. A deployment reachable under several names lists all of them (7a or 7c), or pins all of them through the admin endpoint.

## 6. Method 1: device keys

A device holds a random 32-byte device secret (formats.md §7.1). Its user identity certifies it with a device certificate (formats.md §7.4), which an admin adds to the member's entry of the signed ACL.

To sign in, the device signs the challenge and the origin (formats.md §10) and calls `POST /v1/auth/session` (api.md §3.2). The server checks the challenge, the origin (§5), the membership, the certificate and the signature.

**Threats.**
* The device secret is the credential: whoever copies it can sign in as the device until an admin removes the certificate from the ACL.
* The signature binds the origin, so a malicious server can't relay a challenge from the real one, as far as the origin policy (§5) knows the server's origins.
* Adding a device needs an admin-signed ACL change.

## 7. Method 2: passkeys

WebAuthn passkeys, **on by default**. An authenticator (the platform's passkey manager or a security key) holds the private key; the server stores only the public key. The browser writes the origin it is talking to into every signed message and scopes each passkey to a relying party, so a passkey can't be phished or relayed (§7.6).

```toml
[auth]
passkeys = true
# passkey_rp_id = "example.org"   # default: the host of the canonical origin (§7.1)
passkey_require_uv = true         # refuse authenticators that didn't verify the user (§7.5)
```

The server verifies WebAuthn itself, with a small verifier on pure-Rust cryptography: no OpenSSL and no attestation trust (§7.2).

**Algorithms** (`pubKeyCredParams`, in this order): EdDSA (COSE −8, Ed25519), ES256 (COSE −7, ECDSA on P-256 with SHA-256) and RS256 (COSE −257, RSASSA-PKCS1-v1_5 with SHA-256). RS256 comes last, for authenticators that only sign with RSA, such as some Windows Hello TPMs; browsers pick the first algorithm an authenticator supports.

**RSA key policy.** An RS256 key (COSE `kty` 3, `n` and `e` as big-endian byte strings, leading zero bytes ignored) is refused at registration (400) unless:
* the modulus has **2048 to 4096 bits** and is odd: smaller keys are too weak, larger ones only cost verification time;
* the public exponent `e` is **odd, at least 65537 and at most 2³² − 1**, and below the modulus. Authenticators use 65537.

An RS256 signature must be exactly as long as the modulus, and its padding is checked in full.

### 7.1 Relying-party id

* The relying-party id (rp id) is `[auth] passkey_rp_id` when set, otherwise the **host of the canonical origin** (§5.5), without scheme or port.
* **No origin, no passkeys.** A server with neither has no rp id, for example a fresh cluster with no `public_origins`, nothing pinned and no ACL origins. Until it has one, the registration and sign-in requests return 400 with a message saying so, and `/v1/info` has no `auth.passkey.rp_id`. An origin is established by any of: `public_origins` (§5.1), a claim that carries `origin` (§5.2), the first sign-in with another method while pinning is in force (§5.2), or ACL origins (§5.3).
* `passkey_rp_id` must be a lowercase domain name. Browsers accept an rp id only on an origin whose host is the rp id or one of its subdomains; set it to a parent domain to share passkeys across subdomains. An origin whose host is an IP address can't use passkeys in browsers (`localhost` can).
* Each passkey records the rp id it was registered under, and its sign-ins are verified against that one. If the canonical origin moves to another host, existing passkeys stay bound to the old rp id: browsers won't offer them on the new host, and sign-in only lists passkeys of the current rp id. Users then register new ones.

`/v1/info` carries `auth.passkey: {rp_id?, user_verification, algorithms}` while the method is on (api.md §2).

### 7.2 Registration

A signed-in user adds a passkey, with a session of any interactive method (not an API token):

1. `POST /v1/auth/passkey/register/begin` (api.md §3.11) returns a challenge, the rp id, the **user handle**, the algorithms, the user's passkeys under that rp id (`excludeCredentials`) and the `userVerification` to ask for. The user handle is the user fingerprint: it names no person, and lets the authenticator return the account with a discoverable sign-in.
2. The client calls `navigator.credentials.create`. It chooses `user.name` and `user.displayName` itself; the server knows no names. It should ask for a discoverable credential (`residentKey: "preferred"` or `"required"`) so that sign-in needs no user name.
3. `POST /v1/auth/passkey/register/finish` with the attestation object, the clientDataJSON and an optional label.

The server checks, and returns 400 (401 for the challenge and the origin) otherwise:
* **clientDataJSON:** `type` is `webauthn.create`; `challenge` is a live challenge this cluster issued, not used yet (it is spent here); `crossOrigin` is not true; `origin` passes the origin policy (§5) in the registration's transaction, which pins it if it is the first, like a sign-in.
* **Attestation object** (CBOR `{fmt, attStmt, authData}`): `rpIdHash` = SHA-256(rp id); the user-present flag (UP) is set; user-verified (UV) is set when required (§7.5); backed-up (BS) only with backup-eligible (BE); the attested credential is present, with a credential id of 1–1023 bytes and a COSE key of a supported algorithm, and nothing follows except an extension map.
* **No attestation verification.** `fmt` `"none"` must carry an empty statement. The statement of any other format (`packed`, `tpm`, `apple`, …) is ignored, not verified: the server keeps no attestation roots and treats every passkey as unattested. It can't tell a hardware key from a software one (`TD-AUTH-WEBAUTHN-ATTESTATION`).
* The credential id isn't registered yet, by anyone (400). The user holds fewer than 100 credentials (429 `quota`).

The passkey is stored in the credential store (§4) with id `BLAKE3.derive_key("zen/v1/passkey-id", credential id)`, and the credential id, the COSE public key, its algorithm, the signature counter, the rp id, the label and the creation time. The last sign-in time is added on each use.

### 7.3 Sign-in

1. `POST /v1/auth/passkey/session/begin {user?}` (api.md §3.12), no session, returns a challenge, the rp id, `allowCredentials` and the `userVerification` to ask for.
   * **Without `user`** (preferred): a discoverable sign-in. The list is empty; the authenticator offers the passkeys it holds for the rp id and returns the user handle. No user name is typed.
   * **With `user`**, a user fingerprint the client remembers: the list holds that member's passkeys under the current rp id, for security keys whose credentials aren't discoverable. A fingerprint that isn't a member gets an empty list, like a member without passkeys. Anyone who knows a member's fingerprint can list the member's credential ids; WebAuthn credential ids are not secrets.
2. The client calls `navigator.credentials.get`.
3. `POST /v1/auth/passkey/session {credential_id, authenticator_data, client_data_json, signature, user_handle?}`.

The server checks all of these, or returns 401:
1. clientDataJSON parses; `type` is `webauthn.get`; `challenge` is a live challenge this cluster issued; `origin` is well formed (§5); `crossOrigin` is not true.
2. The credential id is a stored passkey; its user is a member of the head ACL; `user_handle`, if sent, is that user's fingerprint.
3. The challenge isn't used yet, and the origin policy (§5.4) accepts `origin`.
4. `rpIdHash` = SHA-256(the passkey's rp id); UP is set; UV is set when required (§7.5); BS only with BE.
5. `signature` verifies over `authenticator_data ‖ SHA-256(client_data_json)` with the stored key: ASN.1 DER for ES256 (either form of `s`), 64 bytes for EdDSA (strict verification), the modulus length for RS256.
6. The signature counter rule (§7.4).

The server then stores the new counter and the sign-in time, and issues the session with `issue_session` and the signed challenge and origin (§13): the challenge is spent and the origin policy applied, pinning a first origin, in the session's transaction. The session's `device_fp` is the passkey's id and `method` is `passkey`.

### 7.4 Signature counter

An authenticator may count its signatures. When the stored counter or the new one is non-zero, the new one must be **greater**; otherwise the passkey may have been cloned, and two copies are signing.

* **Refused and logged.** Such a sign-in gets 401 with a message naming a possible clone, and the server logs a warning with the passkey and user ids. The stored counter is not changed. If the passkey was cloned, the user or an admin removes it (api.md §3.9).
* Most synced passkeys don't count and always send 0; the rule then never applies.
* The counter is stored in its own transaction, just before the session's. A sign-in the session's transaction then refuses still leaves the counter advanced, which is correct: the authenticator did advance it.

### 7.5 User verification

`[auth] passkey_require_uv` (default **true**): the authenticator must have verified the user, by PIN or biometric (the UV flag), at registration and at every sign-in. A passkey alone signs in with all of the member's rights, so it should be two factors: the device and the user. The ceremony options ask for `userVerification: "required"`.

Set it to false to accept security keys that only test presence. The options then say `"preferred"`, and presence (UP) is still always required.

### 7.6 Threat notes

* **Phishing and relays.** The browser writes the origin it is on into clientDataJSON, and only lets a page use passkeys of an rp id its host belongs to. The server checks the origin against its policy (§5) and the rp id hash. A look-alike site on another domain can't get an assertion for the rp id at all, and a relayed assertion names the relay's origin. The origin policy's caveats apply (§5.6): while it falls back to the `Host` header, the first sign-in trusts that header.
* **Not post-quantum.** ES256, Ed25519 and RSA are classical signatures. Whoever holds a passkey's public key (the server, a backup, an export) and a large quantum computer could forge its sign-ins. Methods 1 and 6 sign with a hybrid that includes ML-DSA-65.
* **Nothing secret on the server.** The store holds public keys only, so a dump allows no offline guessing, unlike password verifiers (§11.4).
* **No attestation.** Any authenticator, including software that copies keys, can register (§7.2). The counter (§7.4) is the only clone signal, and synced passkeys don't keep one.
* **Native apps.** App origins (`android:apk-key-hash:…`, `ios:…`) are not web origins; the origin policy refuses them.
* **A passkey session is a full interactive session.** It can register further passkeys, set a password (§11.2) and manage the user's credentials, like the other interactive methods.

### 7.7 Unlocking data keys: the PRF keyslot

Signing in gives server access only. A client can also unlock data keys with the same passkey, through the WebAuthn **PRF extension** and a keyslot of type 4 (formats.md §6). The server is not involved: the slot sits in the fs header, which is opaque to it, and the PRF result never leaves the client.

* **Creating a slot.** The client, holding the fs keys, picks a random 32-byte salt, asks the authenticator for its PRF result (`extensions: {prf: {eval: {first: salt}}}` in a `navigator.credentials.get` limited to that passkey), and wraps the fs keys with the result. The slot stores the passkey's credential id (§4.1) and the salt. Whether an authenticator supports PRF shows at registration (`prf: {}` in the `create` extensions returns `prf.enabled`) or in the first `get`.
* **Unlocking while signing in.** With `session/begin {user}` (§7.3) the client gets the user's credential ids. It hashes them to store ids (§4.1), matches them to the fs header's type-4 slots, and passes each matching slot's salt in `prf.evalByCredential`. One touch then both signs in and returns the PRF result. A discoverable sign-in can't do this, because `evalByCredential` needs `allowCredentials`: the client then runs a second `get` for the passkey that signed in, whose store id is the session's `device_fp`.
* **Fallback.** Authenticators without PRF support, and browsers without the extension, can't unlock. Users keep another keyslot (passphrase, recovery key, device).
* **Removal.** Removing the passkey from the credential store (§4) doesn't remove its keyslot, nor the reverse: the slot keeps opening with the authenticator until it is removed from the fs header. As with any keyslot, a key bundle already unwrapped stays known; revoking it takes a rotation (formats.md §2).
* **Threats.** Whoever holds the authenticator, and passes its user verification, can open the slot; the server and a dump hold nothing that opens it. The PRF is a symmetric secret of the authenticator, so the slot doesn't depend on the classical signature algorithms (§7.6). Synced passkeys carry their PRF secret to the user's other devices, so the slot opens on all of them.

## 8. Method 3: OPAQUE

Password sign-in with **OPAQUE**, the augmented PAKE of RFC 9807, **off by default**. The client proves it knows the password without sending it, or anything from which the password can be guessed without the server's own keys. The server stores a **registration record** per credential and one cluster-wide **server setup**.

```toml
[auth]
opaque = true
# The Argon2id parameters, the failed-attempt limit and the lockout are
# method 6's: password_m_cost_kib, password_t_cost, password_p_cost,
# password_max_failures, password_lockout_secs (§11).
```

Next to method 6 (§11), which also signs in with a password:
* **Nothing to guess with outside the server.** Method 6 hands out the salt to anyone who asks and puts a password-derived signature on the wire. OPAQUE's record is useless without the server setup, and its messages are blinded: neither an eavesdropper nor another member gets anything to test guesses against (§8.7).
* **No precomputation.** Guessing needs the server's OPRF key, so it can't start before the server's data is stolen.
* **Mutual authentication.** The client finishes only if the server holds the record and the setup's key.
* **Not post-quantum**, unlike method 6's hybrid signature (§8.7).

Method 6 stays the default: it needs no second round, and its key is post-quantum. A server can offer both.

### 8.1 Cipher suite and server setup

**Suite** (zen-core `opaque`, from the `opaque-ke` crate): RFC 9807's ristretto255-SHA512 configuration.
* OPRF: ristretto255-SHA512 (RFC 9497).
* AKE: 3DH over ristretto255 with SHA-512.
* KSF: **Argon2id** v1.3 over the OPRF output, with a salt of 16 zero bytes, as RFC 9807 recommends for Argon2id (the OPRF already makes the input unique per user and server), a 64-byte output, and the parameters stored with the credential (§8.2). The floors and ceilings are those of formats.md §6.
* Client and server identities: the RFC defaults, the two public keys. No names enter the record, so a login-name change needs a new registration anyway (§8.2), and the server's origin may change without breaking records.
* **Credential identifier:** `H(name)`, the login-name hash of §4.2. The server derives each name's OPRF key from it and the setup's seed, for unknown names too, so a fake record evaluates the OPRF exactly as a real one would.
* **Context:** `"zen/v1/opaque" ‖ 0x00 ‖ origin` (labels.md), the origin the client sees (§8.4).

Ristretto255 with 3DH and SHA-512 is one of the RFC's configurations, the one `opaque-ke` uses by default and checks against the RFC's test vectors; it is pure Rust and builds for wasm32, so browser clients can use zen-core. Argon2id is what the rest of zen-serve already uses for passwords (formats.md §6, §7.5), with the same parameters and limits.

**Server setup.** One per cluster: the 64-byte OPRF seed, the server's AKE key pair and a public key for fake records, 128 bytes in `opaque-ke`'s encoding. It is stored at `pack("auth_key", "opaque")` (keyspace.md §3.7) and created on first use once the cluster is claimed; before the claim, each process uses one of its own for fake answers, so a server that was only started holds no data (operations.md §6).
* It is **data, not server metadata**: every node uses the same one, and backups and exports carry it.
* **Losing or replacing it invalidates every OPAQUE record**, and with them every type-5 keyslot (§8.6): users can no longer sign in with method 3 and must register again from a session of another method. A server never replaces a setup it can't decode; it refuses method-3 requests with 500 until the stored one is restored. Rotating the setup is not supported (`TD-AUTH-OPAQUE-SETUP-ROTATION`).
* A client may remember the server's public key from its registration and compare it at each sign-in (zen-core returns it).

### 8.2 Registration and password change

1. The client blinds the password (zen-core `opaque::Registration::start`) and calls `POST /v1/auth/opaque/register/start` (api.md §3.15) with a session of the user, the login name and the `RegistrationRequest`. The server checks the name (§4.2) and that no other user holds it, and returns the `RegistrationResponse` and the configured Argon2id parameters. This round stores nothing.
2. The client finishes (`Registration::finish`) with Argon2id parameters at least the registration floor of formats.md §6, usually the ones returned, and gets the **export key** (§8.6).
3. `POST /v1/auth/opaque/register/finish` with the **same login name**, the `RegistrationUpload` and the parameters. The server checks the name again, the upload's encoding and the parameters against the floors and ceilings, and stores a new credential with a fresh random id. Any earlier OPAQUE credential of the user is deleted in the same transaction.

The record is bound to the name's OPRF key: an upload sent with another name than its `start` stores a record that never signs in.

The same call is the **password change**, and also changes the login name. The old credential's sessions end, including the calling session if it signed in with the old password, as for method 6 (§11.2). A new registration gives a new export key even for the same password.

Any interactive session of the user may register: device key, passkey, password key or OPAQUE (`TD-AUTH-INVITES` covers a first registration without one). The old password is not asked for. A user holds at most one OPAQUE credential, and may hold a method-6 credential under the same name (§4.2).

### 8.3 Sign-in

1. The client starts (`opaque::Login::start`) and calls `POST /v1/auth/opaque/login/start {name, origin, request}` (api.md §3.16), without a session. The server answers with the `CredentialResponse`, the Argon2id parameters of the credential and a sealed **login state**.
2. The client finishes (`Login::finish`) with the password, the parameters (checked against the ceilings of formats.md §6 only) and the origin it sees. It gets the export key and the `CredentialFinalization`. With a wrong password, an unknown name or a server that used another origin, the client fails here and has nothing to send.
3. `POST /v1/auth/opaque/login/finish {state, finalization}`. The server opens the state, checks the finalization (the client's MAC over the transcript), that the name still points to the same credential and that its user is a member, and issues the session with `issue_session`, the state's challenge and the origin (§13). The session's `device_fp` is the credential id and `method` is `opaque`.

The server answers every credential failure at `finish` (unknown name, wrong MAC, a credential since removed or replaced, a user no longer a member) with the same 401, "unknown login name or wrong password". A state that is expired, used, tampered with or from another cluster also gets 401.

**The login state is stateless**, so `finish` may reach any node:

```
state     = challenge(32) ‖ nonce(24) ‖ XChaCha20-Poly1305(K, plaintext, aad = challenge)
K         = BLAKE3.derive_key("zen-serve 2026 opaque login state", challenge key)
plaintext = H(name)(32) ‖ u8 known ‖ user_fp(32) ‖ cred_id(32) ‖ u16 len ‖ origin ‖ ServerLogin
```

* The **challenge** is an ordinary one (api.md §3.1), made by `start`: its MAC and its 60-second expiry are checked first, and `issue_session` spends it, so a state finishes at most once in the cluster.
* `ServerLogin` is `opaque-ke`'s server state: the expected client MAC and the session key. They are secrets, which is why the state is encrypted, not only authenticated.
* The challenge key is server metadata (keyspace.md §3.4), shared by the nodes of a cluster but not exported: after a restore or import, sign-ins in flight start again.
* Nothing is written for a `start`, so unauthenticated callers can't use it to load storage.

**Unknown names** get OPAQUE's **fake record**, RFC 9807's defence against client enumeration: the response is built from a record with a random masking key and the setup's fake public key, with the OPRF key derived from the name like a real one. Its size and structure are those of a real response, and the masked parts are fresh random-looking bytes on every call, for real and fake records alike. The parameters returned are the configured ones. The state records that the name is unknown, and `finish` does the same work and gives the same 401.
* **Residual leak** (as §4.2): a credential registered with other parameters than the configured ones is distinguishable by them, so clients register with the parameters `register/start` returns. Timing differences of the record lookup are not hidden.

### 8.4 Origin binding

OPAQUE signs nothing, but its MACs cover a **context**: the client and the server both use `"zen/v1/opaque" ‖ 0x00 ‖ origin`, where the client takes the origin it sees and the server the one `start` named, sealed in the state.
* A relay that **forwards its own origin** gets a session refused by the origin policy (§5): `finish` passes the origin to `issue_session` like a signed one (`Signed`), which checks it in the session's transaction and pins it if it is the first (§5.2).
* A relay that **names the real origin** to the server while its victim sees the relay's: the server's KE2 MAC covers the real origin, so the victim's client refuses it and never sends a finalization.
* `start` refuses a malformed origin with 401; the policy itself is applied once, at `finish`, with the `Host` header of the `finish` request for the fallback (§5.4).

This is the protection of method 6 (§11.4): it keeps an honest client from being relayed, as far as the origin policy knows the server's origins. It doesn't stop a phishing page whose own code collects the typed password.

### 8.5 Failed-attempt limiter

Method 3 uses method 6's limiter (§11.3) with the same settings, `password_max_failures` and `password_lockout_secs`, but **counts on its own**: the limiter is keyed by the method and the login-name hash, so each method has the full cap for a name.
* **Every `start` counts as a failure**, for unknown names too: a credential response lets the client test one password guess offline, whether or not it finishes. A successful `finish` clears the name's method-3 count. A failed `finish` doesn't count again.
* While the name is locked for method 3, `start` returns 429 `quota`, even for the right password.
* **Separate from method 6.** A lock of one method leaves the other working for the same name, and a success with one method doesn't clear the other's count. A user holding the same name for both methods therefore allows up to twice `password_max_failures` guesses per lockout period and node, `password_max_failures` against each credential; with the same password for both, an attacker can spend both caps on it.
* **Across nodes.** `start` and `finish` may reach different nodes; the count is on the node of the `start`, the success on the node of the `finish`. A successful sign-in therefore records its time in the credential (`last_used_unix`), and a `start` first clears the node's method-3 count of a name whose last failure there is no newer than that time. Otherwise a load balancer that alternates nodes would lock users out after `password_max_failures` sign-ins.
* The other caveats of §11.3 stay: the limit is per node, and a restart clears it (`TD-AUTH-LIMITER-CLUSTER`).

### 8.6 Unlocking data keys: the export key

A registration and every sign-in with the same credential give the client the same 64-byte **export key**, which the server never sees. It opens a keyslot of type 5 (formats.md §6), which stores the credential id.
* **Creating a slot.** After `register/finish`, the client, holding the fs keys, wraps them with the export key of the registration and the credential id the call returned.
* **Unlocking while signing in.** The session's `device_fp` is the credential id: the client finds the matching type-5 slot in the fs header and opens it with the export key of the same sign-in. No second prompt.
* **A password change** registers a new credential with a new export key, so the old slot no longer opens. The client opens it before (it signs in with the old password, or holds the fs keys), then creates a slot for the new credential and removes the old one.
* **Online only.** Computing the export key needs the server's OPRF evaluation and the record, so a stolen fs header alone allows no offline guessing against the slot, unlike a passphrase slot (type 1). Whoever holds the record and the server setup can guess, as for sign-in (§8.7).
* **Losing the record or the setup** (removing the credential, a member leaving the ACL, a lost setup) makes the slot unopenable for good. As with passkey slots (§7.7), users keep another keyslot.

### 8.7 Threat notes

* **A stolen database.** The record and the server setup are both in the keyspace, so the server, a backup and an export hold everything needed to **guess offline**, at one Argon2id run per guess (the KSF, §8.1), as for method 6 (§11.4). OPAQUE adds nothing against that attacker beyond Argon2id: strong parameters and strong passwords are the defence. Whoever holds the records but not the setup can't guess.
* **Eavesdroppers and other members** get nothing to guess with: the OPRF input is blinded, the masked response opens only with the password, and nothing about a credential is listed to anyone (§4). Method 6, in comparison, gives out the salt, and every sign-in carries a password-derived signature against which whoever sees it can test guesses.
* **Online guessing** costs one `start` per guess, which the limiter caps (§8.5). A `start` costs the server one OPRF evaluation and one 3DH, and the client one Argon2id run.
* **No precomputation.** Guesses can't be prepared before the server's data is stolen: the OPRF key enters every one.
* **Not post-quantum.** Ristretto255 is a classical group. A large quantum computer would let whoever recorded a sign-in solve its discrete logarithms: recover the name's OPRF key, guess the password offline against the recorded response, and impersonate the server to the client. The session token itself is protected by TLS (operations.md §8). Methods 1 and 6 sign with a hybrid including ML-DSA-65.
* **Lockout as denial of service**, and **phishing**: as method 6 (§11.4).
* **Enumeration.** Responses look the same for known and unknown names (§8.3), up to the parameter leak and timing.
* **Mutual authentication.** A client that finishes knows the server holds the record and the setup's private key; a relay without them can't complete the AKE with it.
* **The server setup is a key.** Protect backups and exports accordingly (operations.md §5.1, §6). Losing it locks out every method-3 user and their type-5 slots (§8.1).

## 9. Method 4: API tokens

Bearer secrets for services and bots, **off by default** (`[auth] api_tokens = false`).

* **Issuing.** An admin, signed in interactively, calls `POST /v1/auth/tokens/create` (api.md §3.8) for a **member**, with an optional label and expiry. The server picks a 32-byte random secret and returns it **once**, as the token text `zen_at_` ‖ base64url(secret) (50 characters). It stores only the id, `BLAKE3.derive_key("zen-serve 2026 api token", secret)`, in the credential store (§4), with the label, the expiry and the issuing admin.
* **Use: directly, without a session.** A service sends `Authorization: Bearer zen_at_…` on every request, and the UTF-8 bytes of the same text as the stream's `auth` token (api.md §9). The prefix and the length tell it apart from a session token. Exchanging tokens for sessions would add a renewal loop to every service for no security gain: a session token is a bearer secret too.
* **Checked on every request**, like a session (§3): the method is on, the credential exists, it hasn't expired, and its user is a member of the head ACL. A node caches the lookup for up to 10 s.
* **Rights.** A token acts as its member, with the member's grants, admin included. It can't manage sign-in: `/v1/auth/credentials/*`, `/v1/auth/password/set`, `/v1/auth/tokens/create` and `/v1/admin/origins/*` refuse it with 403. `/v1/auth/logout` refuses it with 400.
* **The device.** The token id is the caller's device (§3): fencing tokens, idempotency, the op chain and the ephemeral rate limit are per token.
* **Listing and revoking.** `/v1/auth/credentials/list` shows tokens as metadata (id, label, created, expires); the secret can't be shown again. `/v1/auth/credentials/remove` revokes one: at once on the node that revokes, within 10 s on the others. Revoking takes an admin, or the member signed in interactively.
* **Expiry.** An expired token is refused at once, and the sweeper deletes it.

**Threats.**
* A token is a bearer secret with the member's full rights and no origin binding: whoever reads it, from a config file, a log or a CI variable, is the member until it is revoked or expires.
* Give each service its **own member**, with only the grants it needs (`TD-AUTH-TOKEN-SCOPES`), and an expiry.
* The server stores only a hash, so a dump or backup holds no usable tokens.
* A token never unlocks data keys (§12). A service that needs to read data also needs the fs keys, from a keyslot.

## 10. Method 5: TLS client certificates

Sign-in with a TLS client certificate, **on by default** but **dormant** until a certificate can reach the server. The certificate is checked twice: the TLS layer verifies it (zen-serve itself, or a reverse proxy zen-serve trusts), and the server then looks it up among the certificates **registered** to members. A certificate that verifies but isn't registered signs no one in.

```toml
[tls]                                    # native TLS (operations.md §8)
cert = "/etc/zen/api.pem"
key = "/etc/zen/api.key"
client_ca = "/etc/zen/clients-ca.pem"    # mode 1: ask for client certificates from this CA

[auth]
mtls = true
mtls_trusted_proxies = ["10.0.0.5", "fd00:1::/64"]   # mode 2: proxies that verify certificates
mtls_proxy_header = "x-client-cert"                 # the header they forward them in
```

Two ways for a certificate to arrive, usable together:

| Mode | Set up with | Who verifies the chain and the expiry |
|---|---|---|
| 1, native (§10.1) | `[tls]` with `client_ca` | zen-serve, in the TLS handshake |
| 2, trusted proxy (§10.2) | `[auth] mtls_trusted_proxies` | the proxy; zen-serve trusts its word |

**Dormant.** With `mtls` on but neither mode set up, the method can't work. The server logs that once at start-up, `/v1/info` leaves `mtls` out of `auth.methods`, and `/v1/auth/mtls/session` returns 401 saying so. Registration (§10.3) still works, so an admin can bind certificates before turning a mode on. Sessions the method issued earlier keep working while `mtls` is on: dormancy only stops new sign-ins. Turning `mtls` off refuses them, like any method that is off (§2).

### 10.1 Native TLS

With `[tls] client_ca` set and `mtls` on, the API listener asks every client for a certificate (operations.md §8):
* **Optional at the TLS layer.** A client without a certificate still connects, so browsers and the other sign-in methods work on the same port.
* **Verified when sent.** A certificate must chain to a CA in `client_ca`, be valid now (not before, not after), and allow client authentication (extended key usage `clientAuth`, if the extension is present). Otherwise the **handshake fails**: the client gets a TLS alert, not an HTTP response. A browser that offers a wrong certificate therefore can't reach the server on that connection at all.
* **Keys and signatures**, for the client certificate, its CA chain and the handshake signature:
  * ECDSA on P-256 or P-384, and Ed25519;
  * RSA with a 2048- to 4096-bit modulus and an odd public exponent from 65537 to 2³² − 1, the policy of passkeys (§7): PKCS#1 v1.5 or PSS signatures with SHA-256, SHA-384 or SHA-512 on certificates, and PSS in the handshake, as TLS 1.3 requires.

  A key or CA outside these fails the handshake: a 1024-bit RSA CA, for example. RSA verification uses the pure-Rust `rsa` crate's release candidate (`TD-AUTH-WEBAUTHN-RS256` has the follow-up); only its public-key operations are used.
* **No revocation checks.** zen-serve reads no CRLs and asks no OCSP responder (`TD-AUTH-MTLS-REVOCATION`). To revoke a certificate, remove its registration (§10.3); to revoke a whole CA, remove it from `client_ca` and restart.
* With `mtls` off, the listener doesn't ask for certificates, and `client_ca` is not read.

The verified end-entity certificate is attached to the connection, and every request on that connection can use it.

### 10.2 Trusted proxy

zen-serve speaks plain HTTP (or TLS without `client_ca`) behind a reverse proxy that terminates the clients' TLS, verifies their certificates itself, and forwards the verified certificate in a request header (operations.md §8.3).
* `[auth] mtls_trusted_proxies`: the proxies' addresses, as IP addresses or CIDR blocks (`"10.0.0.5"`, `"10.0.0.0/8"`, `"fd00::/8"`). An address with bits set past the prefix is refused at start-up. IPv4-mapped IPv6 peers count as their IPv4 address.
* `[auth] mtls_proxy_header` (default `x-client-cert`): the header's name, lowercase.
* **Formats.** The header carries the whole certificate, as either:
  * PEM, URL-escaped: nginx's `$ssl_client_escaped_cert`;
  * base64 DER, optionally URL-escaped: Caddy's `{http.request.tls.client.certificate_der_base64}`, HAProxy's `%[ssl_c_der,base64]`, Traefik's `X-Forwarded-Tls-Client-Cert`.

  Of a chain, the first certificate counts: the first PEM block, or the base64 text up to the first comma. An empty header means no certificate. A fingerprint alone (such as nginx's `$ssl_client_fingerprint`, a SHA-1 of the certificate) is not enough: the server needs the public key (§10.3).
* **From a trusted proxy**, the header is the only source: no header means no certificate, even when the proxy's own connection presents one (that certificate is the proxy's, not the user's). A header that doesn't parse gets 401, and a warning in the log.
* **From any other address**, while proxy mode is set up, a request to an mTLS endpoint that carries the header is **refused** with 401, and the server logs a warning (at most one a minute). Refusing rather than ignoring makes a misconfigured proxy, or a client trying to name someone's certificate, visible. Without proxy mode the header means nothing and is ignored. Other endpoints never read it.

**zen-serve trusts the proxy completely.** It doesn't check the forwarded certificate's chain, dates or key usage: the proxy must have done that, and must have required the client to prove possession of the key in the handshake. Certificates are not secrets, so a header that a client could set itself would let anyone sign in as anyone. The proxy must therefore **always set or clear the header** on every request it forwards, and nothing else may reach zen-serve from a trusted address: no other clients and no other services on the proxy's host. operations.md §8.3 has an nginx example.

### 10.3 Binding certificates to members

A certificate signs in only after it is **registered** to a member, with `POST /v1/auth/mtls/register` (api.md §3.13). Each registration is a credential (§4).
* **By the member**, signed in with any interactive method (not an API token), on a connection that presents the certificate: natively or through the trusted proxy. Presenting it proves possession of the key.
* **By an admin**, for any member: the same way, or by uploading the certificate (PEM or DER). An uploaded certificate needs no proof of possession; it must still pass the TLS layer at each sign-in. The record notes the admin in `issued_by`.
* Refused (400): no certificate on the connection and none uploaded, a certificate that doesn't parse, a user who isn't a member, a label over 128 bytes, or a key that is registered already, to anyone. 429 `quota` past 100 credentials.

**The credential id is `SHA-256(SubjectPublicKeyInfo)`**, the DER public-key structure of the certificate: the key's fingerprint, the same value as the `pin-sha256` of RFC 7469 before base64. A certificate renewed **with the same key** (a new serial, new dates, even a new subject) keeps working without a new registration. A renewal with a new key needs one. Choosing the key rather than the whole certificate also means one key can be registered only once, whichever certificates carry it. An operator can compute the id with `openssl x509 -in cert.pem -pubkey -noout | openssl pkey -pubin -outform der | sha256sum`.

The id is the session's "device" (§3), so all sessions of one certificate key act as one device.

Mapping certificates to members by their subject or SAN, without a registration, is not supported (`TD-AUTH-MTLS-SUBJECT-MAPPING`).

### 10.4 Sign-in

`POST /v1/auth/mtls/session` with `{}` and no session (api.md §3.14) turns the certificate of the request's connection into an ordinary session (§3):
1. 403 `method_disabled` if `mtls` is off; 401 if the method is dormant.
2. The certificate: in mode 2 from a trusted proxy's header, otherwise the one the handshake verified (§10.2). None: 401.
3. Its id (§10.3) is a registered `mtls` credential, and its user is a member of the head ACL. Otherwise 401.
4. The sign-in time is stored, and the session issued with `issue_session` and **no signed origin** (§13): the handshake, not a signature, binds the sign-in. The session's `device_fp` is the credential id and `method` is `mtls`.

**Why a session endpoint, rather than the certificate on every request.** Every method ends in the same bearer session, so the rest of the server (the stream, idempotency, fencing, sessions across nodes) needs nothing new. The session token then works on any connection and any node, including nodes without native TLS. The certificate is checked once per sign-in, so a removed registration ends its sessions as for any credential (§3, step 4), and a certificate that expires ends nothing until the next sign-in, at the latest after `session_ttl_secs`.

Removing the registration through `/v1/auth/credentials/remove` (api.md §3.9) ends its sessions, and a member leaving the ACL loses all their registrations (§4).

### 10.5 Threat notes

* **Relay-proof natively.** In TLS 1.3 the client signs the handshake transcript, which includes the server's key share and certificate, so a sign-in can't be relayed through a server that doesn't hold the real server's key. The session token itself is a bearer secret, like every session (§3).
* **The proxy mode trusts the proxy.** Whoever can send requests from a trusted address, or misconfigure the proxy to pass a client's header through, can sign in as any member whose certificate they have, and certificates are public. Keep `mtls_trusted_proxies` to the proxies themselves, and the network between them and zen-serve private.
* **Not post-quantum for authentication.** The client's signature is ECDSA, Ed25519 or RSA (natively; whatever the proxy accepts in mode 2). A large quantum computer that recovers a certificate's private key from its public key could sign in with it. The **key exchange** of native TLS is post-quantum hybrid (`X25519MLKEM768`, operations.md §8), which protects the session token and the traffic against later decryption, not the sign-in against forgery. Methods 1 and 6 sign with a hybrid including ML-DSA-65.
* **No revocation** (§10.1): a stolen certificate key works until its registration is removed or the certificate expires (natively; in mode 2, as the proxy checks).
* **Expiry** is checked at the TLS layer natively, and by the proxy in mode 2; zen-serve doesn't check it in mode 2.
* **Linkability.** The credential id is a public function of the certificate, and it is the session's device id, which other members can see (for example as the `sender` of ephemeral messages, api.md §9). Someone holding the certificate can tell it signed in.
* **The client CA is a gate, not an identity.** Natively, any certificate of the CA passes the handshake; only the registration decides who it signs in as. A CA that issues to people outside the deployment lets them complete handshakes, but not sign in.
* **Nothing secret on the server.** The store holds the key fingerprint and metadata only.

## 11. Method 6: password-derived key (default)

The **primary method**. The client turns a password into a hybrid Ed25519 + ML-DSA-65 key pair (formats.md §7.5) and signs `challenge ‖ origin` with it. **The password never leaves the client**, and the server stores only a public key.

```toml
[auth]
password_keys = true
password_m_cost_kib = 262144    # Argon2id parameters advertised for registration (256 MiB, …
password_t_cost = 3             # … 3 passes, 1 lane: the browser recommendation of formats.md §6),
password_p_cost = 1             # and returned for unknown login names
password_max_failures = 10      # failed sign-ins per login name before a lockout
password_lockout_secs = 300
```

The configured parameters must meet the registration floor of formats.md §7.5.

### 11.1 Login names

Each user has at most one password credential, under one login name (§4.2). The name only finds the account; it is not an input of the key.

### 11.2 Registration and password change

1. The client reads `auth.password_params` from `/v1/info`, picks a random 32-byte salt and derives the key (formats.md §7.5).
2. It calls `POST /v1/auth/password/set` (api.md §3.7) with a session of the user: the login name, the salt, the parameters and the **public identity**.
3. The server checks the name (§4.2), the salt length, the parameters against the registration floor and ceilings, and that the identity decodes. It then stores a new credential with a fresh random id. Any earlier password credential of the user is deleted in the same transaction.

The same call is the **password change**, and also changes the login name. The old credential's sessions end, including the calling session if it signed in with the old password. The new credential has a new id, so to the rest of the server it is a new device (§3).

Any interactive session of the user may set the password: a device-key session (the usual way to add a password to a new member, see `TD-AUTH-INVITES`), a password session, or later a passkey session. The old password is not asked for; a stolen session can already act as the user.

### 11.3 Sign-in

1. `POST /v1/auth/password/params {name}` returns the salt and parameters, or stable fakes for an unknown name (§4.2). No session needed.
2. The client derives the key, with the sign-in ceilings of formats.md §7.5, gets a challenge (api.md §3.1) and signs the session message with purpose `zen/v1/sig/password-session` (formats.md §10).
3. `POST /v1/auth/password/session {name, challenge, origin, sig}` (api.md §3.6). The server checks the challenge and the origin policy (§5), then the signature against the stored identity, then that the user is a member. It answers every credential failure (unknown name, wrong key, not a member) with the same 401, and issues an ordinary session otherwise.

**Failed-attempt limiter.** Each node counts failed sign-ins per method and login-name hash in memory, for unknown names too. Method 3 counts separately, with the same settings (§8.5). After `password_max_failures` failures, the name is locked for method 6: every method-6 attempt gets 429 `quota`, even with the right password, until `password_lockout_secs` after the last failure. A method-6 success clears the method-6 count. A lock or success of method 3 doesn't affect method 6, nor the reverse. Challenge and origin failures don't count.
* The limit applies **per node**: a cluster of n nodes allows n times as many guesses.
* A restart clears it (`TD-AUTH-LIMITER-CLUSTER`).

### 11.4 Threat notes

* **Offline guessing.** The stored public identity, salt and parameters form a verifier: whoever holds them (the server, a backup, a dump) can test password guesses offline, at one Argon2id run per guess. That is the same exposure as a passphrase keyslot (formats.md §6), which the server also stores. Strong parameters and strong passwords are the defence.
* **Online guessing** costs the attacker one Argon2id run per guess and is capped by the limiter.
* **Lockout as denial of service.** Anyone who knows a login name can keep it locked. The account can still sign in by other methods, and an admin can raise `password_max_failures` or change the name.
* **Relay.** The signature binds the origin (§5), like a device signature.
* **Enumeration.** Parameter lookups and failed sign-ins look the same for known and unknown names (§4.2), up to the residual leak noted there and timing.
* **Phishing.** A look-alike site can collect the typed password. The origin binding stops it from signing in **through** the real server with a relayed challenge, but not from deriving the key itself once it has the password. Passkeys (§7) resist phishing; passwords don't.

## 12. Server access and data keys

Signing in never gives the server a key to the data. Which methods can also **unlock data keys** on the client:

| Method | Server access | Can also unlock data keys |
|---|---|---|
| 1 `device_key` | yes | yes: an X-Wing device keyslot (formats.md §6, type 3) opens with the device secret |
| 2 `passkey` | yes | yes, if the authenticator supports the WebAuthn PRF extension: a PRF keyslot (formats.md §6, type 4) opens with the passkey's PRF output (§7.7) |
| 3 `opaque` | yes | yes: an OPAQUE export-key keyslot (formats.md §6, type 5) opens with the export key of a sign-in with the credential (§8.6) |
| 4 `api_token` | yes | no |
| 5 `mtls` | yes | no: the certificate's private key stays in the TLS stack (the browser, the OS key store, a smart card, or the proxy), which can sign handshakes but derives no secret a keyslot could use |
| 6 `password_key` | yes | yes: the client holds the password, so it can also open or create a passphrase keyslot (formats.md §6, type 1) |

For method 6 the sign-in key and a passphrase keyslot are independent derivations, with separate salts and labels: neither reveals the other. Using the same password for both is the user's choice. The server already holds an offline-guessable verifier for each (§11.4).

## 13. Adding a method

For implementers of later methods. A method:
1. Adds itself to `auth::IMPLEMENTED` in zen-server once it works; it also adds its `AuthMethod` variant (a new id and wire name) in zen-proto and its `[auth]` flag.
2. Calls `AppState::require_method` first in each of its endpoints.
3. Stores its credentials with `cred::put`, as a `CredRecord` with its method id and any new optional fields it needs, and finds them with `cred::get`, `cred::owner`, `cred::list` and, for a typed name, `cred::login` (§4.2). Credential removal, listing, the per-user limit and the clean-up when a member leaves the ACL then work unchanged.
4. Ends a sign-in with `auth::issue_session(user, credential id, method, signed)`. A method that signs or MACs a challenge and an origin passes them as `Signed`, which spends the challenge and applies the origin policy (§5) in the session's transaction, pinning the first origin. A method that signs no origin passes `None`, as TLS client certificates do (§10.4): the handshake binds them instead.
5. Documents itself in its section here, in api.md §3 and in TECH_DEBT.md for what it defers.

Sessions of a method other than device keys stay valid only while their credential exists in the store (§3, step 4), so every such session must name a stored credential.
