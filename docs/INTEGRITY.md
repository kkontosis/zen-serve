# Integrity

A guide to what zen-serve's integrity checks catch, how to turn them on, and what to do when they fire. It's for app developers and operators. The API is in [CLIENT.md §12](CLIENT.md#12-integrity) (TypeScript) and [§13](CLIENT.md#13-the-rust-client) (Rust). The objects have their own sections: [DB.md §7.1](DB.md#71-authenticated-databases), [SVC.md §6](SVC.md#6-what-the-server-sees) and [BLK.md](BLK.md). The normative text is [spec/formats.md §14](../spec/formats.md#14-device-checkpoints).

## 1. What the server can and can't do

Everything a client stores is sealed under keys the server never has. So **the server can't**:
* read a value, an event, a file, a row or a block
* forge one, or change one: the AEAD tag fails
* move a value to another key, topic, fs or volume: each value is sealed with where it lives in its AAD

**Without integrity, the server can** still lie by omission and by age. Every answer it gives is a value some client once wrote, so it opens and looks right. It can:
* serve an **older** value: a row before its last update, a header before a key rotation, a filesystem tree before the latest moves
* **withhold** the newest data: a topic's last events, a device's latest checkpoints, a file's latest operations
* show two groups of clients two histories, a **fork**: each group's writes are hidden from the other

Integrity detects all three. The limit is **fork consistency** (DESIGN.md §2.6): a server that keeps two groups of devices apart *forever*, never letting either see a checkpoint of the other, can't be caught by anything inside the system. The moment one checkpoint crosses between the groups, or two users compare their pins over another channel (§7), the fork shows. An independent witness would close that last gap (TD-WITNESS). This is a property of any system where the server is the only channel, not a gap in the implementation.

## 2. How it works

**Device checkpoints.** Each device with a signing key writes **checkpoints** to a per-fs log on the server (formats.md §14):
* **Chained:** checkpoint `seq` names the hash of checkpoint `seq − 1`.
* **Signed** with the device's hybrid signature (Ed25519 and ML-DSA-65; both must verify), so the server can't make one.
* **Sealed** under the fs key: the server sees which devices checkpoint and when, not what they name.
* **Cumulative:** each one names the newest state of every object the device wrote or read through. That means topic heads and its own event chains, tree op counts and state hashes, relayed connections' cursors, namespace and volume roots, plus the ACL and header versions it verified. So a verifier needs only each device's newest checkpoint.
* **Cross-linked:** each one also names the newest checkpoint it verified from every other device (`seen`).

**Pins and floors.** A client verifies each device's newest checkpoint and keeps it as a **pin**. From the pins it keeps a **floor** per object: the newest state any device vouched for. Every server answer about an object is then checked against its floor:
* an answer **below** the floor is a rollback
* an answer **beside** it (the same position, other content) is a fork

The client also keeps its own position for everything it wrote, so a rollback of its own writes is caught even before it checkpoints.

**Forks.** A device's checkpoints form one chain. A server that forks devices A and B must show each a different chain for the other, or hide the other's newest checkpoints. Either way it breaks as soon as one checkpoint crosses. Say it hides B's newest checkpoints from a third device, C:

```
  what A signed:   A1 ── A2 ── A3 ── A4 {seen: B3}
  what B signed:   B1 ── B2 ── B3 ── B4 {seen: A2}

  the server hides B3 and B4 from C, so C sees B only up to B2.
  C verifies A4 and reads "seen: B3"; it asks the server for B3:
    • no B3 to show                        → fork (a seq the server can't produce)
    • a B3 that B didn't sign              → invalid
    • a B3 other than the one A saw        → fork (another checkpoint at one seq)
```

So the server can hide the newest checkpoints of B from C only as long as C sees no checkpoint of a device that has seen them. The same holds for exported pins compared out of band (§7): each device head in them is checked as a `seen` link.

**The cadence and its window.** Checkpoints aren't written on every change: a device writes one after `sigEvery` changes (default 64), `delayMs` after its last change (default 5 s), on `checkpoint()`, and on close. Between a change and its device's next checkpoint, **other devices** have no floor for it, so the server can show them the state before it. That's the window. `sigEvery: 1` closes it, at one signature and one put per change (§9). The writing device itself is never in the window: it keeps its own position.

**Keyed chains inside objects.** Objects carry hash chains of their own, so the server can't forge a successor or skip a link:
* events: `(chain id, dseq, dprev)` per writer
* trees: `(ops, chain)`, extended by every operation and purge
* zen-db namespaces: NsRoot `(seq, prev)`
* volumes: the root record `(seq, prev)`

These chains are inside sealed values, so they need no signature per write. Checkpoints sign the heads.

## 3. What is covered

| Object | What a device notes | Where it's checked | What's caught | Spec |
|---|---|---|---|---|
| **ACL** | the version and hash it verified | every ACL read (`acl.head()`), and against each checkpoint's ACL claim | an ACL below the pin, or another ACL at the pinned version | formats.md §9, §14.4 |
| **Signed fs header** | `(version, HH)` | every header read | an older version, another header at a version, an unsigned header after a signed one | formats.md §12.1 |
| **Topics** | the head it read through; its own chains `(dseq, hash)` | unfiltered reads and subscriptions; the end of each read; `topic.verifyHead()` | gaps and rewrites in a writer's chain; a withheld tail (a head below the floor) | formats.md §5.1, §14.1; GAPS G26 |
| **Trees** (`@zen/fs`) | `(ops, chain, state)` where its replica is caught up | each caught-up point of the replica | an `ops` below the floor (rollback); the same `ops` with another chain or state (another history, or a wrong merge) | fs.md §9 |
| **Relayed connections** (`@zen/svc`) | `(in_acked, out_head)` after each step | each claim, after a sync | a claim older than the newest step | svc.md §12; SVC.md §6 |
| **Authenticated zen-db namespaces** | the NsRoot `(seq, NH)` | every read, against the NsRoot read in the same transaction; the NsRoot against the floor | an older or withheld row, index entry or catalog record; an older namespace | zendb.md §14; DB.md §7.1 |
| **Volumes** (zen-blk, zen-ublk) | the root record `(seq, RH)` | every block read, against the tree; the root at open, against the floor and the writer's pin | an older block, node or root | blk.md §5–§6; BLK.md §6 |
| **Checkpoints themselves** | each device's head `(seq, HC)` | every sync | a device's head below its pin (withheld); two checkpoints at one `seq`, or a `seen` link the server can't produce (fork); one that doesn't open or verify (`invalid`) | formats.md §14.4 |

## 4. What isn't covered

* **Basic zen-db namespaces**, and plain KV values outside the objects above. Each value is sealed, so the server can't forge one, but it can serve an older one or leave one out of a range. Use an authenticated namespace (`integrity: 'authenticated'`) where freshness matters. Its writers serialize on the NsRoot (DB.md §7.1).
* **Ephemeral messages** (`epub`) have no history to check against: only the client's replay filter applies, so the server can drop or delay them.
* **A file's version records.** The tree state covers each node's version *count* and change offset, not the version records and manifests themselves. A server could serve an older version record set under a node's current change offset, and the replica would show the older content (still content some client wrote; it can't forge content). This is TD-FS-VERSIONS-INTEGRITY.
* **`/unencrypted` values** aren't sealed, so nothing here applies.
* **Readers that filter** (topic reads by `key`, consumer groups, partitioned readers) see subsets of each chain. They check for rewrites against the pins, not for gaps.
* **CRDT tables** are refused in authenticated namespaces: their merged state is the server's.
* **The first sync trusts what it's shown.** A device with no pins has no floor: a server can show a brand-new device an older (but consistent) state. From its first sync on, it can't go back. Comparing pins (§7) with a device that has them closes this.
* **Within the window** (§2), other devices can be shown the state before a change.

