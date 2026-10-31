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
* **Depth.** A `move` whose `parent` has more than `limits.crdt_max_depth` ancestors (default 1,000) is refused with 400: the cycle check walks them, one read each.
* **Purged nodes.** An operation that names a purged node (§6) as its `node` or `parent`, while the node's tombstone is still kept, is refused with 409 `stale_op`. The server no longer has the state the operation refers to; applying it would resurrect an empty node (no meta, no content) in place of what the sequential merge would show. After the tombstone is dropped the id is unknown: a `move` of it is a creation, and a `move` under it fails with 400 (unknown parent).
* **Rebase.** The client reissues a `stale_op` operation with a fresh `hlc`, and the operation then takes its arrival position. The client library does this automatically. The user-visible effect: very old offline moves are applied as if made at sync time.

## 4. Content

```
write {node, replaces: [dot], chunks: [chunk_id] | pages: [page_id], manifest}
```

* A node's content is a **multi-value register** of **versions**:
  * Each version is `{dot, device, chunks | pages, manifest}`.
  * `dot` is a 12-byte id assigned by the server: `versionstamp ‖ u16(i)`, where `i` is the write's position among the commit's writes. Dots are returned in the commit result.
* `replaces` lists the dots of the versions the writer had seen. The server removes those that still exist, then adds the new version.
* **Concurrent writes** each replace only what their author saw, so both survive as **siblings**. A client shows them as conflict copies. A later write that lists both resolves the conflict.
* A version lists its chunks in one of two forms:
  * **inline:** `chunks`, the chunk ids in file order
  * **paged:** `pages`, the ids of the pages (§4.1) that hold the chunk ids, in file order. A large file uses it: the inline list must fit one value with the manifest.

  The server sees the ids (so it can garbage-collect chunks and pages), but not their order's meaning or the file size. A `write` with both `chunks` and `pages` is refused (400).
* `manifest` is the sealed manifest (formats.md §11.3): the file size, the chunk size (version 2) or each chunk's length (version 3), how its chunks are keyed (§4.2), and the same chunk list or page list, authenticated. Clients check that it matches `chunks` or `pages`, and check each page against its digest.
* `write` with neither writes an empty file. The server doesn't distinguish files from directories, so the client decides whether a directory has content.
* Every chunk, or every page, must exist when the write commits: uploaded earlier, or in the same commit.
* `node` must exist.

### 4.1 Chunks

