# zen-serve: design part 4 (keyed and sequential consumers, server-side CRDTs on ciphertext)

Follows DESIGN-3 and API.md.

---

## 1. Event keys, sequential delivery, per-key offsets

### 1.1 Event key

An append may carry an optional `key`, sent next to `topic`.

* The client sends `key_token = PRF(K_topic, key)`, 16 bytes. It's scoped per topic, so the same entity can't be linked across topics.
* **Leak (documented):** the server learns *which events concern the same entity*, but not the entity itself.
* On append, zen-serve also writes a **topic-key index** in the same transaction:

```
(t, topic, versionstamp)                -> envelope              // main log
(t, topic, "k", key_token, versionstamp) -> ∅                    // per-key log
```

So "the next event for key K after offset v" is a single range read.

### 1.2 Consumer modes

Set per consumer group:

| Mode | Order guarantee | Parallelism | Server state |
|---|---|---|---|
| `broadcast` | per topic, per subscriber | everyone gets everything | none (client keeps its own cursor) |
| `sequential` | **e+1 is not delivered until e is committed** | 1 | one cursor |
| `partitioned(N)` | sequential per partition (`key_token mod N`) | N | N cursors + N leases |
| `per_key` | **sequential per key**, parallel across keys | as many as there are ready keys | per-key cursor + low watermark (§1.3) |
| `single_key(K)` | sequential, only events of key K | 1 | one per-key cursor |

`sequential` is not just ordered commits. It's a **delivery gate**: the server doesn't hand out (push or pull) e+1 to anyone until e's commit has advanced the cursor. `max_inflight = 1` is the default for sequential groups. A group can raise `max_inflight` for prefetching; commits still have to land in order.

### 1.3 Server bookkeeping for `per_key`

```
(c, group, topic, "kc", key_token)            -> last committed versionstamp for that key
(c, group, topic, "ready", next_vs, key_token) -> ∅      // keys with pending work, oldest first
(c, group, topic, "claim", key_token)         -> {holder, token, expires}   // only while in flight
```

* **On append** (same transaction): if the key had no pending work, insert `ready(vs, key)`. The versionstamped-key write puts the commit version inside the key. Cost: one extra write per registered `per_key` group. That's negligible for a handful of groups. With many groups we'd switch to a lazy dispatcher.
* **Dispatch:** read the first entries of `ready` whose key has no live `claim`, create a claim with a fencing token, and deliver. Keys are dispatched oldest-pending-first, so no key starves.
* **On commit** with `consume: {key, from, to, claim_token}`, all in one transaction:
  1. check the claim token and that `kc == from`
  2. set `kc = to`
  3. delete `ready(old)`, then insert `ready(next event of that key)` if one exists
  4. release the claim
* **Low watermark** = the first entry of `ready`: the smallest uncommitted offset in the whole topic for this group. It's used for:
  * resuming
  * monitoring lag
  * retention: events below every group's low watermark (and older than retention) can be deleted, along with their per-key index entries

Claims are short leases measured in commit versions (DESIGN-3 §3.1). A crashed worker's key becomes available again when its claim expires, and its late commit is rejected (stale token). Other keys never wait for it.

### 1.4 API additions

```
append:  [{topic, key_token?, envelope}]
consume: [{group, topic, key_token?, from, to, token}]
POST /v1/consume/groups   {group, topic, mode, partitions?, key_token?, max_inflight?}
WS   /v1/stream           {consume: group}  → server pushes ready events + claim tokens
```

```ts
zen.topic("orders").publish(order, { key: order.customerId });
zen.consumer({ group: "billing", topic: "orders", mode: "per_key" })
   .run(async (event, tx) => { ... });          // same tx binding as before
zen.consumer({ group: "audit", topic: "orders", mode: "sequential" }).run(...);
zen.consumer({ group: "c42", topic: "orders", mode: "single_key", key: "cust-42" }).run(...);
```

---

## 2. Server-side CRDTs on encrypted data

### 2.1 The key observation

Most CRDT merge rules **never look at the user's values**. They look at *metadata*:
* operation ids `(replica, counter)`
* timestamps
* version vectors
* parent pointers
* left/right neighbour ids

The values are just carried along. So a CRDT can be split into two parts:

```
op = { metadata (opaque ids, clocks)  ← server can read and merge on this
     , payload  (AEAD ciphertext)     ← server carries, never reads }
```

* All ids are either **random** (node ids, replica ids) or **PRF tokens** (map keys). They mean nothing to the server.
* The server runs the merge algorithm on metadata alone, and stores and serves the **merged, compacted state**.
* Clients decrypt only the payloads they need.

### 2.2 Which CRDTs work this way

