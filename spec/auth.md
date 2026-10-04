# Authentication: sign-in methods, credentials, origin policy

How clients sign in to zen-serve. The endpoints are in [api.md](api.md) §3, the byte formats in [formats.md](formats.md) §7 and §10, and the storage layout in [keyspace.md](keyspace.md) §3.5 and §3.7.

Signing in only gives **access to the server**: which encrypted data a member may read or write is the signed ACL ([formats.md](formats.md) §9), and reading it needs the fs keys, which the server never holds (§12).

The signed ACL stays the **source of truth for membership, grants and admins**. Every credential belongs to an ACL member. Removing a member from the ACL ends all of their sessions at once, whatever method created them, and deletes their stored credentials (§4).

## 1. Methods

| Id | Name (`/v1/info`) | Method | `[auth]` flag | Default | Signs the origin | Section |
|---|---|---|---|---|---|---|
| 1 | `device_key` | Device keys: a random per-device hybrid key, certified in the signed ACL | `device_keys` | on | yes | §6 |
| 2 | `passkey` | Passkeys (WebAuthn): a key pair held by an authenticator | `passkeys` | on | yes (WebAuthn) | §7 |
| 3 | `opaque` | Password via OPAQUE (augmented PAKE) | `opaque` | off | — | §8 (reserved) |
| 4 | `api_token` | API tokens: admin-issued bearer secrets for services and bots | `api_tokens` | off | no | §9 |
| 5 | `mtls` | TLS client certificates (native TLS or a trusted proxy) | `mtls` | on | no (TLS) | §10 (reserved) |
| 6 | `password_key` | Password-derived signing key: the password never leaves the client | `password_keys` | on | yes | §11 |

* The id is stored in session records and credentials; the name is used on the wire. Ids 3 and 5 are reserved for methods not implemented yet.
* **Method 6 is the primary method**: `/v1/info` names it as `auth.default` whenever it is on.
* A server **offers** a method when it implements it and its flag is on. `/v1/info` lists exactly the offered methods (§2). A flag that is on for a method the server doesn't implement yet has no effect.

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
# Method 6 (§11): password_m_cost_kib, password_t_cost, password_p_cost,
#                 password_max_failures, password_lockout_secs.
```

`/v1/info` (api.md §2) carries:

```
auth: { methods: [text],               // offered methods, in id order
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
| `password_key` | the id of the user's password credential (§11) |
| `api_token` | the token's id (§9) |

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
                       name_hash?: bytes(32),       // methods with a login name (6; 3 later)
                       salt?: bytes(32), params?: {m_cost_kib, t_cost, p_cost},
                       identity?: bytes,            // method 6: the public identity
                       issued_by?: bytes(32),       // API tokens: the issuing admin
                       webauthn_id?: bytes,         // method 2: the WebAuthn credential id
                       cose_key?: bytes,            // method 2: the COSE public key
                       alg?: int,                   // method 2: its COSE algorithm
                       sign_count?: u32,            // method 2: the last signature counter
                       rp_id?: text,                // method 2: the relying-party id
                       last_used_unix?: u64 }       // method 2: the last sign-in