* A chunk is an immutable sealed blob (formats.md §11.4), at most `max_value_bytes`. Clients cut a file into chunks by the fs policy (formats.md §12): by content (`chunking = cdc`, formats.md §11.7: 8 KiB to about 88 KiB, 32 KiB on average at the defaults), so that an insertion re-uploads only the chunks around it; or at a fixed 64 KiB (`fixed`).
* Chunk ids are random 16-byte ids chosen by the client. They belong to the fs, not to a tree.
* Chunks are uploaded through the commit field `chunks` (api.md §6). Large files are uploaded over several commits, then referenced by one `write`.
* **Pages** (formats.md §11.6) are uploaded through the commit field `pages`, each `{fs, id, data}`. Every chunk a page lists must exist when the page is uploaded: uploaded earlier, or in the same commit (the commit's chunks apply before its pages). A page is immutable: re-uploading identical bytes stores nothing new but restarts its grace period, and different bytes under an existing id are refused (400). The first upload of a page counts one reference to each chunk it lists (once per occurrence), so a page shared by many versions holds its chunks once.
* The server counts references to every page (one per paged version listing it, once per occurrence). A page no version references, and that hasn't been uploaded or released for `limits.chunk_grace_secs`, is deleted, which releases its chunk references (§6). Its grace is measured like a chunk's, from its newest upload or release (keyspace.md §3.6, `pc`).
* Re-uploading an identical chunk stores nothing new but restarts its grace period (below). Uploading different bytes under an existing id is refused (400).
* **Upload sessions** (api.md §12.1) keep a long upload's content past the grace period. A chunk or page put in a commit that names an open session is referenced by that session until it is closed or expires, and then released like any other reference, so its grace period starts then. A resume days later finds what it uploaded, and the `write` that finally references it needs no re-upload.
* The server counts references to every chunk. A chunk that no version references, and hasn't been uploaded or referenced for `limits.chunk_grace_secs` (default 24 h), is deleted (§6). The grace period covers chunks uploaded in one commit and referenced by a later one. It is measured from the chunk's **newest** upload or release (keyspace.md §3.6, `cp`), so an earlier one that has aged past the grace period doesn't delete a chunk that was uploaded or released again since.

### 4.2 File keys

A version-2 manifest (formats.md §11.3) says how its chunks are sealed: under the fs keys (`key_mode 0`, as version 1), or under a 32-byte file key of its own (`key_mode 1`, formats.md §11.4). File keys only matter for sharing one file without the whole fs (milestone 11); they cost a re-upload after a rotation. So they are a policy of the fs, kept in its header (formats.md §12). These are client rules; the server never sees a key.

* **`file_keys` off (the default):** new versions use `key_mode 0`. Chunks are sealed at the fs's current epoch, and a new version reuses unchanged chunks across rotations, as version 1 does.
* **`file_keys` on:** new versions use `key_mode 1`.
  * The writer takes the key of the version it builds on (`previous`), or else of the node's newest version.
  * A node with no file key yet (new, or with only `key_mode 0` or version-1 versions) gets a fresh random key at the current epoch, and every chunk is re-sealed.
  * **After a rotation** (the key's `key_epoch` is older than the fs's current epoch), `rekey_after_rotate` decides:
    * `rewrite` (the default): keep the key for a write that builds on a previous version, so unchanged chunks are reused; a write that replaces the whole file (no `previous`) gets a fresh key.
    * `eager`: a fresh key on the next write, every chunk re-sealed and re-uploaded, so a revoked member who saw the old key can't read new content even with its chunks.
    * `keep`: never a fresh key.
* Chunks are reused only under the same key: the same file key, or both under fs keys. Switching the policy never re-keys existing files; it applies from each node's next write.
* Concurrent writers of one node may pick different keys or modes. Each version carries its own, so siblings stay readable.

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

The sweeper (every node runs it) keeps the tree bounded. It visits only trees with work left (keyspace.md §3.6, `ts`): a tree is listed by any commit with a `move` on it, and dropped once its move log, trash and tombstones are all gone, so its cost doesn't grow with the number of trees ever used.
* **Move log:** entries older than the horizon are removed. No accepted operation can need them (§3.4).
* **Margin.** "Older than the horizon" here means older than `crdt_horizon_secs` plus `crdt_max_skew_ms` by the sweeping node's clock. So a node whose clock is slightly behind never accepts an operation that needs a removed entry.
* **Trash purge.** A child of `TRASH` is purged when:
  * its move into trash is older than the horizon, and
  * every node in its subtree has a move older than the horizon.

  Purging removes the whole subtree: node records, versions, children entries. Chunk references are released, and each purged node leaves a tombstone in the change feed, plus a by-id entry (keyspace.md §3.6, `tp`) that refuses later operations on the node (§3.4) until the tombstone is dropped.

  Each round that removes nodes adds a purge record to the tree's op chain (formats.md §11.5), so `(ops, chain)` names every change of the node set (§9).

  The sweeper works through the trash in bounded rounds, each resuming where the last one stopped (keyspace.md §3.6, `tq`) and starting over after the last child of `TRASH`. So a subtree that can't be purged yet doesn't hold up the others. A subtree too large for one round is removed over several, leaves first: a node is removed only in a round that also removes all of its children, so an interrupted purge never leaves an orphan.

  The rule above holds for the whole subtree, including the part a round doesn't reach. Before removing part of a subtree, the round reads the tree's move log from the horizon on (it keeps every recent move), and walks up from each node whose current move is that recent. If one is inside the subtree, nothing of the subtree is removed. This check is bounded at 5,000 log entries and 2,000 node reads per round. While a tree has more recent moves than that, subtrees too large for one round wait. Smaller ones are still purged, because their round sees every node.

  A purge can still contradict an operation that is inside the horizon but has not arrived yet: an offline device that moved a file out of a directory before the directory was trashed, and syncs after the purge. Its operation is refused with `stale_op` rather than applied to a node whose content is gone; the client shows the file as deleted.
* **Pages:** unreferenced pages are deleted after the grace period (§4.1), and their chunk references are released. The sweeper collects pages before chunks; a chunk released by a page waits one more grace period.
* **Chunks:** unreferenced chunks are deleted after the grace period (§4.1).
* **Upload sessions:** a session that is closed or past its expiry has its references released in bounded rounds (keyspace.md §3.6, `ux`), each release an ordinary one: a GC candidate whose grace starts then. A session reference keeps a chunk or page alive like a version's.
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
  * Each trash-purge round that removes nodes extends it with a purge record (formats.md §11.5).
  * `/v1/fs/tree/chain` returns `(count, chain)`.
* **What the server could still do.** It could serve an old version as the current one, drop operations, or merge wrongly. That would deny service or show stale data, but it can't forge content. A client that replays the operations it holds detects a wrong merge.
* **Tree state.** A replica computes a hash of the node states it holds, defined below. A device names `(ops, chain, state)` of a tree in its device checkpoints (formats.md §14), taken where its replica had applied every change up to a `changes` response with `more: false`, which carries the `ops` and `chain` of that point (api.md §12). Another device at the same `ops` must hold the same `chain` and `state`; a different one, or a lower `ops` than a verified checkpoint names, means the server showed it another or an older history (formats.md §14.4).
  * **Same `ops`, same state.** Operations and purge rounds (§6) are all in the chain, and nothing else changes a node record, so honest replicas at the same `ops` hold the same nodes.
  * **When a replica checks.** Only where its node table is exactly the server's at a `changes` response with `more: false`: it has applied that response and nothing newer, and after a full sync it has also dropped the nodes the full sync didn't return.
  * **What it checks**, against the floor of verified checkpoints and its own pins: an `ops` below the floor is a **rollback**; the same `ops` with another `chain` or `state` is a **fork**.

### 9.1 Tree state

**Leaf.** Each node the change feed delivers with a state (api.md §12, `NodeState`) has a leaf; tombstones have none:

```
leaf = H(0x00 ‖ node(16) ‖ u8 flags ‖ parent(16) ‖ u64 move_hlc ‖ move_device(32)
            ‖ u64 meta_hlc ‖ meta_device(32) ‖ lp(meta) ‖ u32 versions ‖ changed(12))
H(x) = BLAKE3.derive_key("zen/v1/tree-state", x)
```

* `flags`: bit 0 = has `parent`, bit 1 = has `meta`. A field that is absent is zero bytes (`meta`: empty), as in the node record (keyspace.md §3.6).
* `meta` is the sealed meta (kind 4), byte for byte, so the leaf commits to the node's name and attributes without opening them. `changed` is the node's change offset, which moves whenever its content does.

**Fold.** A radix tree of fixed shape over node ids, 256-ary, three levels:

```
N(p), p of 2 bytes  = H(0x01 ‖ for each node whose id starts with p, in id order: node(16) ‖ leaf)
N(p), p of 0 or 1 byte = H(0x02 ‖ for each byte b in 0…255 with a node whose id starts with p ‖ b: u8 b ‖ N(p ‖ b))
state = N(empty)
```

* Empty subtrees are left out, so the empty tree's state is `H(0x02)`.
* The shape depends only on the set of nodes, so two replicas with the same nodes compute the same state, whatever order they learned them in. A change recomputes one bucket and two inner nodes of at most 256 entries each.
* The tree is a Merkle tree of a collision-resistant hash, so it needs no key, unlike a sum or XOR of leaves, which generalized-birthday attacks break.

Vectors: `test-vectors/integrity.json`.

## 10. Leakage

Compared with the plain KV store, the server additionally sees, per tree:
* **its shape:** which node is under which, children counts, depth
* **timing:** when nodes are created, moved, renamed, deleted and written, and by which device
* **content structure:** the number of chunks in each version, and which chunks versions share (dedup between versions of one file)
* **sizes:** the size of each sealed metadata value and chunk, which approximates name lengths and file sizes to 64 KiB granularity under fixed-size chunks unless the client pads; under content-defined chunks every chunk's length, and runs of equal lengths where files share content (formats.md §11.7)
* **uploads:** which chunks and pages were put under one upload session, which the device and commit timing already show

It does not see names, types, modes, times, sizes inside the manifest, or contents. Clients may add padding nodes, padding chunks or padded metadata to blur the counts.

## 11. The local replica (client)

A client may keep a **replica** of a tree on its local store (`@zen/fs`, docs/CLIENT.md; the stores in zendb.md §9.3). It is a cache of what the server holds, never a second source of truth:
* **The node table is whole.** The replica keeps every node's state as the change feed delivers it (§5): the wire `NodeState`, with its meta still sealed (kind 4). It follows the feed from a stored cursor; on `resync` it runs a full sync and then drops the nodes the full sync didn't return. A full sync interrupted by a restart starts again.
* **Content on demand.** Versions (the wire `Versions`, manifests sealed), sealed chunks and pages are fetched when read and kept, as the server stores them, in a cache bounded by size (least recently used first). **Pinned** nodes (a file, or a folder and everything under it) keep their current versions' content and are refreshed after each feed batch; their content is never evicted.
* **Paths** walk from `ROOT` by display name (§8). A file with siblings (§4) shows its newest version under its name and each other one as a read-only **conflict copy** named `stem (conflict <first 8 hex digits of the writing device>-<n>).ext`, `n` counting from 2 in descending dot order (the newest version is the first).
* **Offline**, reads use the kept node table and content; content that isn't kept fails with `offline`.
* **One writer per store.** Where several tabs or processes share a store, one of them, the owner (a Web Lock, or a lock file on a Node disk store), follows the feed and writes the node table; the others read the store and reload the nodes the owner names on the tab channel. Kept content is immutable, so any of them may add it.

### 11.1 Local writes

A replica accepts writes (mkdir, file writes, symlinks, renames, deletes, mode, mtime, xattrs) without the server, and sends them later:
* **The queue.** Each call is one **entry**: its operations, in order, with `move`/`meta` timestamps taken when the call is made, so an edit made offline keeps its place among the others. Entries are kept in the store (zendb.md §9.3) before the call returns, and survive a restart.
* **What the replica shows** is the node table with the queue applied in order: moves and meta changes rewrite the nodes they touch, and a write gives its file a **local version** (sealed content kept in the store) that hides the versions it replaces. A queued entry stays shown until it is sent and the replica has caught up with the server's state. While a batch is in flight, the feed may bring its write back before the reply gives the dot: the files it writes hide server versions newer than the cursor the replica had when the batch was first sent, until the outcome is known.
* **Writes replace the version shown** (and the file's earlier local writes, whose dots are filled in once they are known), not every version: a write made elsewhere meanwhile stays as a sibling, and both show (a conflict copy, §11). Deleting a conflict copy writes the shown version again, replacing that copy's version.
* **Content.** A write is sealed when it is made, chunks and pages as on the server, and kept in the store; only chunks whose bytes changed are sealed (the others keep their ids, §4.1). Kept content of queued writes is never evicted. Before each commit its content is uploaded (again, after a restart: uploads of identical bytes are idempotent), so a write queued longer than `chunk_grace_secs` still finds it.
* **Sending.** One sender, the owner (§11), sends entries in order, packed into commits within the limits, at most one write per node per commit. A batch's commit is stored before it is first sent and sent again, as is (the same `commit_id`, api.md §6), until its outcome is known, across restarts.
  * `stale_op` / `clock_skew`: the batch is rebased (§3.4) and its timestamps taken afresh.
  * `too_large`: the batch is split.
  * Another refusal drops the batch's entries, and the server's state shows again. A move the server skips (a cycle, §3.2) is not a refusal: the next catch-up shows where the node is.