## 5. Turning it on

Integrity is opt-in per fs object, and attached **before unlocking**, so the header is checked too.

* **TypeScript:** `const integ = await session.fs(1).integrity({device, local, sigEvery, delayMs, onAlarm})`, then `unlock()`. Without `device` the client verifies but writes no checkpoints. `local` is where pins are kept (default: memory, so a new process starts over). CLIENT.md §12.
* **Rust:** `fs.integrity(IntegrityOptions {signer, pins, …})`, or any `zen-client` CLI command, which attaches integrity with a pins file next to the device key. CLIENT.md §13.
* **Node workers** (`apps/shared`): `ZEN_INTEGRITY=1`, or `integrity` in `signInWorker`. The pins are kept in the worker's vault directory, when it has one.
* **zen-db:** `Db.open(fs, ns, {integrity: 'authenticated'})` creates the namespace authenticated, and needs integrity attached to the fs. A namespace created authenticated stays so, and opens verified without the option.
* **`@zen/fs` replicas** and **`@zen/svc` handlers** use the integrity attached to their fs: nothing else to set.
* **zen-ublk:** always on. A volume can't be opened without it.
* **zen-mount:** not yet (TD-FUSE-INTEGRITY).

## 6. Signers and enrollment

A device writes checkpoints only with a **signing key**: a device key certified by the user's identity.