| CRDT | Server needs | Works on ciphertext? |
|---|---|---|
| LWW register / LWW map | key token + HLC timestamp + replica id | **Yes**: keep the highest (ts, replica) |
| Multi-value register | version vector | **Yes**: keep values no other value dominates (siblings = conflict) |
| OR-set / add-wins set | element token + add/remove tags | **Yes** |
| G-counter / PN-counter | per-replica entries + a per-replica sequence number | **Yes**: keep each replica's newest entry. Only the *sum* is client-side. |
| **Move-tree (Kleppmann et al. 2021)** | node id, parent id, timestamp | **Yes**: cycle checks and undo/redo only use ids |
| Sequence/text (RGA, Fugue, Yjs-style) | op id + left/right origin ids | **Yes** in principle, but it leaks the edit structure (where and how much was typed). Deferred; rich text stays client-side in Loro. |
| Arithmetic on values (sums, max of value) | the value itself | Only with homomorphic encryption: lattice-based (PQ) additive HE for counters. Research territory; per-replica decomposition avoids it. |

Academic background:
* Barbosa et al., "Secure Conflict-free Replicated Data Types" (2021)
* Kleppmann, "Making CRDTs Byzantine Fault Tolerant" (2022)
* Kleppmann et al., "A highly-available move operation for replicated trees" (2021)
* Ink & Switch's Keyhive/Beelay (encrypted sync, client-side merge)

### 2.3 The server-side CRDT filesystem

```
node     = random 128-bit id
(fs, "n", node)              -> {parent, move_ts, enc_meta{name, mode, mtime, xattrs}, content_ref}
(fs, "p", parent, node)      -> ∅          // children index → one-call readdir
(fs, "m", move_ts, op_id)    -> move op    // bounded move log for late/offline ops
content  = MV-register of enc_chunk_list per file (siblings = conflict versions)
chunks   = (fs, "c", chunk_id) -> AEAD(chunk)
```

* **Moves, renames, create and delete** are move-ops `(ts, node, new_parent, enc_meta)`. The server applies them in timestamp order and skips any move that would create a cycle. An op that arrives late from an offline client triggers undo/redo over the move log. All of this uses ids only.
* **Ordering and clock abuse:**
  * Timestamps are hybrid logical clocks (HLC).
  * The server rejects timestamps more than a small skew ahead of its own clock, so no member can win every conflict by writing from "the future".
  * Ops from online clients effectively serialize in commit order.
* **Garbage collection:** delete markers are a move into a trash node. The move log and tombstones are trimmed once **causally stable**, meaning every registered replica's acknowledged version vector has passed them. The server can track this because it sees the vectors.
* **Concurrent file writes:** the MV-register keeps both versions as siblings, and the client shows "conflict copy". Optionally, each chunk is its own LWW register, so edits to different regions merge.
* **Duplicate names in one directory:** the server can't see names, so two concurrent `foo.txt` files can exist and the client shows them as `foo.txt` / `foo (2).txt`. **Optional** `name_token = PRF(K_dir, name)` lets the server enforce uniqueness, but leaks name equality within a directory. Off by default.

### 2.4 What the server gains by merging

* **Compaction without clients.** The materialized state replaces the op history, so new devices download state, not the whole history.
* **Partial sync.** The server knows the tree shape, so a client fetches just the directory it opens, plus subscriptions to that subtree.
* **Offline-first and online-strong together:**
  * Online clients get immediate, server-ordered results.
  * Offline clients' ops merge correctly when they return.
  * KV transactions (DESIGN-3) are still available where a hard invariant is needed.
* **Change feed.** Each applied op appends a change event to a topic, keyed by node id. Per-key consumers (§1) then react to a single file's or directory's changes.

### 2.5 Trust

The server is still untrusted:
* **Ops are authenticated:** AEAD on payloads, and per-device signatures or checkpoints over the op hashes.
* **The merged state is an optimization, not an authority.**
  * A client can replay the ops it holds and compare.
  * Checkpoints signed by clients over (state hash, op-set hash) make wrong merges detectable, in the spirit of Snapdoc and Byzantine-tolerant CRDTs.
  * A server that merges wrongly can deny service, but can't forge content.

### 2.6 Leakage

Compared with the KV-based filesystem, the server additionally sees:
* the **tree shape**: which node sits under which, children counts, depth
* when moves and renames happen
* files' chunk counts
* which replica edited what, and when

It does not see names, contents or metadata values. Optional padding nodes and chunks can blur counts.

### 2.7 Placement in the architecture

A new server primitive, **CRDT objects**: `(fs, "crdt", object_id, type)` with server-understood types `lww_map`, `mv_register`, `or_set`, `counter`, `move_tree`.

* Ops come in through the same `/v1/commit` (`crdt_ops: [{object, op}]`) and are merged inside that FoundationDB transaction. So they mix atomically with KV writes, event appends and event consumption.
* The **filesystem** becomes `move_tree` + MV-register files, server-merged, which replaces both the KV filesystem and the client-side tree CRDT.
* **Loro** remains for rich documents (text, lists), where merging on the client is better for privacy.
