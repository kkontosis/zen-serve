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
  * **CI on 0aabe09 (the pin):**
    * Green on both runs, every job: push 43732576357 and pull_request 43732582680.
    * Dispatch run 43732595233:
      * **`openssh-pq` passed**, the OpenSSH 10.6p1 tests on CI's runner.
      * **Its `client` job failed** in the kanban race test, "two concurrent moves of one card", with runs `[1, 3]` against `[1, 2]`. The code isn't this PR's, and it is the race's third failure (milestone 8 saw `[2, 3]`).
    * **The diagnostic narrows the cause:** one recorded re-run, but three runs. Every re-run inside `kv.ts`'s loop calls `retry`, so the extra run must be another transaction on that `Db` during the race, which `racing`'s instance patch counts. The proposed patch: count only the race's own transactions (an `AsyncLocalStorage` context), and list the others in `why` with their call site. It went to the PR as a comment and is not applied here. The failed job gets its one re-run once the dispatch's benches finish.
    * **On 26.04, a second zen-mount failure** (run 43732312501, on 2a5075b): `EACCES` unlinking a conflict file (`mount.test.ts:242`). It is added to `TD-CI-UBUNTU-2604`.
  * Pushed f40f48f, the log above.
  * **CI green on both runs of f40f48f**, every job: push 43733509068 and pull_request 43733515896. PR #84 is mergeable (clean). **Step 10.1 is done**, apart from the two items below.
  * **Left, once plan mode ends:**
    1. When dispatch run 43732595233's benches (`files-bench`, `devices-bench`) finish, re-run its failed `client` job once (`rerun_failed_jobs`). That is the one re-run the comment on PR #84 announced.
       * If it passes, log it.
       * If it fails again in the kanban race, that is a second failure on this PR's head. Tell the user, and ask whether to apply the proposed `racing` patch (an `AsyncLocalStorage` context; other transactions listed in `why`) here or in its own PR.
       * Read the benches' figures too, if they ran to the end: `files-bench` (STATS §7.6, CI's runner) and `devices-bench` (BLK.md §8's CI table). Note any that differ from the last ones by more than 1.5×, without new docs in this PR.
    2. Copy this file to `docs/MILESTONE-10.md` (`cp`), commit `docs: step 10.1 done (CI on f40f48f), the log`, push, and confirm CI green on both runs.
    3. Then PR #84 waits on review and merge, and step 10.2 (the git spike and spec/git.md) is planned in detail on the user's word.
  * Item 2 done: cce7379 pushed. **CI on cce7379 finished with no failed check** (16:28 UTC). This line reaches `docs/MILESTONE-10.md` with the next commit; copying it now would only start CI again. Item 1 (the kanban re-run, after dispatch run 43732595233's benches) is still open.
  * **The 16:44 check-in** (trig_01GZeDUAf2WWpq1hrtsdGW5j; its prompt named 2a5075b, since superseded by the pin): nothing new. CI is green on both runs of f40f48f and cce7379 has no failed check. Dispatch run 43732595233 was still running its benches (`files-bench`, `devices-bench`, about 90 minutes, started 15:56), so its `client` job can't be re-run yet. Plan mode was on, so no new check-in was set. **Next, once plan mode ends:**
    1. Re-check that run.
    2. Once it has finished, re-run its failed job once, and read the benches' figures.
    3. Set one check-in about an hour on, if the run is still going.
  * After plan mode, a check-in was set for 17:46 UTC (trig_01De6RNb2cnXDua8ZeHifPX5) for the kanban re-run.
  * **PR #84 merged** (17:21 UTC). **Step 10.1 is done.** The PR's subscription ended with it.
    * The kanban re-run no longer gates anything. The race stays open as the PR comment describes: its proposed `racing` patch goes in its own PR, or on the user's word.
    * Dispatch run 43732595233's benches may still finish. Their figures are read only if the user asks.
  * **Next, once plan mode ends:**
    1. Cancel the 17:46 check-in (`delete_trigger` trig_01De6RNb2cnXDua8ZeHifPX5); the merge leaves it nothing to do.
    2. On the user's word, restart from main:
       * `git fetch origin main`, then merge with `--ff-only` if the branch holds only merged history; otherwise restart the branch from `origin/main` (a force-with-lease push of merged history only).
       * Bring the log lines written since cce7379 into `docs/MILESTONE-10.md` (`cp`).
       * Plan step 10.2 in detail here: the git spike and `spec/git.md`.
  * **Go** (the user, after compacting): the 17:46 check-in was already cancelled. Restarted from main by fast-forward (ce0cdf3, PR #84's merge); this file copied to `docs/MILESTONE-10.md`, which brings in the lines since cce7379. Step 10.2 planned in detail below ("Step 10.2 in detail").
* **Step 10.2** (the git spike and spec/git.md):
  * Planned in detail (below), committed, a new PR opened. Next, on the user's "go": the spike (item 1), then the `spec:` commit and the log, as "Step 10.2 in detail" says.
  * Committed cd238cb and pushed; PR #85 opened and subscribed. **CI green on both runs of cd238cb** (push 43740993559 and pull_request 43741014251); PR #85 mergeable (clean). A safety-net check-in is set for 18:16 UTC (trig_01Xfk3o2VkSNh8MFecG726KL); plan mode was on, so it wasn't cancelled.
  * **On approval of this plan (it is the "go" for step 10.2):**
    1. Cancel the 18:16 check-in (CI is already green).
    2. Run the spike (item 1 of "Step 10.2 in detail"), logging each answer's commands and numbers here.
    3. Write the `spec:` commit (item 2), copy this file to `docs/MILESTONE-10.md`, push, and confirm CI green on both runs.
  * **Approved** (the user); the 18:16 check-in cancelled. **The spike** (the scratchpad's `git-spike/`, real git 2.43.0, Node 22; a directory per repo stood in for the fs, with `packs/<random id>.pack|.idx`, a `db.json` of refs and packs changed by compare-and-set under a lock, and a `pushes.log` for the emit):
    * **Item 1, the helper's model:** both prototypes of `git-remote-zen` were run through one feature matrix (`matrix.sh a|b sha1|sha256`, 25 checks, plus 4 on the hook for (b)):
      * **(b) a hidden local mirror behind `connect`: 29 of 29**, with sha1 and with sha256, with this machine's git config and with an empty one (as CI's), over protocol v2 and v0. clone (HEAD, tags, object format, `fsck --strict`), fetch, fast-forward push, non-fast-forward refused, `--force`, `--force-with-lease` (a stale lease refused), deleting a branch (seen by `fetch --prune`), `--atomic` with one non-fast-forward ref (nothing pushed), two concurrent pushes (exactly one wins; the loser's `pull --rebase` and push work), **`clone --depth 1` (shallow) and `clone --filter=blob:none` (5 blobs missing: partial)**, a push of known objects only (no pack), and a refusing hook (zen, the mirror's refs and its objects unchanged).
      * **(a) packs as files (`fetch`/`push`): 23 of 25.** Everything above but shallow and partial clones: git sends `option depth 1` and `option filter blob:none`, the helper answers `unsupported`, and **git silently fetches everything**. Force-with-lease works through `option cas`. Making them work would mean computing reachability on the client from whole packs, which is a fetch anyway.
      * **Protocol v2 over `connect`:** git gives a `connect` helper no `GIT_PROTOCOL` (its env was dumped), so `upload-pack` speaks v0 unless the helper starts it with `GIT_PROTOCOL=version=2`, which git's client then detects. Without it, `push.negotiate=true` (set in this machine's global config) fails with "--negotiate-only requires protocol v2" (a warning; the push goes on).
      * **Git sets `GIT_DIR` for the helper,** which overrides `git -C <mirror>`: every git command on the mirror runs with `GIT_DIR`, `GIT_WORK_TREE`, `GIT_INDEX_FILE`, `GIT_OBJECT_DIRECTORY` and the like removed.
      * **The mirror is one per repo URL** (a hash of it), not per remote name: a clone and an `ls-remote` of the same URL under two names made two mirrors.
      * **The choice: (b).** It keeps every feature tested, it is the local server's core too (10.5), and its pushes go through `git receive-pack`, so git's own checks (fast-forward, atomic, leases) are real ones. Its costs are below.
    * **Item 3, numbers on this repo's full history** (585 commits, 5,326 objects, 4.78 MiB as one pack; the session's checkout is shallow, so a full `--bare` clone into the scratchpad):
      * **One push per commit** (585 pushes, topo order, model (a)'s cut): 585 packs, **24.7 MB, 5.2× the one pack** (no deltas across pushes), 109 ms per push here. Every 6th commit: 98 packs, 16.4 MB; every 58th: 11 packs, 7.6 MB.
      * **Clones** (plain `git clone --bare file://` of the one pack: 0.33 s, 4 MB):

        | packs | (a) | (b) | (a) disk | (b) disk: mirror + clone |
        |---|---|---|---|---|
        | 1 | 0.40 s | 0.51 s | 4 MB | 4 + 4 MB |
        | 11 | 0.78 s | 0.68 s | 8 MB | 8 + 4 MB |
        | 98 | 1.88 s | 1.03 s | 17 MB | 17 + 4 MB |
        | 585 | 7.30 s | 1.69 s | 30 MB | 28 + 3 MB |

        (a) grows with the pack count (an `index-pack --strict` per pack); (b) copies the packs into the mirror and `upload-pack` sends the clone one pack.
      * **Memory:** the largest process in every clone is the helper's Node (47 to 58 MB peak RSS, most of it Node's own baseline); git's processes stay below it (a plain clone's largest is 12.7 MB).
      * **The mirror repacks itself:** `git repack -a -d` in the 585-pack mirror took 1.08 s and left 4 MB; push and fetch kept working, since the mirror records which zen packs it has imported by their ids, not by its own files.
      * **Content-defined chunking on packs** (a gear CDC shaped like the fs defaults, 8 KiB min, ~32 KiB average, 88 KiB max): a full repack shares **5.5%** of its bytes with the 585 small packs it replaces, and **21%** with the full repack of the same repo 10 commits earlier (with or without `-f`). So CDC (the fs default) helps a little, and rewriting the whole history on each repack is the cost to avoid.
      * **A geometric repack** (`git repack --geometric=2 -d`) rolled the 585 small packs into one (3.6 MB) in 0.94 s. Later small packs then roll up with each other, and the large pack is rewritten only when the small ones add up to its size: the policy for 10.4.
    * **Item 2, cutting a pack:** `pack-objects --revs --stdout` of `main ^main~20` held exactly the 1,071 objects `rev-list --objects` lists. A `--thin` pack is refused by `index-pack --strict --stdin` ("pack has 38 unresolved deltas"). A pack cut against an older tip is valid and only larger. **`index-pack --strict` needs the objects the pack builds on in its repository** ("did not receive expected object" in an empty one), and **git 2.43 segfaults running it outside any repository**. So a reader imports packs in their push order, or indexes them without `--strict` and checks connectivity once at the end.
    * **Item 4, the `pre-receive` hook:** it ran with the new objects in the quarantine (`objects/tmp_objdir-incoming-…`), readable by `pack-objects`, with the push's `old new ref` lines on stdin (the mirror's refs, so they are the compare-and-set's old values). A refusing hook left no refs and no objects. **The window after the hook:** with the mirror's `refs/heads/main.lock` held, zen committed the push and git then reported `failed to update ref` to the client, though the push had happened; the next sync moved the mirror to zen's tip. A `reference-transaction` hook at `prepared` holds the ref locks, but it runs once per ref unless the push is `--atomic`, which would split one push into several zen commits. So the hook stays `pre-receive`, and the helper holds the mirror exclusively for the whole `receive-pack`, which leaves only a crash or a full disk in that window.
    * **Item 5, HTTP and SSH to a mirror:** `git http-backend` behind 40 lines of Node (CGI env, `Git-Protocol` passed through) served clone, fetch (protocol v2) and push; a wrong token got 401 and a foreign `Host` 421. Over SSH, git runs `ssh -o SendEnv=GIT_PROTOCOL -p <port> git@<host> "git-upload-pack '/repo.git'"`, so protocol v2 needs the server to accept an `env` request for `GIT_PROTOCOL`. The SSH engine has no `env` request handling yet, so stock git gets v0 there, which works; 10.5 adds `env` for `GIT_PROTOCOL` only.
    * **Also checked:** `git push -o ci.skip` is refused ("the receiving end does not support push options") and `git push --signed` too ("the receiving end does not support --signed push"); `--signed=if-asked` pushes unsigned. Both are listed in git.md §12 and TD-GIT-SCOPE.
  * **`spec:` 146a687:**
    * spec/git.md:
      * §1, the three ways in
      * §2, a repo: names, a tree per repo, packs under git's names, the object format
      * §3, the `git` database: `repos`, `refs` and `packs`, `authenticated` by default
      * §4, a push as one transaction
      * §5, a fetch as a sync of the mirror
      * §6, the mirror
      * §7, `git-remote-zen`
      * §8, the local server and the relayed service
      * §9, repack and gc
      * §10, rights
      * §11, what the server sees
      * §12, what isn't supported
    * Also: the index row, four glossary terms, the label `zen/v1/sig/git-ssh-key` (members sign their own SSH keys for the relayed service), and TD-GIT-SCOPE and TD-GIT-REF-POLICY.
    * **Changes from the plan:**
      * **Packs keep git's names** (`objects/pack/pack-<hash>.pack`), not random ids. Node names are sealed in node metadata (fs.md §1), so the server never sees them, and the plan's worry about a pack hash in the clear doesn't apply.
      * **Each repo has a tree of its own,** so pushes to different repos don't serialize on one tree's header (fs.md §7). The cost, in §11: the server can tell repos apart, though not their names.
      * **`packs.seq`** orders a repo's packs, so a sync imports them oldest first for `index-pack --strict`. Repacks roll up only the newest packs (geometric, factor 2), which keeps that order.
      * **A push conflicting only on the `repos` row is retried by the hook** (two pushes to different branches); one conflicting on a ref is refused.
      * **The mirror is per user, not per clone** (`$XDG_CACHE_HOME/zen-git/<fs>/<repo>.git`), so clones on one machine share it. It is locked exclusively for a sync and a `receive-pack`, shared for an `upload-pack`, and repacks itself past 16 packs.
    * `npm run check` passes.
  * **What this changes for the later steps:**
    * **10.3:** `git-remote-zen` is model (b), `connect` to a mirror, and the hook is §4's transaction. `fetch`/`push` over packs isn't built.
    * **10.4:** the repack is §9's geometric roll-up.
    * **10.5:** the SSH engine learns an `env` request for `GIT_PROTOCOL` only (svc.md §7.2), and the HTTP side is `git http-backend` behind the server (item 5's 40 lines).
    * **10.5 and 10.6 share 10.3's mirror code.**
  * Pushed 1f478a4 (the log). **CI's `pure-rust` failed, my mistake:** `registry_matches_code` (`crates/zen-core/tests/labels.rs`) requires spec/labels.md to list exactly `labels::ALL`, and the `spec:` commit added `zen/v1/sig/git-ssh-key` to the spec only (here I had run `npm run check`, not the Rust tests). **The fix:** `labels::SIG_GIT_SSH_KEY`, in `ALL` and `SIG_PURPOSES` (the relayed service signs with it in 10.6). `cargo test -p zen-core` (the registry test included) and clippy `-D warnings` pass.
  * **CI green on both runs of 8bc64b6** (push 43744991670 and pull_request 43744997370), every job. PR #85 is mergeable (clean). **Step 10.2 is done;** PR #85 waits on review and merge. The 18:54 check-in (trig_01Cgzj3Up7qdHVmLS5mpZeLy) is still set: plan mode was on, so it wasn't cancelled.
  * **Noted from CI's logs:** the runners' git is **2.56.1** (the spike ran 2.43.0). 10.3's tests with real git run against both.
* **Step 10.3** (`packages/git` and `git-remote-zen`):
  * Planned in detail below ("Step 10.3 in detail"), in plan mode, while PR #85 waits on merge.
  * **On approval of this plan (it is the "go" for step 10.3):**
    1. Wait for the user's merge of PR #85.
    2. Restart from main by fast-forward, and cancel the 18:54 check-in if it is still pending.
    3. Copy this file to `docs/MILESTONE-10.md` and commit `docs: step 10.3 plan`.
    4. Push, open the PR, and subscribe.
    5. Implement 10.3 as "Step 10.3 in detail" says.
  * **Approved** (the user). PR #85 merged (18:24 UTC); restarted from main by fast-forward (6d4f598); the 18:54 check-in cancelled. This file copied to `docs/MILESTONE-10.md`, the plan committed, a new PR opened. Then the implementation, as "Step 10.3 in detail" says.
  * Committed 6c860b8 and pushed; PR #86 opened and subscribed. **The user asked whether to compact: yes, before the implementation.** After compacting, confirm with the user ("go") before implementing; then start with the `spec:` commit (item 1 of "Step 10.3 in detail"), copying this file to `docs/MILESTONE-10.md` first.
  * **Go** (the user, after compacting). CI on 6c860b8 finished with no failed check (18:36 UTC).
  * **`spec:` af836a9** (local, not pushed yet). git.md:
    * §1: `zen://` over HTTPS, `zen+http://` over plain HTTP; the sign-in origin is the URL's origin; the vault in `$ZEN_GIT_VAULT` or `$XDG_CONFIG_HOME/zen-git/vault`; unlocking by a device keyslot (formats.md §6), with integrity attached first.
    * §6: the lock directory `<repo>.git.lock/` with a pid, taken over from a dead pid; `upload-pack` takes no lock.
    * §7: the hook signs in again and resumes the vault's session. It is refused without the helper's environment (so a push straight into the mirror is refused), and it cuts its pack inside the quarantine.
    * §10: `zen-git setup` grants the topic rights.
    * **Changed from the plan:** §4's transaction is zen-db's retrying `db.transaction`, not a manual one. Its re-run is §4.4's retry rule: it goes on while the refs keep their old values.
  * **Not yet committed** (the working tree):
    * In spec/git.md: §7 says the helper is installed as `git-remote-zen` and `git-remote-zen+http`, since git runs `git-remote-<scheme>`. Amend it into the `spec:` commit.
    * **Fix still needed:** §6's mirror path gained the host (`zen-git/<host>/<fs>/<repo>.git`, so two servers' fs 1 don't collide). The code has it, but spec/git.md still has the old path. Write it into §6 and amend the `spec:` commit too.
  * **`packages/git`**, written (uncommitted):
    * `src/url.ts`
    * `signin.ts`: the vault, integrity, unlock by the device slot, and the `git` database (schema 1: `repos`, `refs` and `packs`, with private `by_repo` indexes)
    * `store.ts`: `readState`, `downloadPack`, and `push`, which uploads with sessions, then runs one transaction with the compare-and-set, the files, the rows and the emit. It reuses a live pack of the same hash.
    * `mirror.ts`: `cleanEnv`, `initMirror` (its config and the guarded hook script; recreated when the format or zen's tree differs), `syncMirror` (packs in `seq` order, `index-pack --strict`, the hash checked, the refs, HEAD, a local repack past 16 packs), the pid lock, and `settlePushed`
    * `hook.ts`: the pack cut in the quarantine, the push, and `forbidden` reported as `push refused`
    * `helper.ts`: `capabilities`, `option` and `connect`, with v2 unless `protocol.version` is 0 or 1
    * `setup.ts`
    * `cli.ts` (`login`, `setup`, `head`, `hook`), `main.ts` (initWasm, `zen: …` errors) and `git-remote-zen.ts`
    * Also: `scripts/build-zen-git.mjs` (two entries and a shared chunk), the root workspaces and `check`, `vitest-setup.ts`, and vitest.config's comment.
  * **Tests** (`packages/git/test/git.test.ts`), 14, all passing here with git 2.43:
    * round trips: create, clone, tags, HEAD, fsck; fetch; a push of known objects only (no pack); delete and `--prune`
    * non-fast-forward refused, `--force`, `--force-with-lease` (a stale lease refused), `--atomic`
    * two concurrent pushes to one branch (exactly one wins; the loser rebases), and to different branches (both land)
    * `--depth 1`, `--filter=blob:none`, sha256
    * an interrupted push: refs, packs, `next_seq` and the mirror unchanged; the upload sessions listed; the next push works
    * the consumer: two events with §4's body
    * carol (fs read only): clones; her push is refused (`push refused: forbidden: no write right`) with nothing changed
    * no vault: a clear error
    * a push straight into the mirror is refused
    * memory
  * **Found while testing:**
    * `zen+http` needs the bin name `git-remote-zen+http`.
    * The wasm must be initialized before `zw` is used (`main.ts`).
    * A read-only member can't create the `git` database: a writer's first push does.
    * Concurrent checkpoints of one device from several processes are handled by integrity (`version_mismatch` adopts the other's head).
  * **Negative controls,** each run once and restored (the sources diffed clean), each failing its test:
    1. no compare-and-set: the one-branch race (both won)
    2. packs imported in reverse `seq`: fresh clones of multi-pack repos (the race test's and carol's)
    3. the hook exits 0 on a refusal: carol's push succeeds
    4. no emit: the consumer times out
  * **Costs** (STATS):
    * **This repo's full history** (584 commits, 5,322 objects, a 3.5 MB pack), two runs: push 1.7–1.8 s, the hook 1.0 s of it; clone 1.0–1.2 s, against 0.33 s for a plain `git clone --bare file://`. Peak RSS: the push's helper 93–100 MB, the hook 115 MB, the clone's helper 121 MB.
    * **The synthetic 300-commit test:** push 0.78 s (the hook 0.37 s), clone 0.5 s, each process about 95 MB.
    * Node with the client's wasm is about 90 MB of that. The hook's second sign-in resumes the vault's session.
  * Plan mode came on mid-step (a worker restart) and was left with the log above. Then:
    * **`spec:` c4a41e3:** af836a9 amended with §7's two helper names and §6's mirror path with the host.
    * **`git:` a0fdffc:** packages/git, the tests, the build script and the workspace entries. `npm run check` passes; `npx vitest run packages/git` 3× green (14 of 14 each time).
  * **`docs:`:**
    * docs/GIT.md, begun: setting up (keyslots, `zen-git setup`, `login`), using it, the mirror, costs, what isn't supported, troubleshooting
    * spec/OPS.md: §1.3's `git` row and a new §9, git pitfalls
    * STATS §5.8: the costs above
    * README: the package's row
    * this log
  * Next: push, and confirm CI is green on both runs (git 2.56.1 there).
  * Pushed 85a6180 (`docs:`).
    * **CI green on both runs of 85a6180,** every job: push 43757240535 and pull_request 43757244816.
    * The pull_request run's `client` job ran on git 2.56.1: 76 test files passed and 8 skipped (the opt-in benches), 712 tests.
    * PR #86 is mergeable (clean).
    * **Step 10.3 is done;** PR #86 waits on review and merge.
    * This line reaches `docs/MILESTONE-10.md` with the next step's restart from main; plan mode was on when CI finished.
  * **Next, on the user's word:**
    1. Restart from main once PR #86 is merged, bringing this line into the docs copy.
    2. Plan step 10.4 in detail here: the repack leader job, its gc, and the dumb-HTTP public mirror.
  * **PR #86 merged** (the user, 21:39 UTC). **Step 10.3 is done.** The PR's subscription ended with it. The user asked whether to compact: yes.
  * **After compacting, on the user's "go":**
    1. Restart from main by fast-forward.
    2. Copy this file to `docs/MILESTONE-10.md`, which brings in the lines since 85a6180.
    3. Plan step 10.4 in detail here (in plan mode), commit the plan, open a new PR, and subscribe.
* **Step 10.4** (repack, gc and the public mirror):
  * **Go** (the user, after compacting). Restarted from main by fast-forward (9aa22ef, PR #86's merge); this file copied to `docs/MILESTONE-10.md`, which brings in the lines since 85a6180. Step 10.4 planned in detail below ("Step 10.4 in detail"), in plan mode.
  * **On approval of this plan:**
    1. Copy this file to `docs/MILESTONE-10.md` and commit `docs: step 10.4 plan`.
    2. Push, open the PR, and subscribe.
    3. Stop there: implementation starts on the user's next "go".
  * **Approved** (the user). This file copied to `docs/MILESTONE-10.md`; the plan committed, pushed, a new PR opened and subscribed. Next, on the user's "go": the `spec:` commit (item 1 of "Step 10.4 in detail"), then the code, the tests and the docs.
  * Committed fec5631 and pushed. PR #87 is open and subscribed.
    * **CI green on both runs of fec5631:** push 43765972554 and pull_request 43765994079.
    * PR #87 is mergeable (clean).
    * A safety-net check-in is set for 22:42 UTC (trig_01UvokdwekTzLSkSP32Z7Puf). Plan mode was on when CI finished, so it hasn't been cancelled.
  * **Once plan mode ends:**
    1. Cancel the 22:42 check-in, since CI is already green.
    2. On the user's "go", start with the `spec:` commit, copying this file to `docs/MILESTONE-10.md` first.
  * The 22:42 check-in was cancelled.
  * **PR #87 merged** (the user, 22:03 UTC). It held only the plan, so step 10.4's implementation needs a new PR. The PR's subscription ended with the merge.
  * **On the user's "go":**
    1. Restart from main by fast-forward.
    2. Copy this file to `docs/MILESTONE-10.md`.
    3. Write the `spec:` commit.
    4. Push, open a new PR, and subscribe.
    5. Then the code, the tests and the docs, as "Step 10.4 in detail" says.
  * **Go** (the user, after compacting). Restarted from main by fast-forward (e022f1c, PR #87's merge); this file copied to `docs/MILESTONE-10.md`.
  * **`spec:` 9aa3a73** (git.md §3–§5, §9–§13, TD-GIT-SCOPE): one repack topic per fs, the geometric roll-up from the rolled-up packs alone, a push skipping known packs, a covered roll-up, `PackGone`, the scheduler's grant, §13's public mirror. Pushed; **PR #88** opened and subscribed.
  * **`git:` fdad63e:** `zen-git worker`, `repack` and `publish`, `gitcmd.ts`, `publish.ts`, `worker.ts`; schema 1's `packs.of` and index `live`; the test world shared (`test/world.ts`); `repack.test.ts` (10 tests). **Found by the tests:** a purged node reads as an empty stream, not an error (the download now checks its byte count); `git repack` writes the dumb-HTTP files itself unless `-n` (so negative control 5 first passed; `publish` now passes `-n`).
  * **The bench found a stall** (2,000 pushes in a row, a worker beside them): in about half the runs the live packs grew to 1,600–1,984, then one repack took 50–98 s. Traced over several runs (the consumer's lease, `next` and ack calls; each handler's start; the group's cursor; the topic's order):
    1. The server was fine: `next` returns the first event after the cursor, and the cursor moves one event at a time.
    2. The repack handler started again and again without finishing: zen-db's consumer reruns a handler that throws `conflict`, without reporting it.
    3. The conflict: every writer of an authenticated database conflicts with every other on its NsRoot (zendb.md §14.5), and commits with tree operations serialize at the tree's header (fs.md §7). The repack's commit read each rolled-up row with a `get` of its own, so once behind by hundreds of packs it never landed between pushes.
    * **Fixed** (`spec:` 28ea8d4 and 4fe1607, `git:` 497f387 and c75b8a9): a repack is short commits, each retried alone: the files with a `pending` row and its gc; one range read that makes the new pack live; the old rows marked replaced 16 per commit, each with its gc. The gc deletes 16 per commit. The worker also renews its lease every 3 s while it works (the client renews only between deliveries), and logs handler errors and retries under `ZEN_GIT_DEBUG` (8c632a8).
    * **After the fix, four runs kept up:** at most 23–44 live packs, 129–131 repacks, the longest 1.0–2.8 s, the worker 188–230 MiB. STATS §5.8 has the figures.
  * **Negative controls,** each run once and restored (the sources diffed clean), each failing its test: (1) the live check removed: the two-repacks test (rewritten so the two runs differ, after the first fix made it pass through the files' commit); (2) the roll-up cover skipped: the cover test downloads them; (3) the `PackGone` retry removed: the stale sync; (4) no `repack` emit: the worker test times out; (5) `publish` without `update-server-info`: the dumb clone fails.
  * **CI:** the `client` job of 497f387 failed in `apps/echo/e2e/echo.spec.ts`: a port that `freePort()` picked was taken before the server bound it (two Playwright workers). Not this PR's code; one re-run passed; a proposed harness patch is in a PR comment, for its own PR. CI on c75b8a9 and 4fe1607 finished with no failed check.
  * **`docs:` 3438c75:** STATS §5.8's repack bench, GIT.md's worker costs and troubleshooting, this log.
  * **Verified here:** `npm run check`; `npx vitest run packages/git` 3× green (24 of 24 each, git 2.43); `packages/db` once (222 passed, 7 skipped); the bench as above. Next: CI green on both runs of the next head; then PR #88 waits on review and merge, and step 10.5 (`zen-git serve`) is planned on the user's word.

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

## Step 10.2 in detail: the git spike and spec/git.md

No product code. The step answers the design questions with real git, then writes `spec/git.md`, which steps 10.3 to 10.6 implement. The spike's scripts and repos live in the scratchpad only; what they found goes into this log, and what they decided goes into the spec.

**Survey (this session):**
* **The placeholder's 175 MiB limit is gone:** TD-FS-LARGE-FILES is resolved (paged versions, about 640 GiB at the defaults; fs.md §4, §4.1), so large packs need no work of their own beyond a streaming upload.
* **Files in a transaction** (zendb.md §7.6): the chunks go up in earlier commits (an upload session, fs.md §4.1, keeps them past the grace), and the transaction carries only the `write`. So a push's pack can be uploaded first and become visible only with its refs.
* **Integrity** (zendb.md §14): in a `basic` database the server can serve an older value or leave rows out of a range read; an `authenticated` database detects both, up to the checkpoint window (§14.7). Git's objects check themselves by their ids, so the refs are what the server could roll back.
* **Topic paths** (zendb.md §2.3): apps choose every path outside `("zen", …)`, so `("git", repo, "pushes")` is free.
* **Tools here:** git 2.43.0, with `git-http-backend` and the rest of git-core.

### 1. The spike (the scratchpad, real git 2.43; each answer logged with its numbers)

1. **The remote-helper model.** Two candidates, each built as a small Node prototype of `git-remote-zen` over a plain directory standing in for the fs:
   * **(a) Packs as files** (the `fetch`/`push` capabilities, as git-remote-gcrypt does): the helper lists refs, downloads the packs it lacks into the local repo and indexes them; on push it cuts a pack (item 2) and writes it with the refs.
   * **(b) A hidden local mirror** (the `connect` capability): the helper syncs a bare mirror from the packs and connects git to a real `git upload-pack`/`git receive-pack` on it; a `pre-receive` hook does the zen push (item 4). The local server (10.5) needs this mirror anyway, so (b) would give the helper and the server one core.

   For each, checked with real git: `clone`, `fetch`, `push`, `push --force`, `push --force-with-lease`, deleting a branch, tags (light and annotated), HEAD and its default branch, `push --atomic`, `fetch --depth 1`, `clone --filter=blob:none`, and a repo with `--object-format=sha256`. Logged: which work, which fail and how (a clear error, or a silent full fetch), and the memory (RSS) and disk each uses on this repo's clone.

   **The choice:** the model that keeps more of these working at a cost within OPS.md §6's memory rule. The default expected is (b), unless the mirror's disk or its plaintext copy weighs against it; then (a), with the unsupported features listed.
2. **Cutting a pack** for a push: `git pack-objects --revs --stdout` given the new tips and `^` each tip the remote has, never `--thin` (a stored pack must stand alone), then `git index-pack --strict` (which also writes the `.idx`). Checked: the pack holds exactly the missing objects (`git verify-pack -v`), a thin pack is refused by `index-pack` without `--fix-thin`, and a pack built against stale remote tips still applies (it only carries extra objects).
3. **Numbers on a real history** (this repo, replayed): one push per commit (the worst case of packs), then measured:
   * the pack count and total size against one `git repack -a -d` pack
   * the time to clone from N packs (N = 10, 100, 1,000) against one pack
   * how CDC chunking (fs.md §4.1) dedups packs and their repack, which decides the fs policy for repos: CDC or `fixed`

   These set the manifest's design and the repack thresholds in 10.4 (a count of packs, or the small packs' share of the bytes).
4. **A `pre-receive` hook doing the zen transaction** inside `git receive-pack`: the new objects are in the quarantine (`GIT_QUARANTINE_PATH`), readable by the hook. Checked:
   * the hook can cut the pack from the quarantine (or take the pack `receive-pack` kept, with `receive.unpackLimit` 1 and `--fix-thin`'s completed pack)
   * a hook that exits non-zero leaves nothing of the push behind: no refs, no objects
   * the refs arrive on its stdin as `old new ref` lines, the push's old values, so the hook's compare-and-set uses them
   * the window after the hook succeeds and before git updates its own refs, and what fails there (a locked ref): the mirror is then behind zen, and the next sync repairs it
   * `--atomic` gives one hook call for all refs either way
5. **Smart HTTP and SSH to the mirror** (for 10.5's choices): `git http-backend` behind a small Node server (CGI env), and `git upload-pack`/`git receive-pack` as SSH exec commands; checked with `git clone http://127.0.0.1:PORT/…` and against apps/echo's SSH stub. Only that they work and what each needs; 10.5 plans the rest.

### 2. `spec:` commit (spec/git.md, the index, glossary, labels, TECH_DEBT)

**spec/git.md**, its sections:
1. **Scope and the three ways in:** `git-remote-zen` (`zen://<host>/<fs>/<repo>`), the local server (`ssh://localhost`, `http://127.0.0.1`), the relayed service (`ssh://git@<host>`); what each trusts.
2. **A repo in an fs:** the directory `/<repo>.git/`; `packs/<id>.pack` and `.idx`, each written once and never changed; `<id>` is random (16 bytes, hex), and git's pack hash stays in the sealed `git_packs` row; the fs's chunking policy (from item 3); the repo's object format (sha1 or sha256), fixed at creation.
3. **The database:** one zen-db database per fs (its name derived from the fs, labels.md), with:
   * `git_repos`: repo → object format, HEAD (a symbolic ref), created
   * `git_refs`: (repo, ref) → oid, or a symbolic target; the pusher's device; the push's time
   * `git_packs`: (repo, pack id) → git's pack hash, the pack's node, its size and object count, whether it is live or replaced (by a repack), when it was replaced
   * the database is `authenticated` (zendb.md §14) by default, so a server serving old refs is detected; a `basic` one is allowed and the leakage section says what that loses
4. **A push:**
   * the pack cut as item 2 says (or none, for a push of known objects or of deletes only)
   * its chunks uploaded first, under an upload session
   * one manual transaction: read the refs it updates (their old oids); refuse if one differs, unless forced; write the pack's file and its `git_packs` row; put the refs; emit to `("git", repo, "pushes")`
   * a conflict from a concurrent push: the client re-reads and tells git the push was refused (a non-fast-forward), as a stock server would
   * the event's body: pusher, the updates (ref, old, new), the pack id
   * non-fast-forward checks are the pusher's (cooperative), since the server can't read commits; the local server and the handler check them as `receive-pack` does
5. **A fetch:** read the refs and the live packs at one read version; download the packs the client lacks, check each against its `.idx` (`index-pack --strict`), and only then update local refs.
6. **Repack and gc:** a leader job (10.4) writes one combined pack, then one transaction marks the old packs replaced and the new one live, conditioned on the pack list it read; a replaced pack is deleted after a grace (default 24 h) so fetches in flight finish; the thresholds from item 3.
7. **Rights:** the fs grants: `read` to fetch, `write` to push; a repo with its own rights lives in an fs of its own; branch protection and force-push rules are cooperative, enforced only by the local server and the handlers, and the spec says so. The topic's grants for consumers of the push events.
8. **The local server and the relayed service:** their sign-in (the vault as the device), the allowed SSH keys, the HTTP token (bound to 127.0.0.1, `Host` checked), the mirror, the hook doing §4's transaction, and what each machine holds in plaintext.
9. **Leakage:** what the server sees: pack sizes and counts, push times, which device pushed, the number and sizes of ref rows (padded as zendb.md §4.3), the topic's events; never names, refs, oids or contents. What a relayed handler sees: everything it serves.
10. **What isn't supported** (from item 1): e.g. shallow and partial clones, if the chosen model can't do them, and how git reports it.

**Also:** spec/README.md's table gains git.md; glossary.md (pack, ref, remote helper, mirror); labels.md (the database name's label, if one is derived); TECH_DEBT: `TD-GIT-SCOPE` (what §10 leaves out), and `TD-GIT-REF-POLICY` (branch protection is cooperative).

### Verification

* Each spike answer's commands and numbers are in this log (the scratchpad may be cleared, so the log is what lasts).
* The spec read against the code it relies on: `db.begin`, `tx.fs(tree).write`, `tx.emit`, upload sessions, the integrity levels, the leader jobs (the survey's citations).
* `npm run check` (the docs only change markdown, so no build); CI green on both runs.

**Defaults chosen (overridable):**
* **The helper's model is the spike's to decide** by the criteria in item 1, (b) expected.
* **One database per fs** for every repo in it, `authenticated` by default.
* **A pack's file is named by a random 16-byte id**, not by git's pack hash: that hash names the pack's contents, so in the clear it would let anyone holding the same pack confirm the repo holds it. Git's hash is kept in the sealed `git_packs` row.

## Step 10.3 in detail: `packages/git` and `git-remote-zen`

`@zen/git`, a Node package with two bins: `git-remote-zen` (the remote helper, git.md §7) and `zen-git` (`login`, `setup`, `head`, and the internal `hook`). It implements git.md §2–§7, the mirror core that 10.5 and 10.6 reuse. Nothing in the server changes.

**Survey (this session; file:line from the survey):**
* **Bins:**
  * `@zen/svc` declares `bin: {"zen-connect": "./dist/zen-connect.mjs"}`.
  * `scripts/build-zen-connect.mjs` bundles it with esbuild: ESM, node22, the `@zen/client` aliases, `external: ['node:*', '@zen/wasm']`, then chmod 755.
  * `scripts/vitest-setup.ts:12` builds it once per run.
  * A new package is also listed in the root workspaces and `check`, and in `vitest.config.ts`'s aliases.
* **Signing in without a prompt:**
  * Build the vault as `credentialVault(sealedStore(nodeDiskLocalStore(dir)))`.
  * First run: `vault.saveDevice({identity, deviceSeed, cert})` from a device file. Later runs: `vault.signIn(client)` (`packages/fuse/src/cli.ts:235-250`).
  * `vault.unlock(fs)` needs a **device keyslot** on the fs (`packages/client/src/local/vault.ts:75`). `startWorld` makes only a passphrase slot, so the tests add a device slot.
  * Attach integrity before unlocking: `session.fs(n).integrity({device, local})`, then `Db.open(fs, ns, {integrity: 'authenticated'})` (`apps/shared/node.ts:104-111`).
* **The push's pieces:**
  * `db.begin()` gives a manual transaction (`packages/db/src/db.ts:710`), with `commit()`/`abort()` (`txn.ts:1105`).
  * `tx.fs(tree).ops(batch)` and `.write(upload, {replaces: []})` (`packages/db/src/files.ts:19,29`). `tree.batch().create(...)` and `.mkdir(...)` (`tree.ts:1676`).
  * `tree.upload(node, src, {session: true})` and `tree.closeUpload` (`tree.ts:1224,1350`). The closest existing pattern is `packages/loro/src/loro.ts:660-697`: create the node in a batch, upload with a session when `db.features` has `upload_sessions`, then one transaction with ops, write and put, and `closeUpload` in `finally`.
  * `tx.emit(db.topic('git', repo, 'pushes'), type, body)` (`txn.ts:208`).
  * Tables come from `m.createTable(name, {pk, pad: true})` and `m.createIndex(t, name, {fields})` (private by default). Index reads go through `tx.table(t).query().where(f, '=', v).all()` (`packages/db/src/query.ts:294`).
* **Reading a pack:** `tree.readStream(node)` (`tree.ts:785`), streamed to a temporary file.
* **Topic grants:** `session.acl.update(admin, doc => doc.grants.push({fs, topic: fs.topic('git').id, rights, subject}))` (`apps/shared/harness.ts:155-170`). A grant on the prefix `('git')` covers every repo's topic.
* **Consumers:** `db.consume({group, topic, mode: 'sequential'}, (msg, tx) => …)` (`db.ts:841`).
* **Tests:**
  * `startWorld` (`apps/shared/harness.ts:127`, `until` at :362).
  * Spawning a bundled CLI: `apps/echo/test/connect.test.ts:44-104`.
  * A vault CLI's environment, with every `ZEN_*` variable removed: `packages/fuse/test/mount.test.ts:446-520`.

### 1. `spec:` commit (git.md corrections the survey found)

* **§1, URLs:**
  * `zen://<host>[:port]/<fs>/<repo>` reaches the server over HTTPS. `zen+http://` uses plain HTTP, for a server on localhost and for tests.
  * The vault directory is `$ZEN_GIT_VAULT`, or `$XDG_CONFIG_HOME/zen-git/vault` (`~/.config` by default).
* **§10, the topic grants:** a first push can't change the ACL, since only an admin can. So an admin grants them once per fs, with `zen-git setup <fs>`:
  * `append` on the prefix `("git")` to the fs's writers
  * `read` and `consume` on it to its readers
* **§6, locking:** Node has no portable `flock`, and git's readers tolerate concurrent ref updates, new packs, and `repack -a -d`.
  * So a sync and a `receive-pack` hold an exclusive lock: a lock directory holding the holder's pid, taken over when that pid is gone.
  * `upload-pack` takes no lock.
* **§7, the hook:**
  * It runs in a process of its own (`receive-pack` starts it), so it signs in again from the same vault.
  * Its environment names the repo URL and the mirror.
  * The cost of the second sign-in is measured, and is in STATS.

### 2. `git:` commit (`packages/git`)

* `src/url.ts`: parse and check `zen://` and `zen+http://` URLs (git.md §1, §2.1).
* `src/signin.ts`:
  * the vault from `$ZEN_GIT_VAULT`
  * `signIn(url)`: the client, sign-in, integrity with the vault's pins, unlock by the device slot
  * `openDb(fs)`: `Db.open(fs, 'git', {integrity: 'authenticated', schema: 1, migrations})` with git.md §3's tables
  * a `basic` database when the fs has no integrity, as §3 allows
* `src/store.ts`, the zen side:
  * `readState(db, repo)`: one read transaction (§5.1)
  * `downloadPack(tree, row, dir)`
  * `push(db, fs, {repo, updates, packPath, idxPath, hash, format})`: §4 steps 2–4, with the retry rule (§4.4), and `closeUpload` in `finally`
* `src/mirror.ts`:
  * the path `$XDG_CACHE_HOME/zen-git/<fs>/<repo>.git`
  * `init` with §6's config and the hook script, which runs `process.execPath` on `zen-git.mjs hook`
  * the lock
  * `git(args)` with git's variables removed (§6)
  * `sync(state)` (§5: packs in `seq` order with `index-pack --strict`, the imported list, refs, HEAD)
  * the local repack past 16 packs
* `src/hook.ts`:
  * read the `old new ref` lines
  * cut the pack (§4.1)
  * sign in, push
  * on refusal, exit non-zero with a message git shows as `remote: zen: …`
* `src/helper.ts` (the `git-remote-zen` bin), git.md §7:
  * `capabilities`, `option`, `connect`, reading stdin one byte at a time
  * `GIT_PROTOCOL=version=2` for `upload-pack` unless `protocol.version` is 0 or 1
* `src/cli.ts` (the `zen-git` bin):
  * `login --server URL --device FILE`: saves the device in the vault
  * `setup <fs>`: the admin's topic grants
  * `head <url> <branch>`
  * `hook`: internal
* `scripts/build-zen-git.mjs` (as `build-zen-connect.mjs`), the workspace entries, `vitest-setup.ts`.

### Tests (`packages/git/test/`, a real server through `startWorld`, real git, the bundled bins on `PATH`)

Each test gives git a `GIT_CONFIG_GLOBAL` of its own, and the child processes get no `ZEN_*` variables but the vault's.
* **Round trips** (git.md §4, §5):
  * the first push creates the repo (its `repos` row, and HEAD at the first branch); clone it back, `fsck --strict`
  * fetch after another clone's push
  * tags, light and annotated
  * deleting a branch, seen by `fetch --prune`
  * `--force` and `--force-with-lease`, including a stale lease refused
  * `--atomic` with one non-fast-forward ref: nothing pushed
* **Concurrency:**
  * two pushes to one branch at once: exactly one wins, and the loser's `pull --rebase` and push work
  * two pushes to different branches at once: both land (the retry rule)
* **Features:** `clone --depth 1`, `clone --filter=blob:none`, and a sha256 repo.
* **An interrupted push:** the hook killed after its upload, before the transaction (a test-only variable). Then no refs and no `packs` row changed, the upload session is listed, and the next push succeeds.
* **The emit:** a consumer of `("git", repo, "pushes")` sees one event per push, with §4's body.
* **Rights:** a member with fs `read` only can clone. Their push is refused with nothing visible: zen unchanged, and their mirror's refs unchanged.
* **Sign-in:** a helper with no vault fails with a clear message, not a prompt.
* **Memory:** the helper's and the hook's peak RSS during a clone and a push of this repo's history (585 commits), logged for STATS.
* **Negative controls,** each run once and restored, each failing its test:
  1. the hook skips the refs' compare-and-set (the concurrent-push test)
  2. the sync imports packs in reverse `seq` (`index-pack --strict` refuses: the round trip)
  3. the hook exits 0 when the transaction fails (the read-only member's test)
  4. the emit left out of the transaction (the consumer test)

### Docs (`docs:` commit)

* docs/GIT.md, begun:
  * setting up (`zen-git setup`, `login`)
  * `git clone zen://…`
  * the mirror, and that it is plaintext
  * what isn't supported (git.md §12)
* spec/OPS.md, the pitfalls:
  * git's `GIT_DIR` for helpers
  * `index-pack --strict` outside a repository segfaults in 2.43
  * `push.negotiate` needs protocol v2
* STATS: the helper's and the hook's memory and the sign-in cost
* README's package list, and the log

### Verification

* `npm run check`; `npx vitest run packages/git` 3× green; the touched suites (`packages/db`, `packages/client`) once.
* With git 2.43 here, and with CI's 2.56.1 on both runs.
* CI green on both runs.

## Step 10.4 in detail: repack, gc and the public mirror

`@zen/git` gains three commands: `zen-git worker` (a long-running repack and gc job for one fs), `zen-git repack` (a one-shot roll-up of one repo) and `zen-git publish` (a dumb-HTTP copy of a repo, for `/unencrypted`). The push and the sync change a little (below). Nothing in the server changes.

**Survey (this session):**
* **git.md §9 as written:** past 16 live packs, a consumer of a `sequential` group `"repack"` on each repo's `pushes` topic rolls up the newest packs geometrically (factor 2). Its transaction checks the rolled-up rows are still `live`, marks them `replaced` (`replaced_ms`, `by`), and puts the new pack's row with `seq` = the highest rolled-up `seq`. Gc is a delayed message at `replaced_ms` + 24 h.
* **Consumers:**
  * `db.consume(opts, (msg, tx) => …)` runs the handler inside the delivery's transaction (short by default).
  * `db.consumeManual(opts, msg => …)` lets the handler do slow work first, then `msg.ack()` or a `db.begin({consumes: msg})` it commits (`packages/db/src/db.ts:856`). A throw nacks, and the message is redelivered.
  * `ConsumeOptions` (`packages/db/src/broker.ts:456`): `sequential` hands out events under a lease that is renewed automatically, so a long repack keeps it.
* **The scheduler** (`packages/db/src/sched.ts`):
  * It runs in the owner unless `DbOptions.scheduler` is false; the helper's `openGitDb` sets false.
  * It leads through `topic('zen', 'db', ns, 'sched').leader('sched')`.
  * `tx.schedule(topic, type, body, {delayMs})` adds a `$sched` row (`txn.ts:231`).
  * So the worker needs rights on `("zen", "db", "git", "sched")`, which `zen-git setup` doesn't grant yet.
* **Deleting files:** `tree.batch().remove(node)` moves a node to `TRASH` (`packages/client/src/tree.ts:1715`), and the sweeper purges it after the horizon (fs.md §6). `tx.fs(tree).ops(batch)` carries it in a transaction (`packages/db/src/files.ts:29`).
* **Compound indexes:** `planRange` (`packages/db/src/query.ts:169`) uses an index whose leading fields are bound by `=`, ordered by the next field. So an index `[repo, state, seq]` reads only a repo's live packs, in `seq` order.
* **`/unencrypted`** (`crates/zen-server/src/statics.rs:587`):
  * It serves `GET /unencrypted/<path>` by the URL's path, ignoring the query (git's `info/refs?service=…`), and a missing file is a real 404.
  * Its source is `unencrypted_dir` or `static push`'s generation (operations.md §9); a push replaces the whole set.
  * The test harness's server takes `server.top.unencrypted_dir` (`apps/shared/harness.ts:132`).

### 1. `spec:` commit (git.md §3, §4, §5, §9, a new §13, §10, §11)

* **§9, one repack topic per fs:** `("git", "repack")`, `sequential` group `"repack"`, not each repo's `pushes`.
  * One consumer then serves every repo of the fs, including repos created later. A group per repo would need a consumer, and a long-poll, per repo.
  * **§4 step 3.5:** a push whose pack makes the repo's live packs more than 16 also emits `repack` `{1: repo}` there, in the same transaction. The count is a range read of the new index (17 rows or so).
* **§9, the roll-up's pack:**
  * It is built in a temporary repository holding only the rolled-up packs (`index-pack` without `--strict`, since their bases' older objects aren't there).
  * Then `pack-objects --stdout` of every object in it (`cat-file --batch-all-objects`), never thin.
  * Its object count must equal the union of the rolled-up packs'.
  * So the job needs no mirror, and it downloads only the newest packs (the geometric rule keeps them small, except when the roll-up reaches the oldest).
* **§9, choosing the run:** at least the newest two packs, and the shortest such run that leaves every older pack at least twice the bytes of all the packs after it.
* **§9, its transaction:** as written, plus the new row's `of` (the rolled-up hashes) and a `gc` message scheduled in the same transaction (`tx.schedule`, the grace: 24 h by default) to `("git", "repack")`, carrying `{1: repo, 2: [hash], 3: [node]}`.
* **§9, gc:** the job moves the listed nodes to `TRASH`, purged by the sweeper (fs.md §6), and deletes the rows still `replaced`.
* **§3:**
  * `packs` gains `of: [text]` (absent for a pushed pack).
  * Its index `by_repo` becomes `live` on `[repo, state, seq]`.
* **§4 step 3.3:** the pack is skipped when its hash has a row, `live` or `replaced`. A replaced pack's objects are in a live pack, so a re-push of the same pack bytes adds nothing, and no two nodes get one name.
* **§5:**
  * A live pack whose `of` hashes are all imported is marked imported without a download, since the mirror already has its objects. So a mirror that kept up never downloads a roll-up.
  * A pack that has gone (gc after a very old listing) makes the helper read the state again and sync once more.
* **New §13, the public mirror:**
  * `zen-git publish <url> <dir>` writes `<dir>/<repo>.git` for git's dumb HTTP protocol: one pack (`repack -a -d`), `info/refs` and `objects/info/packs` (`update-server-info`), and `HEAD`.
  * It is built in a temporary directory and swapped in by a rename. The previous copy is removed after the swap.
  * It is served by `/unencrypted` (an `unencrypted_dir`, or the site that `static push` uploads).
  * It is plaintext and public by design: the repo's whole history, its refs and its names.
  * Dumb HTTP has no shallow or partial clones, and it is read only.
  * `zen-git worker --publish <repo>=<dir>` publishes again after each push: a `consumeManual` group `"publish"` on that repo's `pushes` topic.
* **§10:**
  * The worker needs fs `write`.
  * `zen-git setup` also grants writers `read`, `append` and `consume` on `("zen", "db", "git", "sched")`, the scheduler's lease topic (the exact rights are confirmed against `Leader` in the code).
* **§11:**
  * The server sees each repack: a new pack's size, the replaced rows, and the `repack` topic's events and their times.
  * It doesn't see the public mirror's contents unless the mirror is published; once published, they are public.
* **The pack manifest the placeholder named is not built:** the `packs` table is that manifest, one range read on `live`. §3 says so.

### 2. `git:` commit (`packages/git`)

* **`signin.ts`:**
  * Schema 1 changed in place (no deployed data): `packs.of`, and the index `live` `[repo text, state text, seq int]`.
  * `openGitDb(…, {scheduler})`, true for the worker only.
* **`store.ts`:**
  * `readState` reads through `live`.
  * `push`: skips a known hash (§4 3.3), and counts the live packs and emits `repack` (§4 3.5).
  * `repackTopic(db)`.
  * `rollUp(packs): PackRow[] | undefined`, a pure function (§9's run).
  * `repack(db, fs, repo, {graceMs})`, which in order:
    1. reads the state and chooses the run
    2. downloads the run's packs into a temporary bare repo
    3. builds and checks the new pack
    4. uploads it under a session
    5. runs the transaction (the rows still `live`, else `{ok: false}`; mark them `replaced`; put the new row; `tx.schedule` the gc)
    6. closes the upload, and removes the temporary repo in `finally`

    A test variable makes it SIGKILL itself after the upload, as the hook's does.
  * `collect(db, fs, body)`, the gc.
* **`mirror.ts`:** `syncMirror` marks a pack covered by `of`. A pack that is missing on download throws a typed `PackGone`, which the helper and the hook's callers catch once to re-read the state.
* **`worker.ts`, `zen-git worker <zen://host/fs> [--grace 24h] [--publish <repo>=<dir>]…`:**
  * signs in, opens the db with the scheduler, and runs `consumeManual` on `repackTopic`
  * `repack` messages: `repack` when the live packs still exceed 16, else ack
  * `gc` messages: `collect`
  * the publish groups
  * stops cleanly on SIGINT and SIGTERM, releasing its leases
  * logs one line per repack (`repo: N packs → 1, X MB, T s`) to stderr
* **`publish.ts`:** the sync under the mirror lock, then §13's directory.
* **`cli.ts`:** `worker`, `repack <url>` (one-shot) and `publish <url> <dir>`.
* **`setup.ts`:** the scheduler's grant.

### Tests (`packages/git/test/`, the same world as 10.3's, real git and the bins)

* **Unit:** `rollUp` on hand-made size lists: nothing at 16 or fewer; the newest two at least; the shortest run; a run reaching the oldest pack.
* **Many pushes, repacked:**
  * 300 pushes through `store.push` (packs cut by `git pack-objects` from a local repo, so the test takes seconds, not 300 helper runs), with a `zen-git worker --grace 3s` process running.
  * Then:
    * the live packs settle at 16 or fewer
    * a fresh `git clone` passes `fsck --strict`, with every ref
    * a clone made before the repacks fetches without downloading any roll-up (no `pack-<roll-up hash>.pack` in its mirror)
    * after the grace, the replaced rows and their nodes are gone
* **A fetch during a repack:**
  * A state read before a repack still syncs a fresh mirror after the repack, within the grace.
  * After the gc, a sync from that stale state hits `PackGone`, re-reads, and succeeds.
* **Concurrency:**
  * A push during a repack: both commit (the repack reads only the rows it replaces).
  * Two `zen-git repack` at once: one commits, the other is refused (`the packs changed`). Nothing is lost, and `fsck` passes.
* **A worker killed after its upload:** nothing changes. The unacked message is redelivered to the next worker, which repacks.
* **A re-pushed pack after a repack:** no new row, the refs move, and a clone works.
* **The public mirror** (the world's server with `unencrypted_dir` set to a temporary directory):
  * `zen-git publish`, then `git clone http://127.0.0.1:<port>/unencrypted/<…>/<repo>.git` (dumb HTTP): refs, tags, HEAD, `fsck --strict`.
  * After a push, `worker --publish` brings the copy up to date, and a `git fetch` from it gets the new commit.
* **Setup:** `zen-git setup` adds the scheduler's grant, and running it twice adds nothing.
* **Bench (opt-in, `ZEN_GIT_BENCH_PUSHES`):** 2,000 pushes with a worker. Logged:
  * the repack count and each one's time
  * the live packs over time and `readState`'s time
  * the roll-ups' total bytes against one `git repack -a -d` of the same history
  * the worker's peak RSS
* **Negative controls,** each run once and restored, each failing its test:
  1. the repack transaction not checking the rows are still `live`: the two-repacks test
  2. the `of` cover skipped: the old clone downloads a roll-up
  3. the helper's `PackGone` retry removed: the stale sync after gc
  4. the push not emitting `repack`: the live packs never settle
  5. `publish` without `update-server-info`: the dumb clone fails

### Docs (`docs:` commit)

* **docs/GIT.md:**
  * running the worker: one per fs, on an always-on machine; a systemd user unit; what happens without one (packs pile up, so fresh clones slow down; the mirrors repack themselves)
  * `repack`
  * publishing a public repo (with an `unencrypted_dir`, or with `static push`, which replaces the whole site)
  * setup's new grant
* **STATS §5.8:** the repack and gc costs, and the bench's figures.
* **spec/OPS.md §9:**
  * a `static push` of a published repo replaces the whole site
  * dumb HTTP is read only
* **TECH_DEBT:** TD-GIT-SCOPE gains dumb HTTP's limits.
* **The log.**

### Verification

* `npm run check`.
* `npx vitest run packages/git` 3× green, with git 2.43 here; `packages/db` once.
* CI green on both runs (git 2.56.1 there).
* The bench run once here, its figures logged.

**Defaults chosen (overridable):**
* **One `("git", "repack")` topic per fs,** not a group on each repo's `pushes` topic (a spec change to §9).
* **The roll-up is built from the rolled-up packs alone** in a temporary repo, so it needs no mirror. If its packs are much larger than `git repack`'s (measured against the spike's 3.6 MB), pack-objects is given paths from `rev-list --objects` for its delta heuristics, and the log says so.
* **A push skips any known pack hash,** live or replaced.
* **The worker runs the scheduler** (gc by delayed messages, as §9 says), and the grace is 24 h.
* **`publish` writes one full pack each time,** not incrementally.

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