* **A device in the ACL** (method 1 sign-in, `DeviceCredentials`) already has one. So do password-key sessions (method 6).
* **Sessions without a device key** (passkey, OPAQUE, API token, mTLS) **enroll** one:
  * `session.enrollDevice(identity, {vault})` makes a device key, certifies it with the user identity and registers it with the server (api.md §15.2). With `vault`, it's kept there as the signer.
  * `session.approveDevice(devicePublic, identity)` certifies a key made elsewhere: a phone shows its device key as a code, and a laptop holding the identity approves it.
* **Verifiers** check each checkpoint's signature against its device's cert, and the cert against the user's member identity in the verified ACL. The server's device registry only tells them which cert to check. A user may register up to 64 devices.
* **An enrolled device signs checkpoints but doesn't sign in.** Signing in by device key (method 1) needs the cert in the ACL itself (auth.md §3), which only an admin can add. Enrollment exists so sessions that sign in some other way can still sign checkpoints.
* **Without a signer**, a client still verifies everything, with the same alarms. It just adds no checkpoints of its own, so other devices learn nothing from what it read.

## 7. Pins

**Where they live:**
* **TS:** the store passed as `local` (store `integrity/<fs>`): the pinned ACL bytes, the header pin, and each device's head with its body. Floors are rebuilt from the bodies on load.
* **Rust and the CLI:** the pins file, by default the device key file's path + `.pins-<fs>`, mode 0600, written atomically.
* **zen-ublk** also keeps a **volume pin** (`--vol-pin`), the writer's own newest root, next to the pins file.

**Keep them with the device key.** Losing them costs trust on first use: the next run accepts whatever the server shows at its first sync. A pins file that doesn't decode, or belongs to another fs, is refused rather than ignored, since silently starting over would hide a rollback.

**Comparing out of band.** Two devices the server keeps on separate histories find the fork as soon as either sees a checkpoint of the other. To check without waiting, compare pins over another channel:
* `integrity.exportPins()` gives a few hundred bytes: the ACL and header pins and each device's head (formats.md §14.5). Show it as a QR code, or send it over chat.
* `integrity.comparePins(bytes)` on the other device returns the alarms. Each device head is checked as a `seen` link, and the ACL and header pins as claims.
* Between a phone and a server: `zen-client … pins compare -` reads the phone's export, and `zen-client … pins export` prints hex for the phone. Exit 0 means one history so far; 3 means an alarm, printed as JSON.

## 8. Alarms and what to do

**An alarm** is `{kind, object, id?, detail}`:

| Kind | Meaning |
|---|---|
| `rollback` | an answer older than the floor or the device's own pin: an older value, a withheld tail, withheld checkpoints |
| `fork` | two histories: the same position with other content, or a `seen` link the server can't produce |
| `gap` | a writer's event chain skipped a position (an event withheld from the middle of a topic) |
| `rewrite` | another event at a position already seen |
| `invalid` | a checkpoint that doesn't open, or whose signature, cert or fields don't verify: bytes no device sealed |

