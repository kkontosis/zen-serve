# Authentication: sign-in methods, credentials, origin policy

How clients sign in to zen-serve. The endpoints are in [api.md](api.md) §3, the byte formats in [formats.md](formats.md) §7 and §10, and the storage layout in [keyspace.md](keyspace.md) §3.5 and §3.7.

Signing in only gives **access to the server**: which encrypted data a member may read or write is the signed ACL ([formats.md](formats.md) §9), and reading it needs the fs keys, which the server never holds (§12).

The signed ACL stays the **source of truth for membership, grants and admins**. Every credential belongs to an ACL member. Removing a member from the ACL ends all of their sessions at once, whatever method created them, and deletes their stored credentials (§4).

## 1. Methods

| Id | Name (`/v1/info`) | Method | `[auth]` flag | Default | Signs the origin | Section |
|---|---|---|---|---|---|---|
| 1 | `device_key` | Device keys: a random per-device hybrid key, certified in the signed ACL | `device_keys` | on | yes | §6 |
| 2 | `passkey` | Passkeys (WebAuthn) | `passkeys` | on | yes (WebAuthn) | §7 (reserved) |
| 3 | `opaque` | Password via OPAQUE (augmented PAKE) | `opaque` | off | — | §8 (reserved) |
| 4 | `api_token` | API tokens: admin-issued bearer secrets for services and bots | `api_tokens` | off | no | §9 |
| 5 | `mtls` | TLS client certificates (native TLS or a trusted proxy) | `mtls` | on | no (TLS) | §10 (reserved) |
| 6 | `password_key` | Password-derived signing key: the password never leaves the client | `password_keys` | on | yes | §11 |

* The id is stored in session records and credentials; the name is used on the wire. Ids 2, 3 and 5 are reserved for methods not implemented yet.
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
```

`/v1/info` (api.md §2) carries:

```
auth: { methods: [text],     // offered methods, in id order
        default?: text }     // the method a client offers first
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
| `password_key` | the id of the user's password credential (§11) |
| `api_token` | the token's id (§9) |

The rest of the server treats the credential id as **"the device"**: fencing-token holders (api.md §8.2), the device of idempotency records (api.md §6), the device in the filesystem op chain (formats.md §11.5), and the ephemeral rate limit and `sender` (api.md §9). The id is stable for as long as the credential exists, so all sessions of one password credential act as one device. A client that needs distinct devices, for example to hold separate leases from two machines, uses device keys.

The session response's `device_fp` carries the credential id, and `method` names the method.

**Every request re-checks the session:**
1. The session exists and hasn't expired.
2. Its method is on (§2).
3. The user is a member of the current ACL; for `device_key`, the device is still certified under that member.
4. For the other methods, the credential still exists in the credential store (§4).

Steps 1–3 apply immediately on every node. A removed credential, like a logout, stops working at once on the node that removed it and within 10 s on the others (their session cache).

## 6. Method 1: device keys

A device holds a random 32-byte device secret (formats.md §7.1). Its user identity certifies it with a device certificate (formats.md §7.4), which an admin adds to the member's entry of the signed ACL.

To sign in, the device signs the challenge and the origin (formats.md §10) and calls `POST /v1/auth/session` (api.md §3.2). The server checks the challenge, the origin (§5), the membership, the certificate and the signature.

**Threats.**
* The device secret is the credential: whoever copies it can sign in as the device until an admin removes the certificate from the ACL.
* The signature binds the origin, so a malicious server can't relay a challenge from the real one (§5).
* Adding a device needs an admin-signed ACL change.

## 7. Method 2: passkeys (reserved)

> **Placeholder.** WebAuthn passkeys with a small RustCrypto-based verifier. The relying-party id derives from the origin policy (§5.5). Not implemented; `passkeys = true` has no effect yet.

## 8. Method 3: OPAQUE (reserved)

> **Placeholder.** Password sign-in with the OPAQUE augmented PAKE. It shares the login-name index of §4.2 with method 6. Not implemented.

## 10. Method 5: TLS client certificates (reserved)

> **Placeholder.** Sign-in with a TLS client certificate, either terminated by zen-serve itself (native TLS) or by a trusted reverse proxy that forwards the verified certificate. Not implemented; `mtls = true` has no effect yet.

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
