# zen-db and the in-app broker

zen-db is a client-side database over the encrypted KV (DESIGN-3 §2.5). The in-app broker is a client-side message layer over topics and consumer groups (DESIGN-2 §2, DESIGN-4 §1). Both live in **one class**, `Db`, and both run inside the app's own process.

A `Db` transaction can do all of these together:
* read and write rows
* emit messages
* consume one delivered message
* schedule messages
* change files

It sends them as **one** `/v1/commit`, so everything applies or nothing does (api.md §6). This is the transactional outbox of DESIGN-2 §1, and its inbox as well.

The server is unchanged. Everything here is built from api.md §5–9 and §12. What the server lacks is listed in §18 and in TECH_DEBT.md.

Part A (§2–9) is the database. Part B (§10–13) is the broker. §14–15 cover integrity and leakage for both. §16 sketches the class API; it is **informative**, and every other section is normative. §17 plans the test vectors, which milestone 5 generates with the code. Part D (§19) adds CRDT tables, whose rows the server merges; its server side comes in milestone 5 (TD-CRDT-ROWS-SERVER).

## 1. Conventions

* **Building blocks.** Byte layouts use formats.md conventions: `u32`, `lp`, `‖`, `PRF16` (formats.md §3), `KDF`, and `H(label, x) = BLAKE3.derive_key(label, x)`.
* **CBOR.** "CBOR" means **deterministic CBOR** (RFC 8949 §4.2.1, G3). Records use small integer map keys; rows use text field names.
* **Paths.**
  * KV paths are lists of elements (formats.md §3.1, §3.2), written `("zen", "db", ns, …)`.
  * A text element is its UTF-8 bytes.
  * `u64(x)` as an element is 8 big-endian bytes.
* **Topic paths** are lists of segments (formats.md §3.3).
* **Reserved first elements.** The first KV element `"zen"` and the first topic segment `"zen"` are reserved for this spec and later zen libraries. Apps must not use them for their own data.
* **Storage.** Every stored value is a sealed kind-1 KV value (formats.md §4), so its AAD binds it to its stored key. Nothing in this spec adds a sealed kind.

---

# Part A: the database

## 2. Names, keys and secrets

### 2.1 Databases

A **database** is `(fs, ns)`, where `ns` is a text name chosen by the app. An fs can hold any number of databases. All of a database's keys sit under the KV path `("zen", "db", ns)`, written `D` below:

| KV path | Value (sealed CBOR) | What |
|---|---|---|
| `D ‖ ("cat", "db")` | DbRecord (§3.1) | format, schema version, integrity level |
| `D ‖ ("cat", "t", table_name)` | TableRecord (§3.2) | one per table: id, primary key, indexes, change topic |
| `D ‖ ("cat", "m", u64(step))` | MigrationRecord (§8) | the migration log |
| `D ‖ ("t", table_id, pk)` | Row (§4) | a row |
| `D ‖ ("o", table_id, pk, u32(i))` | raw bytes | part `i` of a row too large for one value (§4.2) |
| `D ‖ ("u", index_id, value)` | `{1: pk}` | unique-index entry (§5.2) |
| `D ‖ ("f", index_id, value, pk)` | `{1: pk}` | fast-index entry (§5.3) |
| `D ‖ ("r", index_id, u32(shard))` | RootRecord (§5.4) | root of a private index tree |
| `D ‖ ("n", index_id, node_id)` | Node (§5.4) | node of a private index tree |
| `D ‖ ("s", index_id)`, `D ‖ ("s", index_id, u32(i))` | SealedHead, raw bytes | a sealed index: its head and parts (§5.7) |
| `D ‖ ("p", index_id)`, `D ‖ ("p", index_id, u32(i))` | OHead, stash slots | an oblivious index: its head and stash (§5.8) |
| `D ‖ ("q", index_id, u32(bucket))` | Z slots | an oblivious index: one ORAM bucket (§5.8) |
| `D ‖ ("m", msg_id, u32(i))` | raw bytes | part `i` of a large message body (§10.3) |

* `table_id` and `index_id` are random 16-byte ids, assigned when the table or index is created. So renaming a table touches only the catalog, and a dropped-and-recreated table never meets its old keys.
* `pk` is the **pk element**, the CBOR encoding of the primary-key value (§4.1).
* `value` is the CBOR encoding of the indexed values (§5.1): an array with one item per indexed field.
* The server sees only PRF tokens of these elements (formats.md §3.2): 16 bytes each, one-way. So every value that needs a name or key back stores it inside (DESIGN-3 §2.2).
* **Key order is random.** A range over `D ‖ ("t", table_id)` returns the rows in **stored-key order**, which is random. Ordered reads need a private index (§5.4).

### 2.2 Secrets

Two secrets are derived per database from the fs naming key NK (formats.md §2):

```
K_db       = KDF("zen/v1/db", NK, u32(fs) ‖ lp(ns))
K_boundary = KDF("zen/v1/db-boundary", K_db, index_id)       (§5.4, per private index)
K_node     = KDF("zen/v1/db-node-id",  K_db, index_id)       (§5.4, per private index)
```

* They come from NK, not from an epoch key. Like key tokens they must stay stable across epoch rotation (G2): a private index can't be rebuilt on every revocation.
* A revoked member who kept NK can still compute them. That is the same exposure as key tokens (§15).

### 2.3 Topic paths used by the class

| Topic path | Use |
|---|---|
| `("zen", "db", ns, "changes", table_name)` | default change topic of a table (§12.7) |
| `("zen", "db", ns, "sched")` | the scheduler's wake and lease topic (§12.4) |
| `("zen", "inbox", instance)` | reply inbox of one app instance (§12.3); `instance` is random, 16 bytes |
| `("zen", "saga", ns, saga_name)` | replies and timeouts of one saga definition (§12.5) |

Apps choose every other topic path.

## 3. Catalog (G6)

The catalog is the set of `("cat", …)` keys. Clients cache it with each record's version. Every transaction that writes a table puts that table's TableRecord into its read set (§7.4). So a schema change and a concurrent write can't both commit on different schemas.

### 3.1 DbRecord

```
DbRecord = { 1: format = 1,
             2: schema: u32,                  // the app's schema version, 0 for a new database
             3: integrity: "basic",           // "authenticated" is reserved for milestone 6 (§14)
             4: created_hlc: u64 }
```

* **Opening a database:** the client reads the DbRecord and compares it with what it knows:
  * an unknown `format` → refuse (G4)
  * a `schema` newer than the app's → refuse with `schema_newer`
  * an older `schema` → migrate (§8)
  * no DbRecord → create one at schema 0, then migrate
* An `integrity` other than `"basic"` is refused until milestone 6.

### 3.2 TableRecord and IndexDef

```
TableRecord = { 1: name: text,
                2: id: bytes(16),
                3: pk: [text],                   // primary-key field names, ≥ 1
                4: indexes: [IndexDef],
                5: changes?: ChangeDef,          // §12.7
                6: pad: bool,                    // §4.3
                7: state: "active" | "dropping",
                8?: merge: "txn" | "crdt",       // default "txn"; "crdt": a CRDT table (§19)
                9?: crdt_fields: {text: "lww" | "counter" | "set"} }   // CRDT tables: field types, default "lww"

IndexDef    = { 1: name: text,
                2: id: bytes(16),
                3: fields: [[name: text, type: Type, desc: bool]],
                4: kind: "private" | "fast" | "none" | "sealed" | "oblivious",
                5: unique: bool,
                6: state: "building" | "active" | "dropping",
                7: fanout?: u32,                 // private: target entries per node, default 64
                8: shards?: u32,                 // private: number of trees, default 1 (§5.4.5)
                9: built_to?: bytes,             // building: the stored key the backfill reached (§8.2)
                10: decoys?: u32,                // private: decoy leaves re-salted per write, default 0 (§5.4.6)
                11: max_bytes?: u32,             // sealed: size cap of the index, default 1,048,576 (§5.7)
                12: blocks?: u32 }               // oblivious: ORAM capacity in blocks, a power of two (§5.8)

Type        = "text" | "bytes" | "int" | "float" | "bool"
ChangeDef   = { 1: topic: [bytes], 2: image: "keys" | "full" }
```

* **Mutability.** An index's `fields`, `kind`, `unique`, `fanout`, `shards`, `decoys`, `max_bytes` and `blocks` never change. To change them, build a new index and drop the old one.
* **Index kinds.**
  * `kind: "none"` with `unique: true` is an index used only for uniqueness and equality lookups through its unique entries.
  * `kind: "none"` with `unique: false` is invalid.
  * `kind: "sealed"` and `kind: "oblivious"` with `unique: true` check uniqueness inside the index itself, with no unique entries (§5.7, §5.8).
* **Visibility by state.**
  * Writers maintain every index whose state is `building` or `active`.
  * Queries use only `active` indexes.

## 4. Rows

### 4.1 Encoding

A row is a CBOR map of text field names to values:
* allowed values: `null`, booleans, integers (−2^63 … 2^64−1), floats (64-bit), text, bytes, arrays and maps
* dates: integers of unix milliseconds, by convention

The **primary key** is the value of the pk fields, which must be present and must not be null. With one pk field the key is that field's value; with several it is the array of their values. The **pk element** is the CBOR encoding of the key.

