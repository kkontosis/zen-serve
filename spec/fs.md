# The CRDT filesystem

A filesystem whose **server merges concurrent changes without seeing names, metadata or contents** (DESIGN-4 §2.3). It is built from three parts:
* a **move-tree** CRDT for the hierarchy (Kleppmann et al., "A highly-available move operation for replicated trees", 2021)
* an **LWW register** per node for its sealed metadata
* a **multi-value register** per node for its content, which references immutable encrypted **chunks**

The server orders and merges operations using ids and clocks only. Everything a user would recognize is ciphertext: names, modes, times, sizes, contents. Byte layouts are in formats.md §11, the wire API in api.md §6 and §12, and the storage layout in keyspace.md §3.6.

## 1. Objects

* **Tree.** A filesystem tree in an fs, identified by a random 16-byte `tree` id. An fs can hold any number of trees (for example one per mount). A tree exists from its first operation.
* **Node.** A file, directory or symlink, identified by a random 16-byte id. Two ids are reserved in every tree:
  * `ROOT = 00…00`: the top of the tree.
  * `TRASH = FF…FF`: deleted nodes live under it until they are purged (§6).
  
  Both always exist and can't be moved, renamed or written. They have no metadata.
* A node carries:
  * **parent**: a node id, set by moves. A node with no parent (a creation that was undone, §3.2) is invisible.
  * **meta**: sealed metadata (formats.md §11.2): name, type, mode, mtime, xattrs. It is a last-writer-wins register.
  * **content**: a set of **versions**. Usually one; several after concurrent writes (§4).

The server does not know whether a node is a file or a directory. The type is inside `meta`. Any node can have children and content.

## 2. Timestamps

Every tree and metadata operation carries a **timestamp**:

```
ts  = (hlc: u64, device: bytes(32))       ordered by hlc, then device bytes
hlc = unix_ms << 16 | counter             (formats.md §11.1)
```

* `hlc` is a hybrid logical clock chosen by the client. A client keeps `last`, and for each new operation uses `last = max(wall_ms << 16, last + 1)`. Whenever it reads a timestamp from the server (node states, `time_ms` in `/v1/info`), it sets `last = max(last, seen)`. So an operation is always later than everything its author had seen.
* `device` is **set by the server** to the fingerprint of the session's device. A device therefore can't take another replica's place in the order.
* A timestamp is unique: a second operation with the same `(hlc, device)` on the same tree is refused (400).

## 3. The tree

### 3.1 `move`

```
move {node, parent, hlc, meta?}
```

* `move` covers every structural change:

  | Change | As a move |
  |---|---|
  | create | a move of a new `node` id |
  | move | a move to another parent |
  | rename | a move to the same parent with new `meta` |
  | delete | a move to `TRASH` |
  | restore | a move out of `TRASH` |

* `parent` must be `ROOT`, `TRASH` or an existing node of the tree.
* `node` can't be `ROOT` or `TRASH`, and can't be its own `parent` (400). It may be new: this is a creation.
* If `meta` is present, it is also applied as a `meta` operation with the same timestamp (§3.3). It sits in the move only so that create and rename are a single operation.

### 3.2 Merge rule

The tree is defined as the result of applying **all moves in timestamp order**, starting from an empty tree. Applying one move:
* Let `old` be the node's current parent, or none.
* If `node` is an ancestor of `parent`, the move is **skipped**: it would create a cycle. The state is unchanged.
* Otherwise the node's parent becomes `parent`.

The result depends only on the *set* of moves, not on the order they arrive in. The server keeps it so with a **move log**, holding each move with the `old` parent it saw:
* **A move later than every logged move** (the normal online case) is applied and logged.
* **A late move** (from a client that was offline) goes through undo, apply, redo:
  1. Every logged move with a greater timestamp is **undone**, newest first, by restoring its `old` parent.
  2. The late move is applied and logged.
  3. The undone moves are **redone** in timestamp order. Each one is applied again, recomputing its `old` parent and cycle check.