```

Fields a server doesn't know are ignored, so a method can add its own without breaking older readers.

* **Membership comes first.** A credential works only while its user is a member of the head ACL. An ACL version that removes a member deletes all their credentials in the same transaction, which also frees their login name. A member added back starts with none.
* **Removal.** Removing a credential ends its sessions (§3).
* **Limits.** A user holds at most 100 stored credentials.
* **Privacy.** The store never returns keys, salts or secret hashes, only metadata (api.md §3.9). In particular, the public identity of a password-derived key stays on the server: other members never see it, unlike the ACL's user identities. Each credential is readable only by its owner and by admins.

**Endpoints** (api.md §3.9). A user's own session can list and remove their own credentials, and add them through each method's registration endpoint (§7.2, §11.2). Admins can list and remove any member's. Sessions from API tokens can do none of this (§9).

### 4.1 Credential ids

| Method | Id |
|---|---|
| `passkey` | `BLAKE3.derive_key("zen-serve 2026 passkey", WebAuthn credential id)` (§7.2): the owner index (keyspace.md §3.7) finds the user from the id an authenticator returns |
| `password_key` | random, chosen at registration; a password change gets a new one |
| `api_token` | `BLAKE3.derive_key("zen-serve 2026 api token", secret)` (§9) |

Device keys are not in the store: their id is the device fingerprint, and the ACL holds them.

### 4.2 Login names

Methods where the user types a name (6 now, 3 later) find the account through a **login-name index**.

* **Normalization.** Surrounding whitespace is trimmed and ASCII letters are lowercased. The result must be 1–128 bytes of `a-z`, `0-9` and `. _ - @ +`: an email address fits. Other names are refused with 400. Unicode names are deferred (`TD-AUTH-UNICODE-LOGIN`).
* **Only a hash is stored**: `H(name) = BLAKE3.derive_key("zen-serve 2026 login name", normalized name)`. A dump holds no names; a guessed name can be checked against it, as with any unsalted index.
* **Unique across users.** A user can hold one login name per method; claiming a name another user holds returns 409 `name_taken`. That answer tells an authenticated member that the name exists.
* **Unknown names get fake parameters.** The parameter lookup (§11.2) answers an unknown name with the configured Argon2id parameters and a salt `BLAKE3.keyed_hash(K, "zen-serve fake password salt" ‖ 0x00 ‖ H(name))`. `K` is 32 random bytes, kept in the keyspace with the data, so every node, and a restored copy, gives the same answer. It is created on first use after the claim; before the claim no account exists, and each node uses a key of its own, so a server that was only started holds no data (operations.md §6). A name gets the same fake every time, so repeated lookups don't reveal which names exist.
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

**`origin_pinning_always`** (default false) keeps 7b in force even with `public_origins` set: the listed origins **and** the pinned first contact are both accepted. **This is risky**: until the first sign-in, any origin that matches the `Host` header is accepted and pinned, so a relay that gets there first is accepted permanently, next to the configured origins. Use it only while moving a deployment to a new origin, and check the pin.

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

**Algorithms** (`pubKeyCredParams`, in this order): EdDSA (COSE −8, Ed25519) and ES256 (COSE −7, ECDSA on P-256 with SHA-256). RS256 (−257) is not supported (`TD-AUTH-WEBAUTHN-RS256`), so an authenticator that only signs with RSA can't register.

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

The passkey is stored in the credential store (§4) with id `BLAKE3.derive_key("zen-serve 2026 passkey", credential id)`, and the credential id, the COSE public key, its algorithm, the signature counter, the rp id, the label and the creation time. The last sign-in time is added on each use.

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
5. `signature` verifies over `authenticator_data ‖ SHA-256(client_data_json)` with the stored key: ASN.1 DER for ES256 (either form of `s`), 64 bytes for EdDSA (strict verification).
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
* **Not post-quantum.** ES256 and Ed25519 are classical signatures. Whoever holds a passkey's public key (the server, a backup, an export) and a large quantum computer could forge its sign-ins. Methods 1 and 6 sign with a hybrid that includes ML-DSA-65.
* **Nothing secret on the server.** The store holds public keys only, so a dump allows no offline guessing, unlike password verifiers (§11.4).
* **No attestation.** Any authenticator, including software that copies keys, can register (§7.2). The counter (§7.4) is the only clone signal, and synced passkeys don't keep one.
* **Native apps.** App origins (`android:apk-key-hash:…`, `ios:…`) are not web origins; the origin policy refuses them.
* **A passkey session is a full interactive session.** It can register further passkeys, set a password (§11.2) and manage the user's credentials, like the other interactive methods.

### 7.7 Unlocking data keys

Signing in with a passkey gives server access only. Separately, a client can unlock data keys with the same passkey through the WebAuthn PRF extension: see `TD-AUTH-WEBAUTHN-PRF-KEYSLOT`.

## 8. Method 3: OPAQUE (reserved)

> **Placeholder.** Password sign-in with the OPAQUE augmented PAKE. It shares the login-name index of §4.2 with method 6. Not implemented.

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

## 10. Method 5: TLS client certificates (reserved)

> **Placeholder.** Sign-in with a TLS client certificate, either terminated by zen-serve itself (native TLS) or by a trusted reverse proxy that forwards the verified certificate. Not implemented; `mtls = true` has no effect yet.

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

**Failed-attempt limiter.** Each node counts failed sign-ins per login-name hash in memory, for unknown names too. After `password_max_failures` failures, the name is locked: every attempt gets 429 `quota`, even with the right password, until `password_lockout_secs` after the last failure. A success clears the count. Challenge and origin failures don't count.
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
| 2 `passkey` | yes | no (see `TD-AUTH-WEBAUTHN-PRF-KEYSLOT` in [TECH_DEBT.md](TECH_DEBT.md)) |
| 3 `opaque` | yes | not specified yet |
| 4 `api_token` | yes | no |
| 5 `mtls` | yes | no |
| 6 `password_key` | yes | yes: the client holds the password, so it can also open or create a passphrase keyslot (formats.md §6, type 1) |

For method 6 the sign-in key and a passphrase keyslot are independent derivations, with separate salts and labels: neither reveals the other. Using the same password for both is the user's choice. The server already holds an offline-guessable verifier for each (§11.4).

## 13. Adding a method

For implementers of the reserved methods (3, 5). A method:
1. Adds itself to `auth::IMPLEMENTED` in zen-server once it works; its `AuthMethod` variant, id, wire name and `[auth]` flag already exist.
2. Calls `AppState::require_method` first in each of its endpoints.
3. Stores its credentials with `cred::put`, as a `CredRecord` with its method id and any new optional fields it needs, and finds them with `cred::get`, `cred::owner`, `cred::list` and, for a typed name, `cred::login` (§4.2). Credential removal, listing, the per-user limit and the clean-up when a member leaves the ACL then work unchanged.
4. Ends a sign-in with `auth::issue_session(user, credential id, method, signed)`. A method that signs a challenge and an origin passes them as `Signed`, which spends the challenge and applies the origin policy (§5) in the session's transaction, pinning the first origin. A method that signs no origin passes `None`.
5. Documents itself in its section here, in api.md §3 and in TECH_DEBT.md for what it defers.

Sessions of a method other than device keys stay valid only while their credential exists in the store (§3, step 4), so every such session must name a stored credential.
