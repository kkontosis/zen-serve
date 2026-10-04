# Technical debt

Work that was knowingly deferred. Each entry has a **stable reference name**, `TD-<AREA>-<SLUG>`, that code comments, specs and issues can cite. An entry is never renumbered or renamed; when it is done, its status becomes `resolved` with a pointer to the change, and the entry stays.

Fields: **Status** (`open`, `in progress`, `resolved`), **Context**, **Why deferred**, **What it would take**.

## TD-AUTH-INVITES

* **Status:** open
* **Context:** Adding a member is an admin-signed ACL change (formats.md §9): the admin needs the new user's public identity, and for device keys a device certificate, before the user can sign in by any method. There is no invite link, no one-time enrolment code and no self-service onboarding.
* **Why deferred:** An invite flow needs a design of its own: who may invite, how an invitee proves possession of the invite, how the invitee's identity reaches an admin for signing, and how this fits the rule that the signed ACL is the only source of membership. The multi-method sign-in work (auth.md) did not need it.
* **What it would take:** An invite record in the keyspace (hashed one-time secret, expiry, inviting admin, intended grants); an unauthenticated endpoint where the invitee submits their public identity with the secret; a pending-members list that admins read and sign into the next ACL version; spec in auth.md and api.md; tests for expiry, reuse and revocation.

## TD-AUTH-WEBAUTHN-PRF-KEYSLOT

* **Status:** open (method 2, passkeys, may resolve it)
* **Context:** Passkeys (auth.md §7) only give server access. The WebAuthn PRF extension can return a per-credential secret on the client, which could unlock data keys the way a passphrase does.
* **Why deferred:** It needs a new keyslot type (formats.md §6) and passkeys are not implemented yet.
* **What it would take:** A keyslot type 4 whose secret is the PRF output for a fixed, per-slot salt; the slot stores the credential id and the PRF salt; new labels; vectors; and a fallback when the authenticator has no PRF support.

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