All of this happens inside the commit's transaction, so readers never see an intermediate state.

**Consequences for users:**
* Concurrent moves of the same node: the later timestamp wins.
* Concurrent moves that would form a cycle (A into B while B into A): the later one is skipped.
* An older offline move doesn't override a newer online one.
* Nothing is ever lost or duplicated, and the result is always a tree.

### 3.3 `meta`

```
meta {node, hlc, meta}
```

* `meta` is a last-writer-wins register: the value with the greatest timestamp is kept.
* It is independent of the parent, so a concurrent rename and move both survive.
* `node` must exist (it has had at least one move).

### 3.4 Limits on late moves (G10)

* **Clock skew.** An `hlc` more than `limits.crdt_max_skew_ms` (default 60,000) ahead of the server's clock is refused with 409 `clock_skew`. Otherwise one member could win every conflict by writing from the future. The client fixes its clock or waits.
* **Horizon.** An `hlc` older than `limits.crdt_horizon_secs` (default 7 days) is refused with 409 `stale_op`.
* **Undo depth.** A move whose undo/redo would touch more than `limits.crdt_max_redo` logged moves (default 1,000) is also refused with 409 `stale_op`.
* **Rebase.** The client reissues a `stale_op` operation with a fresh `hlc`, and the operation then takes its arrival position. The client library does this automatically. The user-visible effect: very old offline moves are applied as if made at sync time.

## 4. Content

```
write {node, replaces: [dot], chunks: [chunk_id], manifest}
```

* A node's content is a **multi-value register** of **versions**:
  * Each version is `{dot, device, chunks, manifest}`.
  * `dot` is a 12-byte id assigned by the server: `versionstamp ‖ u16(i)`, where `i` is the write's position among the commit's writes. Dots are returned in the commit result.
* `replaces` lists the dots of the versions the writer had seen. The server removes those that still exist, then adds the new version.
* **Concurrent writes** each replace only what their author saw, so both survive as **siblings**. A client shows them as conflict copies. A later write that lists both resolves the conflict.
* `chunks` lists the version's chunk ids in file order. The server sees them (so it can garbage-collect chunks), but not their order's meaning or the file size.
* `manifest` is the sealed manifest (formats.md §11.3): the file size, chunk size, and the same chunk list, authenticated. Clients check that it matches `chunks`.
* `write` with no chunks writes an empty file. The server doesn't distinguish files from directories, so the client decides whether a directory has content.
* Every chunk must exist when the write commits: uploaded earlier, or in the same commit.
* `node` must exist.

### 4.1 Chunks

* A chunk is an immutable sealed blob (formats.md §11.4), at most `max_value_bytes`. Clients use 64 KiB of plaintext.
* Chunk ids are random 16-byte ids chosen by the client. They belong to the fs, not to a tree.
* Chunks are uploaded through the commit field `chunks` (api.md §6). Large files are uploaded over several commits, then referenced by one `write`.
* Re-uploading an identical chunk stores nothing new but restarts its grace period (below). Uploading different bytes under an existing id is refused (400).
* The server counts references to every chunk. A chunk that no version references, and hasn't been uploaded or referenced for `limits.chunk_grace_secs` (default 24 h), is deleted (§6). The grace period covers chunks uploaded in one commit and referenced by a later one. It is measured from the chunk's **newest** upload or release (keyspace.md §3.6, `cp`), so an earlier one that has aged past the grace period doesn't delete a chunk that was uploaded or released again since.

## 5. Change feed and sync

* Every time a node's state changes, it gets a new **change offset**: a 12-byte, strictly increasing position. A node changes when its parent, meta or content changes, including through redo.
* `/v1/fs/tree/changes` (api.md §12) returns the nodes changed after a cursor, in offset order, each with its full current state:
  * **Full sync:** start with no cursor. This returns every live node, plus tombstones of purged nodes.
  * **Partial sync:** continue from the last cursor.
  * With `wait_ms` the request long-polls until something changes.
