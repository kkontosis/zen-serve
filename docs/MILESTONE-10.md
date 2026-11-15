# Milestone 10 plan: git over zen-serve (`git-remote-zen`, a local git server, a relayed git service), with post-quantum SSH

## Context

Milestone 9 is done (PR #83 merged, 54b9e27). DESIGN-3 §6 item 10 is next. The placeholder (docs/MILESTONE-10.md, kept below as "The placeholder") asks for `git clone zen://host/<fs>/<repo>` through a remote helper on the client, with the server blind:
* packs (with their `.idx`) stored as immutable files
* refs in a zen-db table
* **a push as one transaction:** the pack's file op, a compare-and-set of the refs, and an emit to `("git", repo, "pushes")` for CI workers
* fetch downloading only the packs it lacks; read and write rights from the fs grants

**Added by the user** (after milestone 9, "include all your proposals and go"):
1. **A local git server** (`zen-git serve`): a long-running process or user service on the laptop. It signs in through the vault as the device, decrypts locally, and serves git on localhost to stock `git`, IDEs and containers:
   * over SSH, through our own SSH engine (milestone 5.5), with the user's ordinary SSH keys
   * over smart HTTP, with a token the server hands out for git's credential helper, bound to 127.0.0.1 and checking `Host`
2. **The same server as a relayed handler** (milestone 5.5's model, as the mail server): teammates `git clone ssh://git@<host>/…` with stock git and nothing installed; the server stays a blind relay, and handlers take over from each other. Encryption ends at the handler machine, which the docs must say.
3. **Post-quantum SSH** (OpenSSH 10.6, October 2026, enabled the hybrid signature `ssh-mldsa44-ed25519`):
   * `ssh-mldsa44-ed25519` host keys and user keys in the SSH engine
   * `sntrup761x25519-sha512` key exchange, so OpenSSH 9.0–9.8 clients (Ubuntu 24.04: 9.6) get a post-quantum key exchange rather than `curve25519-sha256`
   * the classical leftovers documented: passkeys, OPAQUE (inside post-quantum TLS unless a classical proxy terminates it), mTLS client certificates, TLS server certificates
4. **A new milestone 12, zen-net:** a post-quantum private network between members' devices (a virtual LAN). Only its placeholder is written in this milestone (step 0).

**What is post-quantum already** (for the docs' summary): the X-Wing KEM for keys at rest, hybrid Ed25519 + ML-DSA-65 identities and the default sign-in, TLS `X25519MLKEM768`, and the SSH engine's `mlkem768x25519-sha256`.

## The placeholder (docs/MILESTONE-10.md as it was)

* **Moved earlier:** 7f/7g (packs over 175 MiB, streaming uploads), 7h (the helper signs in without a prompt). Already built: files plus rows in one commit (5c), emits (6a), the scheduler for repacking (6b).
* **Plan for the rest:** `packages/git` (a Node helper binary), the refs schema, a pack manifest for large repos, a gc/repack leader job, an optional localhost smart-HTTP bridge (`insteadOf`), a dumb-HTTP public mirror through M9's static push, tests with real `git` (clone, push, concurrent pushes, force).

## Progress and resuming

**Rhythm (as milestones 8 and 9):** one sub-step per PR. Before each one:
1. restart from main if the previous PR is merged
2. plan the sub-step in detail here, and commit the plan

On the user's "go": a `spec:` commit if needed, then code and docs. Log the step in both plan copies (this file and `docs/MILESTONE-10.md`, kept equal with `cp`). Push, confirm CI is green on both runs, and pursue every red to its root cause. Stop for compaction, and confirm with the user before implementation begins after one. Approval of a plan isn't a "go" unless the plan says so.

**Branch:** `claude/brave-knuth-gk03mm`. Push with retries (2, 4, 8, 16 s). Restart from main with `git fetch origin main && git merge --ff-only origin/main` when the branch holds only merged history.

**Constraints:**
* no model names in the repo; commit trailers as before
* `CARGO_INCREMENTAL=0`; never `pkill -f`; agents never push
* `ZEN_APPS_BUILT=1` for vitest; don't build zen-server while vitest runs; the debug zen-serve built with `--features test-utils`
* no data backward compatibility needed (the app isn't deployed)
* RAM is a first-class cost (OPS.md §6): the helper's and the local server's memory are measured
* delete only temp dirs this session made, after checking they aren't mounted

**Log:**
* **Step 0** (plan):
  * **On approval:**
    1. Restart from main by fast-forward (54b9e27).
    2. Bring milestone 9's last four log lines into `docs/MILESTONE-9.md`, at the end of its log (after item 5, "PR #83 then waits on review and merge.", just before `## Steps`), verbatim:
       > * Done: 2d60822 (STATS §7.6's CI table, TD-SW-THROUGHPUT's runner figures, the log) pushed.
       > * **CI green on both runs of 2d60822** (push 43700383145 and pull_request 43700388589), every job; PR #83 mergeable (clean). **Step 9.5 is done, and with it milestone 9;** PR #83 waits on review and merge. This line reaches `docs/MILESTONE-9.md` with the next step's restart from main.
       > * **PR #83 merged** (the user, 10:08 UTC). **Milestone 9 is done.** The PR's subscription ended with it, and the pending check-in was cancelled.
       > * **Next, on the user's word:** restart from main, bring these last lines into `docs/MILESTONE-9.md`, and plan the next milestone (DESIGN-3 §6 item 10, docs/MILESTONE-10.md).

       (indented as their neighbours, six spaces before `*`)
    3. Write `docs/MILESTONE-10.md` (this plan, replacing the placeholder, whose content is kept above).
    4. Write `docs/MILESTONE-12.md`, the zen-net placeholder (its text: "Step 0's placeholder for milestone 12" below).
    5. DESIGN-3 §6: item 10's scope (the helper, the local server, the relayed service, post-quantum SSH) and a new item 12 (zen-net, the link). README: "Milestone 10: in progress".
    6. Commit `docs: milestone 10 plan (git over zen-serve, post-quantum SSH) and the milestone 12 placeholder`, push, open the PR, and subscribe.
  * Then implement step 10.1 on the user's next "go".
  * **Approved** (the user). Restarted from main by fast-forward (54b9e27); milestone 9's last four lines are in `docs/MILESTONE-9.md`; this file copied to `docs/MILESTONE-10.md`; `docs/MILESTONE-12.md` written; DESIGN-3 §6 items 10 and 12 and the README updated.
  * Committed bc073c2 and pushed; PR #84 opened and subscribed. Next, on the user's "go": implement step 10.1 as "Step 10.1 in detail" says, starting by copying this file to `docs/MILESTONE-10.md` (this line included).
  * **CI green on both runs of bc073c2** (push 43706243558 and pull_request 43706256514); PR #84 mergeable (clean). A safety-net check-in is set for 11:45 UTC (trig_016RN9UQQTXVKYeA8C7af42H).
  * The 11:45 check-in found nothing new; check-ins stopped (the user hadn't written since the PR went up). The PR's subscription stays on.
* **Step 10.1** (post-quantum SSH):
  * **Go** (the user, after compacting). Started on PR #84's branch (or from main by fast-forward if #84 was merged), by copying this file to `docs/MILESTONE-10.md`; then item 1 (OpenSSH 10.6p1's sources read in the scratchpad), the `spec:`, `svc:` and `docs:` commits as "Step 10.1 in detail" says.
  * **Item 1, read from OpenSSH 10.6p1** (a8c72b8, cloned into the scratchpad):
    * **`sntrup761x25519-sha512`** (`kexsntrup761x25519.c`; `kex-names.c` gives both names the same KEM and SHA-512). Q_C = sntrup761 pk (1,158) ‖ X25519 (32), exactly 1,190 bytes or refused. Q_S = ciphertext (1,039) ‖ X25519 (32). K = string(SHA-512(kem secret (32) ‖ X25519 secret (32))), a 64-byte string. The X25519 secret goes through `kexc25519_shared_key_ext(…, raw=1)`, which refuses an all-zero result. H and the key derivation use SHA-512. OpenSSH 10.6's default kex list: mlkem768x25519, sntrup761x25519 (both names), then curve25519.
    * **`ssh-mldsa44-ed25519`** (`ssh-mldsa-eddsa.c`, after draft-ietf-lamps-pq-composite-sigs-18; the file names draft-miller-sshm-mldsa44-ed25519-composite-sigs-00):
      * public blob: string("ssh-mldsa44-ed25519") ‖ string(ML-DSA-44 pk (1,312) ‖ Ed25519 pk (32)), 1,344 bytes inside
      * private (`openssh-key-v1`): string(the 1,344-byte pk) ‖ string(ML-DSA seed ξ (32) ‖ Ed25519 seed (32)); the ML-DSA key is FIPS 204 KeyGen_internal(ξ) (libcrux)
      * signing: M′ = "CompositeAlgorithmSignatures2025" ‖ "COMPSIG-MLDSA44-Ed25519-SHA512" ‖ u8(0) ‖ (empty ctx) ‖ SHA-512(the data). ML-DSA-44 signs M′ with the FIPS 204 context "COMPSIG-MLDSA44-Ed25519-SHA512", **hedged** (32 random bytes, libcrux's `sign`); Ed25519 signs M′
      * signature blob: string("ssh-mldsa44-ed25519") ‖ string(ML-DSA sig (2,420) ‖ Ed25519 sig (64)); exactly that length, no trailing data; both halves must verify
      * the fingerprint is SHA-256 of the public blob, as every key's
      * rekeys: OpenSSH keeps the first exchange's host key (`initial_hostkey`) and renegotiates the algorithm from the same lists each time, so a server offering only the first algorithm on rekey is always accepted
    * **Crates:** `sntrup-kem` 0.1.0-rc.0's KATs (the IETF draft's two vectors), round-trip and size tests pass here; it builds for `wasm32-unknown-unknown` with getrandom 0.4's `wasm_js` (a 92 KB probe). Its `encapsulate` takes a rand 0.10 `CryptoRng`: a `rand_chacha::ChaCha20Rng` seeded from 32 `getrandom` bytes. `ml-dsa` 0.1.1's `sign_internal(Mp, rnd)` takes FIPS 204's external message pieces (`[0, |ctx|], ctx, M′`) and the 32 random bytes, so hedged signing needs no `rand_core` feature; verification is `verify_with_context(M′, ctx, sig)`.
    * Correction, in the code: ML-DSA's hedged signing is the public `ExpandedSigningKey::sign_randomized(M′, ctx, rng)` (the `rand_core` feature), not `sign_internal`; the rng for it and for sntrup761 is getrandom 0.4's `SysRng` (`sys_rng` feature, rand_core 0.10), so no ChaCha crate is added.
  * **`spec:` f04a9fe.** svc.md §7.2, suites.md's new "Post-quantum status", auth.md §8.7, TD-SVC-SSH-SCOPE and TD-SVC-SSH-PQ-CI. **A change from the plan:** `hk` is a top-level field of `engine_state`, not inside `kex`, since the connection keeps it after the key exchange (rekeys offer only it); a state lacking it after a negotiation, or naming a key the service doesn't hold, is refused.
  * **`svc:` 597072b.** kex.rs (sntrup761x25519 under both names, code 3; SHA-512 for K, H and the keys; host-key negotiation in `Chosen.hk`; `Kex::ALL` as the default order), keys.rs (`HostAlg`; `HostKey` of either kind, with `from_openssh`, `hybrid_from_seeds`, `generate` and `to_openssh`; hybrid `PublicKey`s verifying both halves), mod.rs (`Config::new(Vec<HostKey>, …)`, `host_keys`, `host_key(alg)`, `host_alg`; rekeys offer only the fixed algorithm; EXT_INFO's list), state.rs (`hk`, a 32- or 64-byte sid, the held bytes' slack 2,048 for a hybrid PK_OK), client.rs, zen-svc-wasm (`SshConfig` with a PEM or an array, kex aliases, `hostKeyLines`, `sshGenerateHostKey`), @zen/svc (`hostKey: string | string[]`). **Added beyond the plan:** `HostKey::to_openssh` and `sshGenerateHostKey`, so a hybrid host key can be made without OpenSSH 10.6 (and the TS tests have one); OpenSSH 9.6's and 10.6p1's `ssh-keygen -y` read our PEMs.
    * **Found by interop:** OpenSSH 10.6 lists `ssh-mldsa44-ed25519` *last* in its default `HostKeyAlgorithms`, so with both keys held a default client takes Ed25519, unless its `known_hosts` holds only the hybrid's line (it then moves that algorithm first) or it is told `HostKeyAlgorithms=ssh-mldsa44-ed25519`. Tested (`a_known_hybrid_line_is_preferred`, `both_host_keys_held`) and documented in CLIENT.md. And 9.6 knows sntrup761x25519 only by its `@openssh.com` name; the plain name is tested against 10.6p1.
    * **Verified here:** fmt; clippy `-D warnings` on zen-svc (default and `--all-features`), zen-svc-wasm (host and wasm32) and the workspace; `cargo test -p zen-svc` 3× green (unit 15, ssh 21, openssh 14 against 9.6, tls/openssl unchanged); openssh_pq 9/9 and openssh 14/14 against a local OpenSSH 10.6p1 build; `scripts/build-wasm.sh`; `npm run check`; `npx vitest run packages/svc apps/echo` 3× green (84 passed, the opt-in bench skipped).
    * **Negative controls**, each run once and restored, each failing its test: (1) the hybrid verified by its Ed25519 half only → `hybrid_needs_both_halves`; (2) sntrup's K with SHA-256 and (3) its H with SHA-256 → OpenSSH 9.6 refuses (`sntrup761_exec_with_each_cipher`); (4) a rekey offering both host-key algorithms → `rekeys_keep_the_host_key_algorithm`; (5) the restored ask refusing a hybrid key → `a_question_survives_a_restore`; (6) a restored `hk` the service doesn't hold accepted → `state_checks`.
  * **`test(svc):` adcd676**, the ignored `handshake_cost` measurement. **Costs** (STATS §5.5): the engine's key exchange is 0.2 to 1.4 ms in Rust and 2.5 to 8 ms per connection through the WASM in Node; the hybrid host key adds about 0.4 ms (Rust), sntrup761x25519 about 0.7 ms on the server (the client's key generation is the expensive part). `zen_svc_wasm_bg.wasm` is 1.36 MB, 503 KB gzipped (was 1.23 MB, 465 KB).
  * **`docs:`** SVC.md §3–§4, CLIENT.md's `ssh()` (the host keys, which OpenSSH takes which, the key exchanges by version), spec/OPS.md's OpenSSH note, STATS §4 and §5.5, this log. Next: push, CI green on both runs, then one `openssh-pq` dispatch run.
  * **CI on 597072b failed, for a reason outside this PR:** GitHub's `ubuntu-latest` had moved to Ubuntu 26.04 (Playwright's log: "Playwright does not support chromium on ubuntu26.04-x64"). The SSH tests all passed, against the runners' OpenSSH included. What failed, all on the new image:
    1. `rust` and `pure-rust`, both runs: `s_client_tls12_every_suite_and_group` (`crates/zen-svc/tests/openssl.rs`). The handshakes worked; the newer OpenSSL prints `Peer Temp Key` where 3.0 printed `Server Temp Key`. The test now accepts either. Negative control: a wrong expected group still fails it.
    2. `client` (pull_request run): Playwright 1.56.1 refuses to install Chromium's dependencies on 26.04.
    3. `client` (push run): the zen-mount vault test timed out at 60 s; it passed in the other run on the same image.

    The fix: every job is pinned to `ubuntu-24.04`, the image of every earlier green run, which the docs' runner versions also describe. `TD-CI-UBUNTU-2604` records the move to 26.04: a newer Playwright, the new OpenSSH and OpenSSL versions, and the mount test's timeout root-caused there. If the mount test times out on 24.04 too, it is this PR's to root-cause.

## Steps (one PR each; each planned in detail before it starts)

**10.1, post-quantum SSH** (the SSH engine, crates/zen-svc; detail below):
* `sntrup761x25519-sha512` (and `sntrup761x25519-sha512@openssh.com`) key exchange
* `ssh-mldsa44-ed25519` host keys (a service may hold both an Ed25519 and a hybrid host key) and user keys
* tests against the runners' OpenSSH 9.6 (sntrup761, every build) and OpenSSH 10.6p1 (the hybrid signature, the `openssh-pq` job), plus the crate's own test client
* the docs' post-quantum summary (a new spec section) and the classical leftovers

**10.2, the git spike and spec/git.md:**
* **The spike** (the scratchpad only), measured with real git 2.43:
  * the remote-helper capabilities: `fetch`/`push` over packs as files (git-remote-gcrypt's model) against `connect`/`stateless-connect` to a local `git upload-pack`; which one keeps `git clone`, `fetch --depth`, partial clone and `push --force-with-lease` working
  * how a pushed pack is cut: `git pack-objects --revs` with the remote's tips excluded, never thin, verified by `git index-pack --strict`
  * pack counts and sizes on a real history (this repo), for the manifest and repack thresholds
  * a pre-receive hook doing the zen transaction inside `git receive-pack` (quarantine objects reachable), for the servers' push
* **The spec** (`spec/git.md`):
  * the repo layout in an fs: `/<repo>.git/packs/<sha>.pack` and `.idx`, written once; the refs table (zen-db schema: repo, ref, oid, symbolic target, the pusher's device), HEAD
  * a push: the pack's file write, the refs compare-and-set (old oid → new; non-fast-forward refused on the client unless forced), and the emit, in one transaction
  * a fetch: the packs listed against those kept; the refs read at one version
  * gc and repack: a leader job writes a combined pack, then a transaction swaps the pack list; readers in flight keep the old packs until a grace
  * rights: the fs grants (read to fetch, write to push), which cover a whole fs, so a repo with its own rights lives in an fs of its own; branch protection is cooperative only
  * the local server and the relayed service (their auth below), and what the server, and a handler, see

**10.3, `packages/git` and `git-remote-zen`:**
* a Node package with a `git-remote-zen` bin: signs in through the vault without a prompt (7h), unlocks the fs, keeps a sealed local cache of packs
* clone, fetch, push, `--force`, `--force-with-lease`, deletes, tags, HEAD
* **tests with real git:** clone and push round trips, concurrent pushes (one wins, the other is refused and retries after a fetch), force, a push interrupted mid-upload (nothing visible, resumable), the emit seen by a consumer

**10.4, large repos and the public mirror:**
* the pack manifest (many packs as one listing), the repack leader job on the scheduler (6b), its grace
* a dumb-HTTP public mirror through milestone 9's `static push` (`info/refs`, `objects/info/packs`), for repos meant to be public
* tests: thousands of pushes repacked, a fetch during a repack, a clone over plain HTTP from the mirror

**10.5, the local server `zen-git serve`:**
* one long-running process (a systemd user unit and a launchd plist in docs) serving every repo the device can open
* **SSH on localhost**, through the SSH engine (zen-svc, with 10.1's algorithms): `git-upload-pack` and `git-receive-pack` exec requests; users authorized by an allowed-keys list the server keeps (Ed25519, RSA, `ssh-mldsa44-ed25519`)
* **smart HTTP on 127.0.0.1:** a token from `zen-git token`, stored through git's credential helper; `Host` checked against localhost names (DNS rebinding); never `0.0.0.0` unless configured
* **its core:** a local bare mirror per repo, synced from zen (packs) and served by real `git upload-pack`/`git receive-pack`; a pre-receive hook does the zen transaction before git updates its refs, so a push is atomic
* the mirror is plaintext on the laptop's disk (the docs say: disk encryption); its memory and disk measured
* tests: stock `git clone ssh://localhost:PORT/<fs>/<repo>` and `http://127.0.0.1:PORT/…`, a wrong key and a wrong token refused, a foreign `Host` refused, a push through the server seen by a `git-remote-zen` clone

**10.6, the relayed git service:**
* the local server registered as the handler of a relayed SSH service on zen-serve (milestone 5.5): stock git anywhere, `ssh://git@<host>:<port>/<fs>/<repo>`
* SSH keys mapped to members by a signed table in zen-db; a key's rights are its member's fs grants
* two handlers, one killed mid-clone and mid-push: the other takes over (svc.md §6)
* tests with OpenSSH 9.6 and 10.6p1 (a hybrid user key)

**10.7, benches, docs/GIT.md, the outcome:**
* benches: clone, fetch and push of this repo's history and of a large synthetic one, through the helper, the local server and the relayed service, against `git clone` from a local bare repo; memory (RSS) of the helper and the server
* docs/GIT.md (using it, the three ways, security of each), STATS, CLIENT.md, OPS pitfalls, TECH_DEBT, README, DESIGN-3 §6 item 10, the Outcome

## Step 0's placeholder for milestone 12

`docs/MILESTONE-12.md`, "Milestone 12: zen-net, a post-quantum private network between members' devices (placeholder)":
* **Feature:** members' devices form small private networks (a virtual LAN) where they see each other by address, as WireGuard peers do, with zen-serve as the rendezvous and, when needed, a blind relay.
* **Permissions:** a network is a zen-db object. Grants `net.join` and `net.admin`, a table of address assignments updated by compare-and-set, and optional per-peer rules. The server learns who belongs to which network, but never sees the traffic.
* **The handshake:** devices already hold hybrid ML-DSA-65 certificates and X-Wing keys. Peers verify each other's certificates against the ACL, run a post-quantum KEM in both directions, and rekey every few minutes.
* **The data plane, proposed:** WireGuard (the kernel's, or boringtun in userspace), given a post-quantum pre-shared key from that handshake and rotated, as Rosenpass does: hybrid security on audited code. Alternatives: a data plane of our own over UDP, or a userspace network stack that needs no root.
* **Getting packets through:** UDP hole punching with zen-serve as the rendezvous (the server then sees peers' public addresses); otherwise a blind relay of the encrypted packets through zen-serve, which costs server bandwidth and needs quotas.
* **First, a spike** (as 9.1): how often hole punching works, WireGuard with the key against userspace throughput, and what needs root. Linux first.
* **The rest is planned when the milestone starts.**

## What the later steps build on (survey, this session)

* **A push's transaction:**
  * Upload the pack with `tree.upload(node, src, {session: true})` (`packages/client/src/tree.ts:1224`) before the transaction.
  * Then `db.begin()` (`packages/db/src/db.ts:710`), a manual transaction with no retry, in which:
    * the refs rows are read (that puts them in the read set: a concurrent push gives `conflict`)
    * `tx.fs(tree).write(up)` (`packages/db/src/files.ts:19`) writes the pack's file
    * the rows are put
    * `tx.emit(db.topic('git', repo, 'pushes'), …)` (`txn.ts:208`) adds the event
  * The commit makes all of it visible at once; `tree.closeUpload` follows. Example: `packages/db/test/txn.test.ts:302`.
* **Repack:** `db.schedule` and `db.consume` (exactly once per group, in the owner), or `topic.leader` with `leader.ack(d, tx)` fencing (`packages/client/src/log.ts:497`).
* **Signing in without a prompt:** `credentialVault(sealedStore(nodeDiskLocalStore(dir)))`, then `vault.signIn`/`vault.unlock` (`apps/shared/node.ts:79`; zen-mount's `--vault`).
* **Rights:** fs grants cover a whole fs (`read`, `write`), so per-repo rights mean one fs per repo; anything finer is cooperative. Topic grants for `('git', …)`.
* **The relayed SSH handler:** `serve(db, {service, handler, layer: ssh({hostKey, …})})` (`packages/svc`).
  * Logins arrive as `c.question` and are answered with `c.answer(ok)`.
  * Commands arrive as `exec` events, and channels write with back-pressure.
  * `apps/echo/src/ssh.ts` already answers `git ls-remote` with a stub advertisement (tested with real git in `apps/echo/test/ssh.test.ts:145`).
* **Packaging:** `git-remote-zen` as an esbuild bundle with a `bin`, as `zen-connect` is (`scripts/build-zen-connect.mjs`).
* **Tools:** git 2.43 here; the runners ship git.

## Step 10.1 in detail: post-quantum SSH

The SSH engine (`crates/zen-svc/src/ssh/`) gains `sntrup761x25519-sha512` and `ssh-mldsa44-ed25519` (host keys and user keys), with the wasm binding and `@zen/svc` passing them through. There is no change for applications that don't opt in, beyond the new default key-exchange order.

**Survey (this session):**
* **Key exchange (`kex.rs`):**
  * `Kex::names()` (:20) lists each method's wire names, the first being its name; `from_name` (:32) and `code()`/`from_code()` (:38) map them, with codes 1 for mlkem and 2 for curve25519.
  * `negotiate()` (:156) takes the client's first known, configured name. The host-key list is the single constant `HOST_KEY = "ssh-ed25519"` (:57, used at :90 and :162).
  * `server_reply()` (:218) is the one server-side dispatch, and `ClientKex` (:251, test-utils) the client's.
  * The exchange hash and key derivation hardcode SHA-256 (:317, :329).
  * `state.rs:169` refuses a session id that isn't 32 bytes.
* **Host keys (`keys.rs`):**
  * `HostKey` is Ed25519 only (:16), built by `from_seed` or `from_openssh` (unencrypted `openssh-key-v1`, `ssh-ed25519` only, :82).
  * `Config` holds one (mod.rs:109). `on_kex_init` signs H with it (mod.rs:634, :648).
  * EXT_INFO's `server-sig-algs` is a hardcoded string (mod.rs:668).
* **User keys (`keys.rs`):**
  * `PublicKey::from_blob` (:152) has two kinds: Ed25519 and RSA. Then `allows` (:225) and `verify` (:233).
  * A restored `ask` re-parses its key (state.rs:384), so a new kind must parse there too.
* **Crates:**
  * zen-svc has `ml-kem` 0.3, `x25519-dalek` 3, `ed25519-dalek` 3 and `sha2` 0.11 (with `Sha512`).
  * `ml-dsa` 0.1.1 (already in the lock through zen-core, which uses `MlDsa65` in `crates/zen-core/src/sig.rs`) has `MlDsa44`.
  * No sntrup crate is in the lock. RustCrypto's `sntrup-kem` 0.1.0-rc.0 (the KEMs repository, as `ml-kem`; it ships KATs in `tests/kat.rs`) is the candidate. zen-svc already pins an rc for `rsa`.
* **Tests:**
  * `tests/ssh.rs` uses the in-process client. Its kex loops are at :155, :167 and :435, and host-key verification is hardcoded to `ssh-ed25519` (client.rs:255).
  * `tests/openssh.rs` runs the runners' OpenSSH 9.6, which has `sntrup761x25519-sha512`.
  * `tests/openssh_pq.rs` (ignored unless `ZEN_TEST_SSH`) runs OpenSSH 10.6p1, which `scripts/build-openssh.sh` pins. That version has `ssh-mldsa44-ed25519` on by default (its release notes).
  * Two tests use `"sntrup761x25519-sha512"` as an *unknown* name (kex.rs:368, client.rs:150).
* **Wasm and TS:**
  * `crates/zen-svc-wasm/src/ssh.rs`: `options()` (:281) matches kex names by `name()` only, so aliases fail. `SshConfig::new(host_key_pem, opts)` (:218). `key_value` (:152) gives `{algorithm, fingerprint, blob}`.
  * `packages/svc/src/ssh.ts`: `hostKey: string` (:26) and `kex?: string[]`.

### 1. The encodings, read from OpenSSH (the scratchpad only, before any code)

* Clone OpenSSH 10.6p1 at the pinned commit, as `scripts/build-openssh.sh` does, and read its sources, logging the exact layouts here:
  * **`sntrup761x25519-sha512`** (`kexsntrup761x25519.c`):
    * the client's init is the sntrup761 public key (1,158 bytes) ‖ its X25519 key
    * the reply is the ciphertext (1,039) ‖ the server's X25519 key
    * K = SHA-512(the sntrup shared secret ‖ the X25519 shared secret), as a string
    * H and the key derivation use SHA-512
  * **`ssh-mldsa44-ed25519`** (its `ssh-*.c` and `sshkey.c` entries), compared with draft-ietf-sshm-composite-sigs-00:
    * the public key blob
    * the signature blob and the message each half signs: any prefix, domain or pre-hash, and ML-DSA's context string
    * whether OpenSSH signs ML-DSA hedged or deterministically
    * the `openssh-key-v1` private key layout that `ssh-keygen -t mldsa44-ed25519` writes (a seed or the expanded key)
    * the fingerprint
    * whether a rekey may change the host-key algorithm (OpenSSH refuses a changed host key on rekey)
* Also: `sntrup-kem`'s KATs pass, and it builds for `wasm32-unknown-unknown` (zen-svc-wasm). If either fails, the fallback is `sntrup761` 0.4.0, checked the same way, and the log says so.

### 2. `spec:` commit (svc.md §7.2, suites.md, auth.md, TECH_DEBT)

* **§7.2 Algorithms:**
  * **Key exchange, preferred first:**
    1. `mlkem768x25519-sha256`
    2. `sntrup761x25519-sha512` (also `sntrup761x25519-sha512@openssh.com`)
    3. `curve25519-sha256`, under both names
  * **Host keys:** `ssh-mldsa44-ed25519` and `ssh-ed25519`. A service holds one key of each kind at most, at least one in all, and offers the hybrid first. The first exchange fixes the host-key algorithm, and rekeys offer only that one.
  * **User keys:** adds `ssh-mldsa44-ed25519`. Both halves must verify, as formats.md §7.5's hybrid.
* **§7.2 Key exchange:**
  * sntrup761x25519's encoding, as read in item 1
  * each method's hash (SHA-256, or SHA-512 for sntrup761x25519) used for H and the keys
  * the session id is the first H: 32 or 64 bytes
* **§7.2 EXT_INFO:** `server-sig-algs` = `ssh-mldsa44-ed25519,ssh-ed25519,rsa-sha2-256,rsa-sha2-512`.
* **§7.2 `engine_state`:** `kex.alg` 3 is sntrup761x25519. A new `kex.hk` field is the host-key algorithm (1 hybrid, 2 Ed25519), present once negotiated. `sid` is 32 or 64 bytes.
* **suites.md, a new section "Post-quantum status":** one table, each surface against what protects it now. The surfaces:
  * keys at rest (X-Wing)
  * identities and sign-in (hybrid ML-DSA-65)
  * TLS (`X25519MLKEM768`)
  * SSH key exchange (`mlkem768x25519`, `sntrup761x25519`)
  * SSH host and user keys (`ssh-mldsa44-ed25519`, or classical)

  And what stays classical, with why:
  * passkeys (WebAuthn's algorithms)
  * OPAQUE
  * mTLS client certificates
  * TLS server certificates (the WebPKI)
  * SSH clients that offer only `curve25519` (before OpenSSH 9.0)
* **auth.md OPAQUE's threat note:** a recorded sign-in also needs the TLS session broken, which native TLS's hybrid prevents. A proxy that terminates with classical TLS loses that.
* **TECH_DEBT:**
  * TD-SVC-SSH-SCOPE: sntrup and the hybrid key leave its list; `hostkeys-00@openssh.com` and certificates (`ssh-mldsa44-ed25519-cert`) stay.
  * TD-SVC-SSH-PQ-CI: sntrup761 is tested on every build by the runners' 9.6; the hybrid signature by the job started by hand.

### 3. `svc:` commit (crates/zen-svc, zen-svc-wasm, @zen/svc)

* **`kex.rs`:**
  * `Kex::Sntrup761X25519` (code 3, both names).
  * A hash selector per method (`Sha256`/`Sha512`) for `exchange_hash` and `derive`.
  * `server_reply`'s sntrup arm: encapsulate to the client's key, X25519 contributory as the others, the secrets zeroized and forgotten in the step.
  * `ClientKex` learns it.
  * `negotiate` picks the host-key algorithm from the client's list among the keys held, and `Chosen` gains it.
  * The default order is as the spec's.
* **`keys.rs`:**
  * `HostKey` becomes an enum, `Ed25519` or `MlDsa44Ed25519` (ML-DSA-44 from `ml-dsa`, as zen-core's `sig.rs` uses ML-DSA-65), with `blob`, `sign` and `public_line`.
  * `from_openssh` reads the hybrid's `openssh-key-v1` layout; `from_seed` gains a hybrid form for tests.
  * `PublicKey` gets `Kind::MlDsa44Ed25519`: `from_blob`, `from_line`, `allows`, and `verify` (both halves; Ed25519 strict).
* **`mod.rs`:**
  * `Config::new` takes one or two host keys.
  * `on_kex_init` signs with the chosen one.
  * KEXINIT offers only the fixed algorithm after the first exchange.
  * EXT_INFO's list.
* **`state.rs`:** `alg` 3, `hk`, and a `sid` of 32 or 64 bytes. The restored `ask` parses a hybrid key, and the bounds on a key's size are raised to fit one (1,344 bytes plus framing).
* **`client.rs`** (test-utils): offers sntrup, verifies either host-key kind, and signs userauth with a hybrid key.
* **zen-svc-wasm:**
  * `options()` accepts every name of a method (`Kex::from_name`).
  * `SshConfig::new` takes a PEM or an array of PEMs.
  * `hostKeyLines`, and `sshHostKeyLine` for either kind.
* **@zen/svc `ssh()`:** `hostKey: string | string[]`, and the docs of `kex` and `SshKey.algorithm`.
* **Cargo:** `sntrup-kem = "=0.1.0-rc.0"` (no default features but what the KEM needs) and `ml-dsa` 0.1 in zen-svc.

### Tests

* **Unit (kex.rs, keys.rs):**
  * sntrup761x25519: the client and the server agree on K; a short init is refused, and so is an all-zero X25519 result.
  * The negotiation tests use a made-up name (`unknown-kex@zen.test`) where they used sntrup.
  * Host-key negotiation follows the client's order among the keys held.
  * A hybrid user signature with a valid Ed25519 half and a broken ML-DSA half is refused, and the reverse too.
  * The hybrid's lines and fingerprints round-trip.
* **`tests/ssh.rs`** (the test client): the kex loops gain sntrup761x25519. Whole sessions, one byte at a time, and rekeys from either side mid-transfer, with each host-key kind and a hybrid user key.
  * A rekey keeps the host-key algorithm.
  * The state checks: a 64-byte sid restores, and 31, 33 and 65 bytes don't. A state with `alg` 3 or `hk` restores mid-kex.
* **`tests/openssh.rs`** (the runners' OpenSSH 9.6, every build): `KexAlgorithms=sntrup761x25519-sha512` (and its `@openssh.com` name) for exec with each cipher, and a 1 MB `cat` with rekeys.
* **`tests/openssh_pq.rs`** (OpenSSH 10.6p1, `ZEN_TEST_SSH`):
  * `HostKeyAlgorithms=ssh-mldsa44-ed25519` against a `known_hosts` line from `public_line`
  * a user key from `ssh-keygen -t mldsa44-ed25519` authenticates
  * a host key from `ssh-keygen -t mldsa44-ed25519` loads through `from_openssh` and gives the same public line and fingerprint as `ssh-keygen -y`/`-l`
  * the hybrid host key and the mlkem kex together
* **TS:** `packages/svc/test/sshwasm.test.ts` covers a hybrid host key, two host keys, a kex alias, and a hybrid user key's `algorithm`.
* **Negative controls,** each run once, then restored; each must fail its test:
  1. the hybrid verified by its Ed25519 half only (the broken-ML-DSA unit test)
  2. sntrup's K hashed with SHA-256 (OpenSSH 9.6 refuses the exchange)
  3. H for sntrup computed with SHA-256 (the same)
  4. a rekey that offers both host-key algorithms (the rekey test)
  5. the restored `ask` not parsing the hybrid (the state test)

### Docs (`docs:` commit)

* docs/SVC.md: the algorithms, and making a hybrid host key (`ssh-keygen -t mldsa44-ed25519`, OpenSSH 10.6 or later)
* CLIENT.md: `ssh()`'s `hostKey` array
* OPS.md: the OpenSSH line at :66, with which versions get which key exchange and which can use the hybrid keys
* STATS.md: the cost of a handshake with each kex and host key (time in Node through the wasm, and in Rust), so the hybrid signature's cost is known
* the log in both plan copies

### Verification

* `cargo fmt --check`; clippy `--all-targets -D warnings` for zen-svc and zen-svc-wasm (default and `--all-features`), and the workspace
* `cargo test -p zen-svc` 3 times
* the `openssh_pq` tests here against a local OpenSSH 10.6p1 build (`scripts/build-openssh.sh` into the scratchpad)
* `scripts/build-wasm.sh`; `npm run check`; `npx vitest run packages/svc apps/echo` 3 times
* CI green on both runs, plus one `openssh-pq` dispatch run (TD-SVC-SSH-PQ-CI says to start it after changing `kex.rs`)

**Defaults chosen (overridable):**
* **`sntrup-kem`** (RustCrypto, beside `ml-kem`), pinned exactly, checked by its KATs and by OpenSSH 9.6.
* **The hybrid host key is offered first** when a service holds both.
* **No certificates and no host-key rotation** (they stay in TD-SVC-SSH-SCOPE).
* **The default key-exchange order** is mlkem, sntrup, curve25519, as OpenSSH 10's own order.

## Verification (each sub-step)
* **Rust:** `cargo fmt --check`; clippy `--all-targets -D warnings` (default, `--no-default-features`, `--all-features`); the touched crates' tests.
* **JS:** `scripts/build-wasm.sh`; `npm run check`; the touched suites 3×, then the full `npx vitest run`; Playwright when the browser is touched.
* **Real tools:** OpenSSH 9.6 and 10.6p1 for SSH; git 2.43 (and CI's) for git.
* **Negative controls:** each new check run once disabled, and its test must fail. The log says so.
* **CI:** green on both runs of each PR.

## Risks
* **`ssh-mldsa44-ed25519` is new** (a working-group draft, enabled in OpenSSH 10.6). Its exact encoding is read from OpenSSH 10.6p1's source and checked by interop, not taken from secondary sources.
* **The remote-helper model** (packs as files) may not support every git feature (shallow and partial clones); the spike says which, and the docs list them.
* **The local server's mirror is plaintext on disk.**
* **A relayed handler sees the plaintext** of every repo it serves.
