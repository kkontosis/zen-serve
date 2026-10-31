# Block volumes

A **volume** is a fixed-size array of 4,096-byte blocks inside an fs, stored as sealed KV values. A Merkle tree over the blocks, and a hash-chained root record, make every read verifiable against one root; the root's `seq` rides in device checkpoints (formats.md §14), so a server that serves an older volume is caught. One writer at a time holds a fenced lease (api.md §16). This is the engine behind `zen-ublk` (DESIGN.md §6): a block device whose server sees neither contents nor block indexes.

## 1. Keys

A volume is named by bytes `name` (1–64) inside an fs. Its keys are KV stored keys (formats.md §3.1), so each is a chain of PRF tokens:

```
root record   ("blk", name, "r")
block i       ("blk", name, "b", u64(i))
tree node     ("blk", name, "t", u8(level) ‖ u64(j))
lease name    the stored key of ("blk", name, "lease")      (48 bytes; api.md §16)
vol id        the stored key of ("blk", name)               (32 bytes; the checkpoint entry's id)
```

* Every value is sealed as kind 1 (formats.md §4), whose AAD binds the stored key: a block can't be moved to another index, volume or fs, only replaced by an older value of its own key.
* The server sees neither the name nor the block index, and blocks don't sort by index.
* The block size is 4,096. A volume has `blocks` blocks, fixed at creation.

## 2. Leaves and the tree

```
leaf(i)     = H("zen/v1/blk-leaf", u64(i) ‖ sealed)       sealed: the stored bytes of block i
            = 32 zero bytes when block i is absent
```

* An absent block reads as zeros.
* The leaf covers the whole sealed value, nonce included, so an older value of a block is another leaf. No per-block write counter is needed (DESIGN.md §2.5's `write_counter`).

**The tree** has fan-out 128. Its depth `D` is the least `d ≥ 1` with `128^d ≥ blocks` (a 1 TiB volume has `D = 4`). The node at level `l` (1 … D), index `j`, holds the 128 child hashes of level `l − 1`, where level 0 is the leaves:

```
node(l, j)  = child(l−1, 128j) ‖ … ‖ child(l−1, 128j + 127)        4,096 bytes; missing children are zeros
hash(l, j)  = H("zen/v1/blk-node", u8(l) ‖ u64(j) ‖ node(l, j))
            = 32 zero bytes when every child is zero
child(0, i) = leaf(i);   child(l, j) = hash(l, j)
root        = hash(D, 0)
```

* A node whose children are all zero is not stored. An empty volume stores only its root record, and storage follows what was written.
* A stored node's plaintext is `node(l, j)`, exactly 4,096 bytes.

## 3. The root record

Deterministic CBOR (zendb.md §1):

```
Root = { 1: v = 1,
         2: seq: u64 (1, 2, …),
         3: prev: bytes(32),          // RH(previous record), zeros for seq 1
         4: blocks: u64,
         5: block_size: u32 = 4096,
         6: root: bytes(32),          // hash(D, 0)
         7: writer: bytes(32),        // the fingerprint of the device that wrote it
         8: hlc: u64 }
RH   = H("zen/v1/blk-root", CBOR(Root))
```

* Creating a volume writes seq 1, with root zeros, in a commit that `expect`s the record to be absent (409 `conflict`: it exists).
* A decoder refuses another `v` or `block_size`, `blocks` 0, `seq` 0, a field missing, an unknown field, a non-canonical encoding and trailing bytes.

## 4. Writing

One writer at a time, holding the lease (api.md §16). Every commit:
* is **fenced** with the lease's token, so a writer that lost its lease writes nothing (412 `claim_lost`);
* carries the blocks it writes, or deletes for a discard; every node on their paths whose hash changed, deleting the nodes that became empty; and the next root record (`seq + 1`, `prev` = RH of the current one);
* reads nothing: the fence is what keeps other writers out.

So each commit moves the volume from one verified state to the next, atomically.

* **Flushing.** A flush returns once its commits applied. Writes not yet flushed may be lost in a crash, and when a flush needs several commits, a crash may leave a prefix of them applied, as with a disk's write cache.
* **Checkpoints.** After a commit applies, the writer's next checkpoint carries `[4, vol id, seq, RH]`, counted toward the cadence (formats.md §14.3).
* **A lost answer.** When a commit's outcome stays unknown after its replays (api.md §6), the writer re-reads the root record: its own next record means the commit applied, the previous one that it didn't. Until it can tell, it neither reads nor commits: it doesn't know which tree to check against, and another commit at the same `seq` would make two records of one `seq`. Any other record means another writer (`lease_lost`).
* **Nothing to change.** A delete of a block the tree already shows absent is left out, and a commit that would change no block isn't made: discarding blocks that were never written (as `mkfs` does to a whole device) costs nothing.

## 5. Reading

A reader holds a root record it verified (§6). A block is accepted only when its leaf matches the path of nodes from the root, each node checked against its parent's slot (and the top node against `root`). A zero slot means the whole subtree is absent.

A **mismatch** (a node or leaf that doesn't match, or a block present under a zero slot):
* The reader re-reads the root record. If it moved, the volume changed under the reader (`volume_changed`). A writer that holds the lease never sees this, since it wrote every root.
* If it didn't move, it's a **rollback** (formats.md §14.4, object `vol`).

## 6. Freshness

At open, the root record is checked against the floor of `[4, vol id]` from verified checkpoints (formats.md §14.4):
* a lower `seq` is a rollback; the same `seq` with another RH is a fork;
* at the floor's `seq + 1`, `prev` must be the floor's RH, or it's a fork;
* a missing record with a floor is a rollback.

**The writer's own pin.** A writer keeps its newest root `(seq, RH)` in a local file, written after each commit. So a rollback of its own writes is caught on the next open even before a checkpoint carries them (DESIGN.md §2.6, "single writer"): a root below the pin is a rollback, the pinned `seq` with another RH a fork, and the pinned `seq + 1` with another `prev` a fork.

**The window.** Another device sees a writer's commits as a floor only once a checkpoint carries them: between a commit and the writer's next checkpoint, a server can show other devices the older volume. `sig_every = 1` closes the window.

## 7. What the server sees

* The number of stored blocks and nodes, which of them each commit touches, when, and how often the writer flushes.
* A discard, as deletes.
* Not the indexes, the contents or the name. The fixed block size hides write lengths.

## 8. Not yet

Snapshots, clones, resizing and deleting a volume (TD-BLK-SNAPSHOTS).

Vectors: `test-vectors/blk.json`.