* A client applies changes to its local replica, and its own operations optimistically on top. Since the server state is the merge, the client simply takes the server's node states when they arrive.
* **Purged nodes** appear once as tombstones. Tombstones older than the horizon are dropped. A cursor from before the oldest kept tombstone gets 409 `resync`, and the client starts a full sync.
* **Partial trees:** `/v1/fs/tree/children` lists one directory, so a client can fetch only what it opens.

## 6. Garbage collection

The sweeper (every node runs it) keeps the tree bounded:
* **Move log:** entries older than the horizon are removed. No accepted operation can need them (§3.4).
* **Margin.** "Older than the horizon" here means older than `crdt_horizon_secs` plus `crdt_max_skew_ms` by the sweeping node's clock. So a node whose clock is slightly behind never accepts an operation that needs a removed entry.
* **Trash purge.** A child of `TRASH` is purged when:
  * its move into trash is older than the horizon, and
  * every node in its subtree has a move older than the horizon.

  Purging removes the whole subtree: node records, versions, children entries. Chunk references are released, and each purged node leaves a tombstone in the change feed.
* **Chunks:** unreferenced chunks are deleted after the grace period (§4.1).
* **Lost and found.** Undo and redo after a purge can, in rare cases, leave a node whose parent no longer exists. Logged moves of a purged node itself (a skipped move can name one) are passed over by undo and redo. Such a node is an **orphan**. `tree/children` of the missing parent id still lists it, and clients show orphans in a "lost+found" folder. Moving it anywhere repairs it.

## 7. Transactions (G11)

* `crdt_ops` and `chunks` are part of a commit, so they apply atomically with its KV writes, appends and consumes.
* **Long mode** (no `read_version`): the server retries its own conflicts. CRDT operations never fail with `conflict`; they only fail for the reasons listed above.
* **Short mode** (`read_version`): every state the server reads conflicts like any other read, and the client retries.
* All operations on one tree serialize at the tree's header. One tree comfortably handles family-scale load. Use several trees to scale further.

## 8. Names

* The server can't see names, so two concurrent creations of `foo.txt` in one directory both exist. Clients show them as `foo.txt` and `foo (2).txt`, ordered by node id. Renaming either resolves the clash.
* Server-enforced unique names (`name_token = PRF(K_dir, name)`, DESIGN-4 §2.3) are reserved for a later version. They would leak name equality within a directory.

## 9. Integrity

* **Payloads** (meta, manifests, chunks) are AEAD-sealed and bound to their fs, tree and node or chunk id (formats.md §11). The server can't forge, alter or move them.
* **Op chain.**
  * The server records the sending device with every operation.
  * It extends a per-tree hash chain over operations in **arrival order**: `chain_n = H("zen/v1/tree-op-chain", chain_{n−1} ‖ lp(op_bytes) ‖ device)`, formats.md §11.5.
  * `/v1/fs/tree/chain` returns `(count, chain)`.
* **What the server could still do.** It could serve an old version as the current one, drop operations, or merge wrongly. That would deny service or show stale data, but it can't forge content. A client that replays the operations it holds detects a wrong merge.
* **Signed checkpoints** (a later milestone): devices sign `(state hash, count, chain)` with the purpose `zen/v1/sig/tree-checkpoint`, so other devices can check the server's history.

## 10. Leakage

Compared with the plain KV store, the server additionally sees, per tree:
* **its shape:** which node is under which, children counts, depth
* **timing:** when nodes are created, moved, renamed, deleted and written, and by which device
* **content structure:** the number of chunks in each version, and which chunks versions share (dedup between versions of one file)
* **sizes:** the size of each sealed metadata value and chunk, which approximates name lengths and file sizes to 64 KiB granularity unless the client pads

It does not see names, types, modes, times, sizes inside the manifest, or contents. Clients may add padding nodes, padding chunks or padded metadata to blur the counts.