**What each component does:**
* **The call that found it throws** an `IntegrityError` (code `integrity`, with `.alarm`), after `onAlarm` and `integrity.alarms` record it. From then on `integrity.trusted` is false and the device writes no more checkpoints: it won't sign a view it knows is false.
* **A replica** (`@zen/fs`) stops following the change feed. `sync()` rethrows the alarm, and `status().alarm` has it. Local writes still queue and send, since the server can't forge them.
* **A handler** (`@zen/svc`) aborts the connection whose claim failed, and gives up a batch of claims it can't verify.
* **An authenticated namespace:** the transaction fails with the alarm.
* **zen-ublk** fails closed: every request gets `EIO`, the device stays until the daemon is stopped, and it exits 3.
* **The Rust CLI** prints the alarm as JSON on stderr and exits 3.

**A suggested app policy:**
1. Stop writing to the fs, and tell the user plainly: the server, or someone controlling it, is serving stale or split data.
2. Keep the pins. Don't clear them to "fix" the alarm; they are the evidence.
3. Compare pins with another device of the same user or group (§7). If that device sees the fork too, the server is lying. If it doesn't, it is on the other side of the split.
4. Report it to whoever runs the server, with the alarm's `detail`. Move the data off with a device whose pins agree with the honest side.

An honest server never causes an alarm: the tests run honest workloads (concurrent writers, restarts, reconnects, header edits, ACL updates, trash purges) and assert none.

## 9. Choosing the cadence

* **Defaults:** `sigEvery: 64`, `delayMs: 5000`. A device writes a checkpoint at most every 64 changes, and 5 s after it goes quiet. A busy device writes a few checkpoints a second at most; an idle one writes none.
* **`sigEvery: 1`** closes the window: every change is vouched for at once. Use it where another device must never see the state before a change: a handler that moves money (svc handlers note every step), a volume that another host mounts read-only.
* **The costs** (STATS.md §6):
  * Each checkpoint is a signature, a seal and a put: about 4.4 KB stored, kept up to `checkpoint_keep` (1,024) per device, counted in the fs quota.
  * Signing costs 2.6 ms in WASM and 0.2 ms natively. In a browser, `sigEvery: 1` adds the signature to every change.
  * A sync costs one round trip for the heads, then one get and one verify per device that moved.
* **`close()`** always writes a last checkpoint. A process that exits without it leaves its last changes in the window until it next runs.

## 10. Testing against a lying server

Detection is tested against a server that lies, never only against an honest one. The same harness is available to apps.

* **`spawnServer({adversary: true})`** (`@zen/client/testing`) starts a zen-serve built with `--features test-utils` and `adversary = true`. Read endpoints then go through a storage layer that can lie; commits and the server's own bookkeeping stay honest.
* **`lie(adminSession, lies, {clear?})`** freezes what readers see, each lie "as it is now":
  * `{kind: 'kv', fs, key}`: a stored key's value, or its absence
  * `{kind: 'header', fs}`: the fs header
  * `{kind: 'topic', fs, topic}`: a topic's head and everything after it
  * `{kind: 'tree', fs, tree}`: a tree's header and its newer changes
  * `{kind: 'checkpoints', fs, device}`: a device's checkpoint head

  `truth(session)` drops them all.
* **`TestServer.fork()`** copies the server's store and starts a second server on the copy: two consistent histories, for fork tests and whole-store rollbacks.
* **Tampering per session:** pass a `fetch` wrapper to one client to rewrite its answers. The tests use it for a wrong merge and a replayed claim.
* **Rust:** zen-client's test harness (`crates/zen-client/tests/common`) has `Server::start_with` (the adversary on), `lie`, `truth` and `Server::fork`.

**A good attack test** shows first that the attack works (the reader is really served the stale or forked answer), then that it's caught. Run it once with its check disabled, to see that it fails.