```
Row = { 1: pk,                 // the primary-key value, as above
        2: fields: map,        // every field, the pk fields included
        3?: parts: u32,        // §4.2
        4?: digest: bytes(32), // §4.2
        5?: pad: bytes }       // §4.3
```

`Row` is the plaintext of the sealed value at `D ‖ ("t", table_id, pk)`.

### 4.2 Large rows

A sealed value holds at most `max_value_bytes` − 48 bytes of plaintext (api.md §2, formats.md §4). When the CBOR of `fields` is larger than `max_value_bytes − 1024`:
* `F` = that CBOR is cut into parts of at most `max_value_bytes − 1024` bytes each.
* Each part `i` is stored at `D ‖ ("o", table_id, pk, u32(i))`.
* The Row itself holds `{1: pk, 2: {}, 3: parts, 4: H("zen/v1/db-parts-digest", F)}`.
* Readers fetch all parts at the same read version and check the digest. A mismatch is reported as a corrupt row, never returned.
* Writing or deleting the row writes or deletes all its parts in the same commit. A rewrite with fewer parts deletes the excess ones.

The size of a row is bounded by the commit: `max_commit_bytes` (8 MB by default) minus the rest of the transaction.

### 4.3 Padding

A table with `pad: true` pads every Row with `5: pad` (zero bytes) so its encoded size is the smallest bucket that fits:
* the buckets are 256 B, 1 KiB, 4 KiB, 16 KiB, then the multiples of 16 KiB
* rows with parts are padded as a Row, and their last part to 16 KiB

This hides row sizes within a bucket (DESIGN-3 §2.3).

### 4.4 Operations

| Operation | Reads | Writes |
|---|---|---|
| `get(pk)` | the row (and its parts) | – |
| `insert(row)` | the row: it must be absent (`exists` otherwise) | row, parts, index entries (§5.5) |
| `put(row)` | the old row, if any | row, parts, index changes from the old row to the new one |
| `update(pk, fn)` | the row: it must exist (`not_found` otherwise) | as `put` |
| `delete(pk)` | the old row | deletes the row, its parts and its index entries |

* Every write reads the old row first: index maintenance needs the old values, and the read puts the row into the read set.
* A single operation outside an explicit transaction is a transaction of its own (§7).

## 5. Indexes

### 5.1 Indexed values and sort keys

An index has one or more fields. A row's **indexed value** is the array of those fields' values, with `null` for a missing field. A value whose type differs from the declared `Type` is refused at write time (`bad_type`); an `int` field also takes integers in float form with no fraction.

Unique and fast indexes use the indexed value's **CBOR** as a KV element (§2.1), so they compare values only for equality.

Private indexes need order, so they use an order-preserving **sort key**. For each field, its encoding below is concatenated; with `desc: true`, that field's bytes are inverted (each byte XOR 0xFF). Then the sort key of the pk is appended (encoded the same way, ascending, one component per pk field). So entries are unique and ties break by pk.

| Value | Encoding |
|---|---|
| null | `0x00` |
| false / true | `0x10` / `0x11` |
| int (i64 range) | `0x20 ‖ u64(x XOR 2^63)` |
| int above 2^63−1 | `0x21 ‖ u64(x)` |
| float | `0x28 ‖ u64(bits XOR 2^63)` if the sign bit is 0, else `0x28 ‖ u64(NOT bits)`; NaN is refused |
| text | `0x30 ‖ escape(UTF-8) ‖ 0x00` |
| bytes | `0x40 ‖ escape(bytes) ‖ 0x00` |

* `escape` replaces each `0x00` with `0x00 0xFF`, so the encoding is prefix-free and byte order equals value order.
* Text compares by code point (binary UTF-8 order). There is no collation. An app wanting case-insensitive order stores and indexes a normalized copy of the field.
* A pk field of a type outside this table (an array or a map) can't be used in a table that has a private index.

### 5.2 Unique

* **Entry:** for every row whose indexed value contains no null, `D ‖ ("u", index_id, value) → {1: pk}`.
* **Insert:** reading the entry must find it absent, or holding the same pk. Otherwise the write fails with `unique_violation`.
  * In short mode the read is a read conflict.
  * In long mode it is an `expect` with version null.
* **Change of value:** a write deletes the old entry and writes the new one.
* **Equality lookup:** one `get` of the entry, then one of the row.
* **Leakage:** the server sees one key per row and index, and the token of a value. It can recognize a value that comes back after being freed. It learns nothing else, since values are unique (§15).

### 5.3 Fast

* **Entry:** for every row whose indexed value contains no null, `D ‖ ("f", index_id, value, pk) → {1: pk}`.
* **Equality lookup:** one range read of `D ‖ ("f", index_id, value)`, then the rows.
* **Leakage:** the server learns **how many rows share each value**, and when that count changes (DESIGN-3 §2.5). Fast indexes are opt-in, and the API documents this.

### 5.4 Private: a prolly tree

A private index is a **prolly tree**: a search tree whose node boundaries are chosen by a keyed hash of the entries. The same set of entries always gives the same tree, whatever order the writes came in (history independence).

#### 5.4.1 The canonical tree

The tree for a set of entries is defined level by level, so two clients with the same entries build byte-identical plaintext nodes:

1. **Level 0.** The entries are the sort keys (§5.1) of every row, in byte order. Rows with null indexed values are included.
2. **Boundaries.** Cut a level into nodes, left to right. An entry `e` at level `L` **ends a node** when either:
   * `u32_be(PRF16(K_boundary, u8(L) ‖ lp(key(e)))[0..4]) < 2^32 / fanout`, or
   * the node would otherwise reach 1,024 entries or 60,000 bytes of encoded Node.
   
   The last entry of a level always ends a node.
3. **Levels above.** Level `L+1` has one entry per node of level `L`: `[first_key, child_id, count]`. Here `first_key` is the key of the node's first entry, and `count` is the number of level-0 entries under it.
4. **Root.** The first level with exactly one node is the root. An empty index has no root.

`key(e)` is the entry itself at level 0, and `first_key` above it.

#### 5.4.2 Nodes and ids

```
Node       = { 1: level: u8,
               2: entries: [bytes] (level 0) | [[first_key: bytes, child: bytes(16), count: u64]] (above),
               3?: salt: bytes(16),     // only with decoys (§5.4.6)
               4?: pad: bytes }         // only with decoys: zero bytes up to the node size bucket
node_id    = PRF16(K_node, CBOR(Node))
RootRecord = { 1: root: bytes(16), 2: height: u8, 3: count: u64 }
```

* A node is stored at `D ‖ ("n", index_id, node_id)`.
* Its id is a keyed hash of its content, so a node never changes once written: a different content is a different node.
* In one tree a given content appears at most once. Every entry occurs once, and a node's content fixes its position.
* The root is at `D ‖ ("r", index_id, u32(shard))`, absent when the shard is empty.

#### 5.4.3 Reading

* A reader reads the RootRecord, then descends to the nodes covering its key range.
* Nodes are immutable, so they are cached by id with no expiry, and the reader needs **no conflict range and no `expect` on nodes**. The RootRecord alone carries the snapshot:
  * in short mode it is a read conflict
  * in long mode it is an `expect`
* Every node reachable from a given root belongs to that root's tree. So a read that uses cached nodes is still a consistent snapshot of the root it started from.
* **A missing node** is fetched at the reader's read version. If it is absent, the root has changed since: the read restarts from a fresh RootRecord. Outside a transaction it restarts at once; inside one, the restart retries the transaction.
* **Cost:** about `height` node reads per lookup, minus the cached upper levels, plus one read per leaf scanned. With the default fanout of 64, 10^6 rows give a height of about 4.

#### 5.4.4 Writing

* A transaction that changes entries computes the new canonical tree from the old one. In practice it re-chunks the touched leaves until a boundary lines up with an old one, then repeats the same at each level up.
* It then, in the same commit:
  * writes every node of the new tree that the old one lacks
  * writes the RootRecord, or deletes it if the shard is now empty
  * deletes every node of the old tree that the new one lacks
* The RootRecord is in the read set, so this commit applies only if the old tree is still current. The deleted nodes are then unreachable from any root.
* **Concurrent readers.** A reader still descending an older root may find a node deleted. It restarts (§5.4.3).
* **Cost per changed entry:** about `height` new nodes, `height` deleted nodes and the root.

#### 5.4.5 Contention and sharding

Every writer of a shard rewrites its RootRecord. So **concurrent writers of one private index conflict**, and all but one retry, even when their entries are far apart. That is the price of a structure that hides values from the server.

* An index with `shards: K > 1` keeps K independent trees and puts each entry into shard `u32_be(PRF16(K_boundary, 0xFF ‖ lp(pk element))[0..4]) mod K`.
  * Writers of different shards don't conflict.
  * A range query reads all K roots and merges the K ordered streams.
* `shards` is fixed when the index is created. Choose it for the expected number of concurrent writers. The default of 1 suits a family-sized app.

#### 5.4.6 Decoy rewrites

A private index with `decoys: k > 0` blurs which leaves a write touches (the locality leak, §15). Every commit that changes the tree also **re-salts** `k` other leaves:

* **Choosing them.** Pick a uniformly random level-0 entry by descending with the `count` fields. Its leaf gets a fresh random `salt`. Its id changes, and so do the ids of its ancestors up to the root, exactly as for a real change.
* **Padding.** With decoys, every Node is padded with `4: pad` to the smallest size bucket that fits (1 KiB, 2 KiB, 4 KiB, then multiples of 4 KiB). Otherwise a changed entry count shows in the ciphertext size and tells a real leaf from a decoy.
* **Canonical tree.** The tree stays canonical in its entries and boundaries (§5.4.1), but node ids are no longer history-independent. Test vectors use unsalted nodes.
* **Cost:** about `k × height` more node writes and deletes per commit.
* **What it doesn't hide.**
  * Splits and merges still change the node count.
  * Reads still show which leaves a query visits.
  * Over many writes a real hot region still stands out statistically.
  
  Decoys raise the cost of the analysis; they don't remove the leak. For that, use `sealed` or `oblivious`.

### 5.5 Maintenance on write

For each index in state `building` or `active`, a write compares the old row's indexed value with the new one:
* **Unchanged:** nothing.
* **Changed:**
  * unique: delete the old entry, check and write the new one (§5.2)
  * fast: delete the old entry, write the new one
  * private: remove the old sort key and add the new one in that index's tree (§5.4.4)
* **Insert:** add the new entries. **Delete:** remove the old ones.

Several changes to one private index in one transaction are applied to the tree together, at commit time. So a batch costs one rewrite of each touched path, not one per row.

### 5.6 Choosing an index

| Need | Index | Leaks |
|---|---|---|
| lookup by a unique value (email, slot) | `unique: true, kind: "none"` | existence of the value only |
| equality on a frequent value, fast | `kind: "fast"` | how many rows share each value |
| equality, ranges, ordering, paging | `kind: "private"` (default) | tree shape and write locality (§15) |
| the same, with locality blurred | `kind: "private", decoys: k` | as above, statistically weaker |
| the same for a small index, no locality leak | `kind: "sealed"` (§5.7) | the index's size class; every write rewrites it whole |
| the same for a larger index, no access-pattern leak | `kind: "oblivious"` (§5.8) | the capacity and the number of accesses; every access, even a read, rewrites ORAM paths |

### 5.7 Sealed: the whole index in one blob

A `sealed` index stores its entire entry list as one sealed blob, rewritten on every change. The server sees neither order nor locality, only how big the index is.

* **Content.** The entries are the sort keys of §5.1, in order: `Blob = { 1: entries: [bytes] }`. Its CBOR is cut into parts of at most `max_value_bytes − 1024` bytes.
* **Padding.** The number of parts is padded up to a power of two with parts of zero bytes. So the server learns only a size class.
* **Layout:**

  | KV path | Value |
  |---|---|
  | `D ‖ ("s", index_id)` | `SealedHead = {1: parts: u32, 2: digest: bytes(32), 3: count: u64}`, `digest = H("zen/v1/db-parts-digest", CBOR(Blob))` |
  | `D ‖ ("s", index_id, u32(i))` | part `i` |

* **Writing.** A transaction that changes the index reads the head (into its read set), applies its changes to the entry list and writes every part and the head. A change that would exceed `max_bytes` fails with `too_large`; the app then moves to `oblivious` or `private`.
* **Reading.** A reader reads the head, then all parts, and checks the digest.
  * The head's version validates a cached copy (§7.7), so an unchanged index costs one `get`.
  * Every query reads the whole index, so the server can't tell queries apart.
* **Unique** is checked against the entry list. The head in the read set makes that serializable, and no unique entries (§5.2) are written.
* **Contention.** Every writer conflicts on the head, as with an unsharded private index.
* **Cost:** each write uploads the whole index. With the default `max_bytes` of 1 MiB, that suits up to roughly 10,000–20,000 short entries.

### 5.8 Oblivious: a B+tree inside Path ORAM

An `oblivious` index hides **which entries any access touches**, for reads as well as writes. It is a B+tree whose nodes are stored in a Path ORAM (Stefanov et al., "Path ORAM", CCS 2013). Parent nodes hold their children's positions, as in Wang et al., "Oblivious Data Structures" (CCS 2014), so no separate position map is needed.

#### 5.8.1 Layout

* **Parameters:**
  * block size `B = 2,048` bytes of plaintext
  * bucket size `Z = 4` blocks
  * `blocks` (a power of two, fixed at creation) blocks of capacity
  * a tree of `L = log2(blocks)` levels below its root: `2^L` leaves and `2^(L+1) − 1` buckets
* **Bucket** `b` (heap order, root = 1) is at `D ‖ ("q", index_id, u32(b))`. It holds `Z` slots, each a Block or a dummy, padded to exactly `B` bytes. So every bucket's ciphertext has the same size.

  ```
  Block = { 1: id: bytes(16),        // random, fixed for the node's life
            2: leaf: u32,            // the ORAM leaf the block is mapped to
            3: node: ONode }
  ONode = { 1: level: u8,
            2: entries: [bytes] (level 0) | [[first_key: bytes, child: bytes(16), child_leaf: u32, count: u64]] (above) }
  ```

  A dummy slot is a Block with an all-zero `id`.
* **Head** `D ‖ ("p", index_id)`:

  ```
  OHead = { 1: root: bytes(16), 2: root_leaf: u32, 3: height: u8, 4: count: u64 }
  ```

* **Stash** at `D ‖ ("p", index_id, u32(i))` for `i` in `0..2`: 64 slots in total, 32 per value, padded like buckets. It holds blocks that couldn't be written back yet.
  * The stash is shared by every client, so it is stored, not kept in memory.
  * A stash that would need more than 64 blocks fails the access with `oram_overflow`; the index must then be rebuilt. With `Z = 4` the probability is negligible (about 14 · 0.6^64).
* **Node size.** ONodes are B+tree nodes that fit one block. Leaves split when full. Deletes don't rebalance: a node may become underfull, and an empty leaf is removed from its parent.

#### 5.8.2 An access

One **operation** is a lookup, an insert, a delete, or a page of a range scan. Each one performs exactly `A` path accesses, where `A = height_max + 2` and `height_max = ⌈log_{fanout/2}(blocks)⌉ + 1`.

1. Read the head and the stash.
2. **Descend.** For each node on the way from the root:
   1. read every bucket on the path from the ORAM root to the node's leaf
   2. move the real blocks found into the in-memory stash
   3. take the node out of the stash, assign it a fresh uniformly random leaf, and record that leaf in its parent (or in the head, for the root)
3. **Pad.** If the operation needed fewer than `A` path accesses, read random paths until it has done `A`.
4. **Change.** Change the leaf node: insert or delete the entry, split it if needed.
   * New nodes from splits get random ids and leaves, and enter the stash.
   * A range-scan page reads up to `A − height` consecutive leaves, by re-descending; that is part of the `A` accesses.
5. **Evict.** For each path read, from the leaf bucket up, fill its buckets with the stash blocks that can go deepest (Path ORAM's greedy eviction). Fill the rest with dummies.
6. **Commit** in one commit: the `A` paths' buckets, the head, the stash values, plus the row writes of the same transaction. The head is in the read set.

* **Reads write too.** A lookup also remaps and rewrites paths, so every operation commits, and operations on one index serialize on its head. Concurrent operations conflict and retry, so throughput is about one operation per commit round trip.
* **Many readers.** An app with many readers can route them through one instance, for example the database's owner (§9.2) through request/reply (§12.3).
* **Unique** is checked by the insert's own lookup. No unique entries (§5.2) are written.
* **Cost per operation:** `A · (L + 1)` bucket reads and writes of 8 KiB each, plus the head and the stash. For example, `blocks = 2,048` and `height_max = 4` give 6 · 12 = 72 buckets, about 576 KiB each way.

#### 5.8.3 What it hides and what it doesn't

The server sees the same thing for every operation:
* `A` uniformly random paths read
* the same buckets rewritten
* fixed-size ciphertexts

It doesn't learn which entries, positions or neighbours were involved, or whether the operation was a read or a write. It does learn:
* the capacity (`blocks`) and the number and timing of operations
* the rows a transaction then reads or writes by their pk, which the index can't hide. An oblivious index hides **order and locality**; the row accesses still show **which rows**.

### 5.9 Unchanged rules for the new kinds

* **`sealed` and `oblivious`** follow §5.5 for maintenance and §8.2 for building. A backfill page applies its changes to the blob, or performs its ORAM operations, in the page's transaction.
* **Queries (§6)** use them where §6.1 would use a private index.

## 6. Queries

```
query(table)
  .where(field, op, value)          // op: "=", "<", "<=", ">", ">=", "prefix", "in"
  .orderBy(field, "asc" | "desc")
  .limit(n)
  .after(cursor)                    // paging
  → rows, cursor?
```

### 6.1 Plan

The client picks one access path, in this order:
1. **pk equality:** the row's key.
2. **unique equality:** an index whose fields are all bound by `=`.
3. **fast equality:** same rule as 2.
4. **private range:** a private index whose leading fields are bound by `=` and whose next field carries a range or the `orderBy`.
5. **table scan:** a range over `D ‖ ("t", table_id)`, then a client-side filter. The library logs a warning. In long mode a scan must fit one response (`max_range_items`).

* The other predicates are applied to the fetched rows on the client.
* An `orderBy` that no chosen private index provides is applied on the client. It is refused if the result could exceed the query's `limit` (`needs_index`), because pk order is random.
* There are no joins. The examples (docs/EXAMPLES.md) join in the app.

### 6.2 Paging

A cursor carries the last entry seen (a sort key, or a pk for scans). The next page continues strictly after it. A cursor contains plaintext values, so it stays inside the client; it must never be sent to the server or put in a URL.

### 6.3 Isolation

A query inside a transaction is as serializable as the transaction's reads:
* **private index:** its RootRecord is in the read set, so **any** concurrent write to that index makes the transaction retry, not only writes inside its range. That is coarse but phantom-safe.
* **fast:** the `("f", index_id, value)` range.
* **scan:** the table range.
* **unique and pk lookups:** single keys.

## 7. Transactions

### 7.1 Modes

A zen-db transaction is an M4 `Transaction` (packages/client `kv.ts`, DESIGN-3 §2.4) with zen-db's reads and writes on top:
* **`short` (default):** reads at one read version, with read conflicts. Serializable, phantoms included. It must commit within about 5 s.
* **`long`:** every read becomes an `expect` (a version or absence). Ranges become `expect_ranges` hashes, so each range must fit one response (`max_range_items`, api.md §6). There is no time limit.
* **`auto`:**
  1. Run as `short`.
  2. If the function is still running after `auto_switch_ms` (default 3,000), or the commit returns `too_old`, re-run the whole function as `long`.
  3. If the long run hits a range too large for long mode, fail with `too_large`. Such work must be split, or done in short transactions.

The function may run several times (retries, `auto`), so **it must have no side effects** other than through the transaction. Retries follow api.md §1: `conflict` and `too_old` re-run the function, up to `attempts` (default 8), with jittered backoff.

### 7.2 Manual transactions

`begin({mode, consumes?})` returns a transaction that the app commits or aborts itself. It has no automatic retry: `conflict` reaches the caller. `consumes` binds a delivered message (§11.3).

### 7.3 What a commit carries

| Transaction content | Commit field (api.md §6) |
|---|---|
| row, part, index entry, node and root writes | `writes` |
| reads (short) | `read_version`, `read_conflicts` |
| reads (long) | `expect`, `expect_ranges` |
| table drops (§8.3) | `clear_ranges` |
| emitted, scheduled-wake and change messages | `append` |
| the bound delivery's consume step | `consume` |
| file chunks and file operations (§7.6) | `chunks`, `crdt_ops` |

The commit's random `commit_id` makes it idempotent (api.md §6, step 1). M4's `sendCommit` replays it after a network error or `commit_unknown`.

### 7.4 What every write transaction reads

* the TableRecord of every table it writes, so that schema changes serialize with writes (§3)
* the old row of every row it writes (§4.4)
* the unique entries it checks (§5.2)
* the RootRecord of every private-index shard it changes (§5.4.4)

### 7.5 Limits

The client checks a commit against `/v1/info` limits before sending it, and fails early with `too_large`.

* **Op count.** A row write costs about:
  * 1 (the row), plus its parts
  * 2 per changed unique or fast index
  * `2 × height + 1` per changed private index, shared across the rows of one commit
* **Typical capacity.** With `max_commit_ops` 10,000, a commit holds a few hundred to a few thousand row writes.
* **Bulk loads.** `importRows` splits a bulk load into several commits. Each commit is atomic; the load as a whole is not.

### 7.6 Files in a transaction

A transaction can carry filesystem operations (spec/fs.md) next to its rows. They go into `crdt_ops`, and chunks into `chunks`, in the same commit.

* **Large files.** A file larger than one commit uploads its chunks in earlier commits; they survive `chunk_grace_secs` unreferenced (fs.md §6). The transaction then carries only the `write` operation that references them.
* **Rules from fs.md:**
  * one `write` per node per commit
  * in short mode, CRDT operations conflict like reads (fs.md §7, G11)
  * `stale_op` and `clock_skew` are handled by the rebase of fs.md §3.4, inside the transaction's retry
* **Status in M4:** its tree API sends its own commits (`Tree.send`). Milestone 5 adds a variant that adds operations to a transaction.

### 7.7 Cache coherence (G9)

* **Catalog records and rows** may be cached with their version. A transaction that uses a cached record puts its version into the read set (short: a conflict range on the key; long: an `expect`). A stale cache therefore causes a retry, never a wrong commit.
* **Private-index nodes** never go stale (§5.4.3).
* **Freshness outside transactions.** A table with a change topic (§12.7) can keep its cache fresh by subscribing to it. Without one, cached rows are fresh only as of when they were read.

## 8. Migrations (G6)

### 8.1 Schema steps

* **The app declares** its schema version `N` and, for each step `k` in `1..N`, a migration function. The functions use these operations:
  * `createTable`, `dropTable`, `renameTable`
  * `createIndex`, `dropIndex`
  * `transform(table, fn)`: rewrite every row, in pages
* **Opening.** On open with a stored schema `s < N`, the client runs steps `s+1 … N` in order.
* **Logging.** Each step writes a MigrationRecord at `D ‖ ("cat", "m", u64(k))`:

  ```
  MigrationRecord = { 1: step: u32, 2: state: "running" | "done", 3: progress?: bytes, 4: hlc: u64 }
  ```

* **Resuming.** A step is resumable: `progress` records how far a paged operation got, and a client that finds a `running` step continues it.
* **Finishing.** The step's last transaction sets the DbRecord's `schema` to `k` and the record to `done`.
* **Duplicate runners.** Two clients may run the same step at once. Every page is a transaction that reads the record it advances, so their pages conflict and only one advances each page; the cost is duplicate work, nothing worse. To avoid even that, a client may first take the scheduler leader of the database (§12.4) and run migrations while it holds it.

### 8.2 Building an index

1. Add the IndexDef in state `building`, with no `built_to`. From the next commit on, every writer maintains the index (§5.5), because it reads the TableRecord.
2. Backfill: scan `D ‖ ("t", table_id)` in pages of stored-key order.
   * Each page is a transaction that reads its rows and adds their entries.
   * It also sets `built_to` to the last stored key of the page.
   * Adding an entry that is already present is a no-op. A row that a writer changed concurrently conflicts with the page, and the page retries.
3. **Unique indexes.** A duplicate found during the backfill stops the build: the IndexDef stays `building` with the duplicates reported. The app fixes the data or drops the index.
4. When the scan ends, set the state to `active` in one transaction.

### 8.3 Dropping

* **Dropping an index or table** starts by setting its state to `dropping`. Writers then stop maintaining it, and queries stop using it.
* **Clearing.** Its keys are cleared in pages, each one `clear_ranges` of at most `max_range_items` keys (api.md §6):
  * `("u"|"f"|"n"|"r", index_id)` for an index
  * `("t"|"o", table_id)` plus its indexes for a table
* **Finishing.** The definition is removed in the last page's transaction.

## 9. Multiple tabs and processes (G16)

### 9.1 Who holds a lease

Leases and claims are held by a **device**: the server compares the session's device fingerprint (api.md §8.2). The token is what makes a lease exclusive, though.
* A second process of the same device that doesn't know the token is refused (`the lease is held`).
* Two processes of one device therefore can't both lead. But the server can't tell them apart.
* Node workers may each use a device key of their own, so leases and logs name them separately.

### 9.2 Browser tabs

In a browser, every tab of one origin is the same device and shares the same caches. One tab is the **owner**: it holds the Web Lock `zen/db/<fs>/<ns>` (`navigator.locks`, exclusive).

* The owner runs everything that holds state on the server:
  * consumers
  * the scheduler leader
  * migrations
  * the stream connection
* **Other tabs** send it their transactions and subscriptions over a `BroadcastChannel`. Where `SharedWorker` exists, the owner is a shared worker instead of a tab.
* **When the owner closes,** its lock is released and the next tab takes over:
  * it reopens the stream from the last offsets it has seen (§11.2)
  * it waits for any old lease to expire, or re-campaigns (api.md §8.2)
* Without Web Locks every tab is its own owner. That is correct, but it duplicates connections and wastes campaigns.

**Transactions stay safe in every case**: they are serializable on the server. The owner only avoids wasted work and duplicate connections.

### 9.3 Local state

A database keeps no plaintext on disk by default. Caches are in memory. A persistent cache, if added, is encrypted at rest with a non-extractable WebCrypto key (DESIGN-2 §4), and milestone 5 decides on it.

---

# Part B: the broker

## 10. Messages

### 10.1 Encoding

A message is an event (formats.md §5) whose `payload` is the CBOR of:

```
Msg = { 1: type: text,                   // app-defined, e.g. "order.paid"; "zen." prefixes are reserved
        2: id: bytes(16),                // random, chosen by the sender before commit
        3?: corr: bytes(16),             // correlation id (§12.3)
        4?: reply_to: [bytes],           // topic path of the reply inbox (§12.3)
        5?: deliver_at: u64,             // unix ms, set on scheduled messages (§12.4)
        6?: saga: bytes(16),             // saga instance (§12.5)
        7?: step: text,                  // saga step (§12.5)
        8?: headers: {text: text},       // app metadata
        9?: body: any,                   // the content, any CBOR value
        10?: body_ref: BodyRef }         // instead of 9, for large bodies (§10.3)
```

* **Who sent it.** The event body around it carries the sending device (`sender_fp`) and an HLC (formats.md §5). `sender_fp` is authenticated only as "written by a member": any member holding the topic key can seal any value there (§14).
* **Optional key.** A message may carry an event key (DESIGN-4 §1.1), given at emit time. Its token orders the message within `per_key` and `partitioned` groups.
* **Ids.** `id` is the message's identity across redeliveries. A DLQ retry re-appends the same envelope, so it has the same `id` and a new offset (api.md §8.4). Use `id`, not the offset, as the idempotency key of external effects (§12.6).

### 10.2 Causation

An event's id is `topic_id ‖ offset` (16·n + 12 bytes).
* **Filled automatically.** A transaction bound to a delivery (§11.3) fills the `causation` of every event it appends with the consumed event's id.
* **Other events** have an empty causation.

So any chain of handled messages can be followed back to its first cause.

### 10.3 Large bodies

* **When it applies.** The CBOR of `body` can make the sealed envelope exceed `max_envelope_bytes`. The body is then:
  * cut into parts stored at `D ‖ ("m", id, u32(i))` of the sender's database, in the same commit as the append
  * referenced by `body_ref`:

  ```
  BodyRef = { 1: ns: text, 2: parts: u32, 3: digest: bytes(32) }    // digest = H("zen/v1/db-parts-digest", CBOR(body))
  ```

* **Reading.** A reader fetches the parts (it needs fs `read`) and checks the digest.
* **Deleting.** The parts are not deleted with the event: the log keeps every event (TD-LOG-RETENTION). The sender gives the message a `ttl`, and the scheduler deletes the parts after it (§12.4, kind `gc`). A consumer that reads the parts later finds them gone and treats the message as poisoned.

## 11. Broker primitives

### 11.1 Emit

`emit(topic, type, body, {key?, id?, headers?})` seals a Msg as an event and appends it:
* **Inside a transaction** it is one entry of the commit's `append` (M4 `Topic.appendIn`). The message exists only if the transaction commits, and only once, even across `commit_id` replays (api.md §6).
* **Outside a transaction** it is a commit of its own (`/v1/log/append`).

### 11.2 Subscribe: `on`

`on(topic | prefix, handler, opts)` is a stream subscription (api.md §9, M4 `Stream.subscribe`). Every member reads every event; no group is involved. Where it starts:

| `opts` | Starts at | Survives a restart |
|---|---|---|
| none | the read version when subscribing: new events only | no |
| `after: offset` | after the given offset, history first, then live (G9) | if the app stores the offset |
| `cursor: name` | after the offset stored in the system table `$cursors` (§11.5) under `(device_fp, name)` | yes |

* **Plain handlers.** Without `cursor` the handler gets the message only. The stream resumes after the last offset it delivered across reconnects (M4), so a running process misses nothing and sees nothing twice.
* **Durable handlers.** With `cursor`, the handler is `handler(msg, tx)`. It runs in a transaction that also reads and advances the cursor row, and skips events at or before it. So the handler's database effects happen **exactly once per device and cursor name**, even across crashes.
  * Events may be batched: one transaction per batch of up to `batch` events (default 1).
* **Prefix subscriptions** deliver the events of every topic under a prefix. Events of topics the class can't open, because their path isn't known, arrive unopened (M4).

### 11.3 Consume

`consume({group, topic, mode, partitions?, key?, maxInflight?, maxAttempts?, onPoison?, start?}, handler(msg, tx))` runs a consumer group (api.md §8).

* **Group id.** The server's group name is `PRF16(KDF("zen/v1/broker-group", N_topic, ""), lp(group))`, where `N_topic` is the topic's naming key (formats.md §3.3).
  * It is unique per topic, so one app name can be used on several topics, although the server scopes group names per fs.
  * The name itself never reaches the server.
* **Creation** is idempotent (api.md §8.1). `group_exists` means the parameters changed. Groups are immutable (G8), so the app must create a new group under a new name.
* **Loop.** For each delivery the class runs one transaction:
  1. `handler(msg, tx)`
  2. the consume step of the delivery (M4 `Consumer.ack(d, tx)`)
  3. `causation` filled in on every emit
* **Outcomes:**

  | Outcome | Action |
  |---|---|
  | commit succeeds | next delivery |
  | the handler throws | `nack`; after `maxAttempts` the event is dead-lettered (api.md §8.4), or retried forever with `onPoison: "block"` |
  | `conflict` / `too_old` | the transaction retries: same delivery, handler run again |
  | 412 `cursor_moved` / `claim_lost` | someone else committed it or took the key: drop the delivery |
  | 412 `not_leader` | the lease was lost: drop the deliveries, campaign again |

* **Delivery by mode:**
  * `sequential`, `partitioned` and `single_key` hand out events under a lease: one holder per partition, renewed automatically (M4 `Consumer.deliveries`).
  * `per_key` hands out keys under 30-second claims (`claim_ttl`), so a handler must commit well within that, or its commit fails with `claim_lost`.
* **Pull, not push.** Delivery is a long-poll of `/v1/consume/next` (`wait_ms` up to 30 s). The server does not push deliveries over the stream (TD-CONSUME-PUSH).
* **Other forms.**
  * `msg.ack()` acknowledges without writes: a commit with only the consume step.
  * `begin({consumes: msg})` binds a manual transaction to the delivery (§7.2).
* **Limits:**
  * at most `max_groups_per_topic` (64) groups per topic
  * a `per_key` group created at `earliest` on a topic with more than 100,000 events returns 413; create it with `start: "latest"`, or before the topic grows

### 11.4 Ephemeral messages

`publishEphemeral(topic, type, body)` and `onEphemeral(topic, handler)` use `epub`/`esub` (api.md §9) with the M4 sealing: a reserved key token, with HLC replay checks (G21).
* They are never stored, never part of a transaction, and rate-limited per device (api.md §9.1).
* They suit presence and typing indicators.

### 11.5 System tables

Every database has these tables, created with the DbRecord. They don't count as app schema, and app table names may not start with `$`.

| Table | pk | Fields | Indexes |
|---|---|---|---|
| `$cursors` | `[device, name]` | `topic: [bytes]`, `offset: bytes` | – |
| `$sched` | `[id]` | `at: int`, `kind: "emit" \| "gc"`, `topic: [bytes]`, `key?: bytes`, `msg: bytes` (CBOR Msg), `gc?: [[bytes]]` (KV paths to delete) | private on `at` |
| `$sagas` | `[id]` | `saga: text`, `step: int`, `status: text`, `data: map`, `done: [int]`, `timeout?: bytes` | – |
| `$inbox` | `[scope, id]` | `at: int` | – |
| `$ixrows` | `[table_id, pk]` | `values: {index_id: value}` (the values last indexed) | – (§19.6) |

## 12. Patterns

Every pattern below uses only §11, so only api.md §5–9. Each is stated with its commits and its guarantee; the failure cases are in §13.

### 12.1 Publish/subscribe

| Subscriber | Built with | Guarantee |
|---|---|---|
| a live view (UI) | `on(topic)` | every event while running, in topic order |
| a device that must not miss events | `on(topic, {cursor})` | database effects exactly once per device |
| a service: one handler across devices | `consume({mode: "sequential"})`, one group per service | exactly once per service; at most 64 services per topic |

Publishing is `emit`, usually inside the transaction that made the change (the outbox, §12.6).

### 12.2 Work queues

| Queue | Mode | Parallelism | Order |
|---|---|---|---|
| ordered per entity | `per_key`, key = entity | as many workers as ready keys | per key |
| bounded and parallel | `partitioned(N)`, key = entity or random | N leases | per partition |
| unordered, spread | `per_key`, key = a fresh random value per message (`spread: true`) | as many workers as messages | none |
| one at a time | `sequential` | 1 | total |

* **Spread queues.** A spread queue is the server's `per_key` mode used with one key per message. Every message becomes its own ready key, so any worker on any device takes the next one.
  * Its cost is per-key bookkeeping that the server never deletes (`kc` entries, keyspace.md §3.3).
  * A queue with high traffic should use `partitioned(N)` instead.
  * Native competing consumers are TD-CONSUME-COMPETING.
* **Retries.** The default is `retry: "nack"`: immediate redelivery and order kept, then the DLQ.
  * `retry: "delay"` instead acknowledges the message and, in the same transaction, schedules (§12.4) a copy for `now + backoff(attempt)` with header `zen.attempt` incremented. That gives backoff, but loses the order within the key.
  * After `maxAttempts` the copy goes to the dead letters of the class: a scheduled `emit` to `<topic>/zen.dlq`.

### 12.3 Request/reply

* **Inbox.** Each app instance (each owner, §9.2) has an inbox topic `("zen", "inbox", instance)`. The class subscribes to it with `on`, from now, on first use.
* **Requester:** `request(topic, type, body, {timeoutMs, key?})` emits `Msg {type, id, corr: random, reply_to: inbox path, body}`. Inside a transaction it is sent when the transaction commits. The requester then waits for a message on its inbox with the same `corr`:
  * a timeout fails the request with `timeout`
  * late replies are dropped
* **Responder:** `serve({group, topic, mode}, handler(msg, tx) → reply)` is a `consume` whose handler returns the reply body. The class emits `Msg {type: msg.type + ".reply", corr: msg.corr, body: reply}` to `reply_to` **in the same transaction** that consumes the request. So the request's database effects and the reply happen together, exactly once.
  * A handler that throws `ReplyError` sends an error reply (`type ".error"`) and consumes the request.
  * Any other throw goes through `nack`.
* **Repeated requests.** A requester that retries after a timeout sends the request again with the same `corr`. A responder with `dedup: true` records `($inbox, scope = group, id = corr)` in the handling transaction and replies again without re-running the handler.
* **Ephemeral replies.** `reply: "ephemeral"` sends the reply with `epub` after the commit, not as an event.
  * It costs no log space, but may be lost, and is rate-limited.
  * It suits cheap queries such as "who is online".
* **Scatter-gather.** A request to a topic that several `on` subscribers answer collects every reply until the timeout.
* **Grants (formats.md §9.1):**
  * the requester needs `append` on the request topic and `read` on its inbox
  * the responder needs `consume` on the request topic and `append` on the inbox
  
  A grant on the prefix `("zen", "inbox")` gives members both. Inboxes are not secret from other members of the fs: every member can derive every topic key of the fs (formats.md §3.3).
* **Storage.** Inbox topics are kept forever like every topic (TD-LOG-RETENTION).

### 12.4 Delayed messages

The server has no timers (TD-BROKER-SERVER-TIMERS). A **scheduler leader** inside the app delivers them.

* **`schedule(topic, type, body, {at | delayMs, key?})`**, inside a transaction or alone:
  * inserts a `$sched` row with a random `id`, `at` (unix ms, by the sender's clock), `kind: "emit"` and the encoded Msg (with `deliver_at = at`)
  * returns `id`
  
  The row appears only if the transaction commits.
* **`cancel(id)`** deletes the row in a transaction. It returns false if the message was already delivered.
* **The leader.** Any instance may run it (`scheduler.start()`, on by default in the owner, §9.2).
  * **Election.** It campaigns for the lease of a `sequential` group `"sched"` on the topic `("zen", "db", ns, "sched")` (M4 `Leader`).
  * **Loop.** While leading, it repeats:
    1. A short transaction reads the `$sched` index for rows with `at ≤ now`, up to 100 of them.
    2. For each row, `kind: "emit"` appends the stored Msg to its topic (with its key), and `kind: "gc"` deletes the listed KV paths.
    3. It deletes the rows.
    4. It commits.
    5. It sleeps until the earliest remaining `at`, at most `poll_ms` (default 1,000), and re-reads the index's RootRecord to see new rows.
  * **Clock.** `now` is the leader's clock corrected by the server's `time_ms` (api.md §2), as the M4 tree clock does.
* **Exactly once.** Each row is emitted at most once: the transaction that emits it also deletes it. Two schedulers (a deposed leader, a racing one) conflict on the rows, and one of them retries with the rows gone. The lease only saves duplicate work: correctness doesn't depend on it, as in DESIGN-3 §3.2.
* **Timing.**
  * A message is delivered no earlier than `at` by the leader's clock.
  * It is usually late by up to `poll_ms`, plus a lease takeover (≈ `ttl_ms` 10 s) when a leader dies.
  * With no instance running a scheduler, messages wait.
* **Sender.** The delivered event's `sender_fp` is the scheduler's device, not the original sender's.

### 12.5 Sagas

A saga is a series of local transactions in different services, each with a compensation, coordinated by an **orchestrator** in the app.

```
saga(name, steps: [{name, command: topic, compensate?: topic, timeoutMs?}])
```

* **State.** One `$sagas` row per instance: `step`, `status` (`running`, `compensating`, `done`, `failed`, `stuck`), app `data`, completed steps, the pending timeout's `$sched` id.
* **Start:** `start(data)`, usually inside the transaction that created the business object:
  1. insert the row
  2. emit step 0's command, `Msg {type: name + "." + step, saga: id, step, reply_to: ("zen", "saga", ns, name), body: data}`
  3. schedule a timeout to the same reply topic
* **Participants** are ordinary responders (§12.3). Each one handles the command in its own transaction and replies `ok` or `fail` (with data) in that transaction.
* **The orchestrator** consumes `("zen", "saga", ns, name)` with `per_key`, keyed by the saga id. So each instance's events are handled one at a time, and different instances in parallel. Each reply or timeout is one transaction:
  1. Read the saga row. A reply for another step than the current one is stale: acknowledge it and stop.
  2. On `ok`:
     * record the step
     * cancel its timeout
     * either emit the next command and schedule its timeout, or set `done`
  3. On `fail` or timeout: set `compensating`, then emit the compensations of the completed steps, one at a time in reverse order, each answered like a command.
     * When all are done: `failed`.
     * A compensation that ends in the DLQ leaves the saga `stuck`, for an operator.
  4. Acknowledge (the consume step).
* **Guarantee.** Every saga transition happens exactly once: the row update, the next command and the consume step are one commit. A participant's database work is exactly once, too. Its external calls are at least once, with the command's `id` as idempotency key (§12.6).
* **Compensations** must be idempotent and retriable.

### 12.6 Outbox, inbox and external effects

* **Outbox.** `emit` inside the transaction that changes the rows (§11.1). There is no relay process and no dual write.
* **Inbox.**
  * Within one group, each event is committed once (the consume step, api.md §8.3).
  * Messages may legitimately arrive again: a DLQ retry, a requester's retry, a producer re-emitting the same `id`. A consumer with `dedup: true` records `($inbox, scope, msg.id)` in its transaction and skips ids it has seen. The scheduler drops dedup rows after `dedup_ttl` (default 7 days) through `gc` jobs.
* **External effects** (email, payments) can't be exactly once (DESIGN-3 §3.3). The handler emits a command to an effects topic in its transaction. An effects worker then:
  1. calls the outside service with the idempotency key `msg.id`
  2. acknowledges
  
  A crash between the two repeats the call with the same key: at least once.
* **After commit.** `tx.after(fn)` runs `fn` after a successful commit. It may never run, if the process dies first, so it is for UI updates only.

### 12.7 Change events

* **Declaration.** A table with a `changes` definition (§3.2) appends a change message in every transaction that writes it:

  ```
  Msg {type: "zen.change", id, body: {op: "put" | "delete", table: name, pk, row?: fields}}
  ```

  * **Topic:** the definition's `topic`, by default `("zen", "db", ns, "changes", table)`.
  * **Key:** the pk element, so `per_key` consumers see each row's changes in order.
  * **Row:** with `image: "full"` the new fields are included when they fit one envelope, and left out otherwise. The consumer reads the row.
* **One event per row per transaction:** for a row written twice, the final state.
* **Uses:**
  * live views (`table.on(handler)`, an `on` of the change topic)
  * cache freshness (§7.7)
  * projections and read models, through a `per_key` or `sequential` group
* **Cost:** one event per changed row, kept forever (TD-LOG-RETENTION).

## 13. Guarantees and failures

### 13.1 Guarantees

| What | Guarantee |
|---|---|
| rows, index entries, emits, schedules, file ops of one transaction | atomic: all or nothing, once (`commit_id`) |
| database effects of a consumer handler | exactly once per group (consume step in the same commit) |
| database effects of an `on` handler with `cursor` | exactly once per device and cursor name |
| `on` without `cursor` | every event while the process runs, in order; none across a restart |
| emitted message | exists if and only if its transaction committed |
| scheduled message | emitted exactly once, no earlier than `at` |
| saga transition | exactly once |
| external effect | at least once, idempotency key = message `id` |
| order | per topic for `on`; per group mode for consumers (DESIGN-4 §1.2); across topics, by offset (versionstamps are global) |

### 13.2 Failures

The cases of DESIGN-3 §3.2, plus the ones a client library adds:

| Situation | Outcome |
|---|---|
| a handler stalls or its process dies before commit | nothing was written; the lease or claim expires; another holder gets the same event |
| a deposed leader commits late | 412 `not_leader` or `claim_lost`: none of its writes, emits or schedules land |
| two consumers race without a lease | the cursor check lets one commit; the other gets `cursor_moved` and drops the delivery |
| network partition | only clients that reach the server commit; the server's storage decides |
| crash after the commit, before the response | on restart the cursor has moved: no redelivery. In-process, `sendCommit` replays the `commit_id` and gets the original result |
| a handler always throws | `nack` up to `maxAttempts`, then the DLQ (or `block`) |
| a request's responder never answers | the requester times out; it may retry with the same `corr` (dedup, §12.3) |
| the scheduler leader dies mid-batch | its transaction never committed; the next leader emits the same rows |
| the owner tab closes | another tab takes the Web Lock and resumes from its cursors (§9.2) |
| an `auto` transaction's reads grow too large for long mode | `too_large`; the app splits the work |

---

# Common sections

## 14. Integrity

* **Basic, the only level now.**
  * Every value is AEAD-sealed and bound to its key: no forgery, no swapping between keys.
  * The server **can** serve an older valid version of a value, or an older RootRecord together with the nodes of that older tree. It can also withhold events (G26).
  * A member can forge another member's `sender_fp`, since all members hold the topic keys (DESIGN-2 §2.6).
* **Reserved for milestone 6 (the authenticated tier):**
  * the DbRecord value `integrity: "authenticated"`
  * the signature purpose `zen/v1/sig/db-root` (spec/labels.md), for a signed root over each index root, a row tree and the catalog
  * Private-index node ids are already keyed hashes of their content, so a signed root id commits to a whole index. Milestone 6 reuses this.
* **Event signatures** stay as DESIGN-2 §2.6 and DESIGN-3 §0 describe them, for a later milestone.

## 15. Leakage

The server never sees database, table, field or index names, values, keys, message types or bodies. It does see:

**Database**
* that the keys under one PRF prefix (the `"zen"` element) belong to zen-db, and how many databases, tables and indexes exist
* the number of rows per table, and their sizes unless padded (§4.3); parts betray large rows
* which rows are read and written, when, and together in which transactions
* **unique:** one key per row; a value's token reappears if the value is reused
* **fast:** how many rows share each value, and when that changes
* **private** (`kind: "private"`; see §5.6 for the alternatives):
  * node count and tree height, so roughly the number of rows
  * per write, which nodes are replaced. Writes that replace the same leaves have nearby values: over time the server learns **coarse order clusters** (about `fanout` neighbouring values)
  * per query, which nodes are read: the result's size and roughly its position
  * with `decoys`, the same, statistically blurred (§5.4.6)
* **sealed:** the index's size class, and when it is written or read (§5.7)
* **oblivious:** the capacity, and the number and timing of operations. Not which entries, nor whether an operation read or wrote (§5.8.3).
* catalog and migration activity
* **a revoked member who kept NK** can compute key tokens, boundaries and node ids. So they can test guesses about names, values and node contents against what they still see. Rotation doesn't change NK (G2); renaming the keys needs the explicit migration of G2.

**Broker**
* the topic tree's shape; topics under one prefix share a token prefix, so all inboxes, sagas and change topics group together
* event counts, sizes and timing per topic; key tokens link the events of one entity within a topic
* group ids (opaque), modes, lease holders (device fingerprints), lags and DLQ sizes
* **request and reply:** a request on one topic followed quickly by an append on an inbox links requester and responder, and the inbox count reveals the number of instances
* **schedules:** the scheduler's emits happen near `at`, so delivery times are visible after the fact
* **change events:** they are appended in the same commit as their row writes, which links each row's KV token to its event key token
* ephemeral messages: their timing and size (api.md §9)

---

# Part C: the class (informative)

## 16. API sketch

```ts
const db = await zen.db(fs, 'shop', {
  schema: 2,
  migrations: {
    1: async (m) => {
      await m.createTable('orders', { pk: ['id'], changes: { image: 'keys' } });
      await m.createIndex('orders', 'by_status', { fields: [['status', 'text'], ['created', 'int', 'desc']] });
    },
    2: async (m) => m.createIndex('orders', 'by_email', { fields: [['email', 'text']], unique: true, kind: 'none' }),
  },
});
const orders = db.table<Order>('orders');

// CRUD (each one transaction)
await orders.insert(o); await orders.put(o); await orders.get(id);
await orders.update(id, (o) => ({ ...o, status: 'paid' })); await orders.delete(id);
const { rows, cursor } = await orders.query().where('status', '=', 'open').orderBy('created', 'desc').limit(20).page();

// one transaction: rows + messages + schedule + files, one commit
await db.transaction(async (tx) => {
  const o = await tx.table(orders).get(id);
  tx.table(orders).put({ ...o, status: 'paid' });
  tx.emit(db.topic('billing'), 'order.paid', { id }, { key: o.customer });
  tx.schedule(db.topic('mail'), 'order.reminder', { id }, { delayMs: 86_400_000 });
  await tx.fs(tree).write(node, manifest, { replaces });    // milestone 5
}, { mode: 'auto' });

// broker
db.emit(db.topic('audit'), 'login', { who });
const sub = db.on(db.topic('chat', room), (msg) => render(msg), { after: lastOffset });
db.on(db.topic('billing'), async (msg, tx) => { /* exactly once per device */ }, { cursor: 'billing-view' });
db.consume({ group: 'invoicer', topic: db.topic('billing'), mode: 'per_key' }, async (msg, tx) => {
  tx.table(invoices).insert({ id: msg.body.id, … });
  tx.emit(db.topic('mail'), 'invoice.ready', { id: msg.body.id });
});
const reply = await db.request(db.topic('stock'), 'reserve', { sku, n }, { timeoutMs: 5000 });
db.serve({ group: 'stock', topic: db.topic('stock'), mode: 'per_key' }, async (msg, tx) => ({ ok: true }));
const id = await db.schedule(db.topic('mail'), 'nudge', {}, { at: Date.now() + 3600_000 }); await db.cancel(id);
const checkout = db.saga('checkout', [{ name: 'pay', command: db.topic('pay'), compensate: db.topic('refund'), timeoutMs: 30_000 }, …]);
await db.transaction(async (tx) => { tx.table(orders).insert(o); checkout.start({ order: o.id }, tx); });
orders.on((change) => refresh(change.pk));                        // change events
const cards = db.table<Card>('cards');                            // a CRDT table (§19): merge "crdt"
await cards.patch(id, { title: 'New title' });                    // per-field last writer wins, works offline
await cards.incr(id, 'votes', 1); await cards.add(id, 'labels', 'urgent');
db.publishEphemeral(db.topic('presence'), 'typing', { room });    // not stored
```

### 16.1 Methods and what they use

| Method | M4 client (packages/client) | api.md |
|---|---|---|
| `zen.db(fs, ns, opts)` | `UnlockedFs.kv`, `transaction` | `kv/get`, `commit` |
| `table.get`, `query` | `Kv.getEntries`, `Kv.rangeStored` | §5 `kv/get`, `kv/range` |
| `table.insert/put/update/delete` | `Transaction.get/set/delete` | §6 `writes`, `read_conflicts` / `expect` |
| `db.transaction(fn, {mode})` | `transaction()` | §6 |
| `db.begin({mode, consumes})` | `Transaction`, `sendCommit` | §6 |
| `emit` | `Topic.appendIn(tx)`, `Topic.append` | §6 `append`, §7.1 |
| `on` | `Stream.subscribe` | §9 `sub` |
| `consume`, `serve`, `msg.ack` | `Topic.group` → `Consumer.deliveries/next/ack(d, tx)/nack/dlq` | §8.1–8.5, §6 `consume` |
| `request` | `Stream.subscribe` (inbox), `Topic.appendIn` | §9, §6 |
| `schedule`, `cancel` | `Transaction` (rows of `$sched`) | §6 |
| scheduler, migrations' optional leader | `Topic.leader` → `Leader.campaign/renew` | §8.2 |
| `saga` | `consume` + rows + `schedule` | as above |
| `tx.fs(tree)` | `Tree` operations into `Transaction.extra` (new in milestone 5) | §6 `chunks`, `crdt_ops`; fs.md |
| `publishEphemeral`, `onEphemeral` | `Stream.publish`, `Stream.subscribeEphemeral` | §9 `epub`, `esub` |

### 16.2 One commit, everything in it

The transaction below handles a `card.attach` command: it moves a kanban card, attaches a file whose chunks were uploaded before, emits a notification, and consumes the command. Its commit:

```
{ commit_id,
  read_version,
  read_conflicts: [ TableRecord("cards"), Row(card), RootRecord(by_column) ],
  writes:   [ Row(card)',                                   // new column, attachment ref
              Node…', RootRecord(by_column)', Node… = null ],   // private index path rewritten
  append:   [ {topic: id("zen","db","kanban","changes","cards"), key_token, envelope},   // change event
              {topic: id("board", b), envelope} ],          // notification for live boards
  consume:  [ {group: G("attach"), key_token, from, to, token} ],   // the command, per_key
  crdt_ops: [ {op: "write", tree, node: attachment, replaces: [], chunks: [c1, c2], manifest} ] }
```

If any part fails (a stale claim, a conflict on the card or the index, a `stale_op`), nothing applies. The command is redelivered, or the transaction retries.

## 17. Test vectors and tests (milestone 5)

Generated by zen-core's `gen_vectors` into `spec/test-vectors/zendb.json`, checked byte-exactly in CI:
* `K_db`, `K_boundary` and `K_node` for fixed inputs; the group id of §11.3
* Row encodings, a row split into parts with its digest, padding buckets
* the sort keys of §5.1: every type, `desc`, escaping, composite keys with a pk
* a canonical prolly tree for a fixed entry set with `fanout` 4: every node's bytes and id, the RootRecord; the same after inserting and deleting entries; shard selection
* a sealed index: the blob, its parts, padding and head
* an oblivious index: the bucket, Block, head and stash encodings, and one access with a fixed RNG
* Msg encodings, BodyRef, an event id for causation

Tests (G23):
* **properties:**
  * random insert and delete orders give the same root (history independence)
  * query results equal a naive in-memory model
  * concurrent writers never lose an index entry
  * `sealed` and `oblivious` indexes answer like `private` ones
  * an oblivious access always shows `A` paths and identical write sizes, and its stash stays bounded over long random runs
* **every pattern of §12 against a spawned server**, with fault injection:
  * killed handlers, deposed leaders, dropped connections
  * replayed commits, a scheduler failover
* **the five targets of docs/EXAMPLES.md**, end to end

## 18. Gaps

What the server lacks for this layer, deferred (spec/TECH_DEBT.md):
* **TD-BROKER-SERVER-TIMERS:** delayed delivery without a polling leader
* **TD-CONSUME-COMPETING:** competing consumers for unkeyed events, without per-key state per message
* **TD-CONSUME-PUSH:** delivery over the stream instead of long-polling (DESIGN-4 §1.4)
* **TD-LOG-RETENTION:** events, inboxes, change topics and message parts are never trimmed. Change events and request/reply make this more pressing.

---

# Part D: CRDT tables

## 19. CRDT tables (server-merged)

**Status:** specified; the server side is scheduled for milestone 5 (TD-CRDT-ROWS-SERVER). A client checks that `/v1/info` lists the feature `"crdt_rows"` before creating or writing a CRDT table, and fails with `unsupported` otherwise.

### 19.1 Model

A table with `merge: "crdt"` holds rows that the **server merges** field by field, on ciphertext (DESIGN-4 §2.1–2.2), the way it merges filesystem trees.

* **Writes never conflict.** In long mode they are always accepted.
* **Offline writes work.** They are queued with their HLCs and sent later, up to the horizon.
* **Concurrent writes to different fields both survive.**

| Field type | Merge | Typical use |
|---|---|---|
| `lww` (default) | last writer wins per field, by `ts = (hlc, device)` | titles, text, status |
| `counter` | PN-counter: one cumulative value per device; the value is their sum | votes, likes, quantities |
| `set` | add-wins observed-remove set (OR-set) | labels, members, tags |
| row existence | last writer wins on an `alive` register | insert, delete |

**What is given up:** cross-field and cross-row invariants. There are no unique indexes, no balance checks, and no reading of one row to decide another. Those need transactional tables. A transaction can still carry CRDT operations atomically with rows of transactional tables, emits and consumes. It just can't make them conditional.

Deletes are by `ts`. A delete with a greater `ts` than an insert hides the row, and field updates don't resurrect it; only a later `row` operation with `alive: true` does.

### 19.2 Tokens and sealing

```
K_crdt   = KDF("zen/v1/db-crdt", K_db, table_id)
object   = the stored key of D ‖ ("t", table_id, pk)       (the server's object id; 16 bytes per element)
field    = PRF16(K_crdt, 0x00 ‖ lp(field_name))
elem     = PRF16(K_crdt, 0x01 ‖ lp(field_name) ‖ lp(pk element) ‖ lp(CBOR(element)))
```

* **Element tokens** include the pk, so the same element in two rows has unrelated tokens.
* **Field tokens** are the same in every row of a table, like columns.

Values are sealed as **kind 7, CRDT value** (formats.md §4):
* **AAD** label `zen/v1/aad/crdt-value`
* **context** `u32(fs) ‖ lp(object) ‖ field(16 bytes; zeros for the row register) ‖ elem(16 bytes; zeros if none)`
* **key** the KV AEAD key of `key_epoch`

Plaintexts:

| Value of | Plaintext (CBOR) |
|---|---|
| row register | `{1: pk}`, so scans return keys |
| `lww` field | `{1: value}` |
| `counter` entry | `{1: total: int}`: this device's cumulative sum of increments and decrements |
| `set` element | `{1: element}` |

### 19.3 Operations

New `CrdtOp` variants in the commit's `crdt_ops` (api.md §6):

```
{fs, op: "row", object: bytes, hlc: u64, alive: bool, value: bytes}            // insert (alive) or delete
{fs, op: "lww", object, field: bytes(16), hlc: u64, value?: bytes}              // set; value absent = unset
{fs, op: "ctr", object, field: bytes(16), seq: u64, value: bytes}              // this device's new total
{fs, op: "add", object, field: bytes(16), elem: bytes(16), value: bytes}       // gets a dot
{fs, op: "rem", object, field: bytes(16), elem: bytes(16), dots: [bytes(12)]}  // removes observed dots
```

* `device` is the session's device fingerprint, set by the server as in fs.md §2.
* **`row` and `lww`:** keep the value with the greatest `(hlc, device)`.
  * A second operation with the same `(hlc, device)` on the same register is refused (400).
  * `clock_skew` and the horizon apply as in fs.md §3.4: an `hlc` too far ahead gets `clock_skew`, and one older than `crdt_horizon_secs` gets `stale_op`.
* **`ctr`:** for each `(object, field, device)`, keep the operation with the greatest `seq`. A `seq` not greater than the stored one is ignored, so a replay is harmless. A device only ever writes its own entry.
* **`add`:**
  * The element gets the dot `versionstamp ‖ u16(i)`, where `i` is its index among the commit's `add`s. Dots are returned with the commit result, next to fs `dots`.
  * The element is present while it has at least one dot.
* **`rem`:** deletes the listed dots of `(object, field, elem)` that still exist. An `add` the remover hadn't seen keeps its dot, so concurrent add and remove resolve as add-wins.
* **Rights and limits.** Each operation needs fs `write`, and each `value` is at most `max_value_bytes`. Operations count toward the fs quotas like KV writes.
* **Isolation (G11).**
  * In long mode the server retries its own conflicts, so CRDT operations never fail with `conflict`.
  * In short mode the state the server reads conflicts like any read.

### 19.4 Reads

```
POST /v1/crdt/get   {fs, objects: [bytes], read_version?}                → {read_version, objects: [ObjState]}
POST /v1/crdt/range {fs, begin: bytes, end?: bytes, limit?, read_version?} → {read_version, objects: [ObjState], more}

ObjState = { object: bytes,
             row?: {hlc, device, alive, value},
             lww:  [{field, hlc, device, value}],
             ctr:  [{field, device, seq, value}],
             set:  [{field, elem, dot, device, value}],
             version: bytes(10) }                 // versionstamp of the object's last change
```

* Both need fs `read`, and `range` pages like `/v1/kv/range`.
* The client opens every value and builds the merged row:
  * `lww` fields as stored
  * counters summed over devices
  * sets as the elements with at least one dot
  * the row visible if its register is `alive`
* **Inside a transaction,** reads of CRDT rows are **not** part of its read set: there is no conflict range for them. A transaction that reads a CRDT row and writes a transactional row is not serializable with respect to the CRDT row. That's by design: the CRDT row accepts every concurrent write.

### 19.5 Server state (proposal for the implementation)

| Key | Value |
|---|---|
| `pack("cr", fs, object)` | row register: `hlc ‖ device ‖ alive ‖ value` |
| `pack("cw", fs, object, field)` | `lww` register: `hlc ‖ device ‖ value?` |
| `pack("cn", fs, object, field, device)` | counter entry: `seq ‖ value` |
| `pack("cs", fs, object, field, elem, dot)` | set element: `device ‖ value` |
| `pack("cv", fs, object)` | the object's last-change versionstamp |
| `pack("cd", fs, hlc, object)` | deleted-object index, for the sweeper |

**Garbage collection:**
* An object whose row register is `alive: false` and older than the horizon is purged with all its entries.
* So is an object that has no row register and no change within the horizon.
* An operation that names a purged object and is older than the horizon gets `stale_op`. A newer one starts the object afresh.

### 19.6 Indexes

* **Kinds.** CRDT tables allow `fast`, `private`, `sealed` and `oblivious` indexes, never `unique`.
* **The indexer.** Writers don't maintain them, since the merged value is known only after the server merges. Instead:
  * **Change topic.** A CRDT table with indexes must declare `changes` (§12.7). Every commit with operations on a row also appends a change event keyed by the pk; the class adds it.
  * **Indexer group.** The **indexer** is a `per_key` consumer group `"$index"` on that topic, run by the owner (§9.2) like the scheduler. For each event it does one short transaction with the consume step:
    1. read the row's merged state
    2. read its `$ixrows` entry: the values it last indexed
    3. update every index from the old values to the new ones
    4. write `$ixrows`
* **Freshness.** Indexes on CRDT tables are **eventually consistent**: they lag by the indexer's delay, and stop while no instance runs it. Queries always re-check their predicates on the fetched merged rows, so a stale entry never returns a wrong row. A row changed very recently may be missing from a result until it is indexed.

### 19.7 Offline use

* **Queue.** While offline, the class keeps operations and their change events in a local queue: in memory, or encrypted at rest (§9.3). It sends them as long-mode commits when back online.
* **Clock.** It uses one HLC per session, the M4 tree clock.
* **Old operations.** An operation older than the horizon gets `stale_op`. The class reissues it with a fresh `hlc` (a rebase, fs.md §3.4), so the offline edit then wins over edits made since. That is the same rule as files, and it is documented to users.

### 19.8 Leakage

Beyond §15, the server sees, per CRDT row:
* which fields (by token) exist and change, when, and from which device
* the `hlc` of every write, which is wall-clock time to the millisecond
* deletes, through the plaintext `alive` flag
* for counters, how often each device changes each one (`seq`)
* for sets, how many elements each field holds and when each is added or removed. Element tokens are per row, so equal elements in different rows aren't linkable.

### 19.9 Test vectors

Added to §17:
* `K_crdt`, field and element tokens
* kind-7 sealed values for each field type
* the encoding of each operation
* a merged `ObjState` from a fixed set of operations, with its client-side view

Property test: operations from three devices, applied in random orders, give the same merged state.

