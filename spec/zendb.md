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

Part A (§2–9) is the database. Part B (§10–13) is the broker. §14–15 cover integrity and leakage for both. §16 sketches the class API; it is **informative**, and every other section is normative. §17 plans the test vectors, which milestone 5 generates with the code.

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
                7: state: "active" | "dropping" }

IndexDef    = { 1: name: text,
                2: id: bytes(16),
                3: fields: [[name: text, type: Type, desc: bool]],
                4: kind: "private" | "fast" | "none",
                5: unique: bool,
                6: state: "building" | "active" | "dropping",
                7: fanout?: u32,                 // private: target entries per node, default 64
                8: shards?: u32,                 // private: number of trees, default 1 (§5.4.5)
                9: built_to?: bytes }            // building: the stored key the backfill reached (§8.2)

Type        = "text" | "bytes" | "int" | "float" | "bool"
ChangeDef   = { 1: topic: [bytes], 2: image: "keys" | "full" }
```

* **Mutability.** An index's `fields`, `kind`, `unique`, `fanout` and `shards` never change. To change them, build a new index and drop the old one.
* **Index kinds.**
  * `kind: "none"` with `unique: true` is an index used only for uniqueness and equality lookups through its unique entries.
  * `kind: "none"` with `unique: false` is invalid.
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
               2: entries: [bytes] (level 0) | [[first_key: bytes, child: bytes(16), count: u64]] (above) }
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

The commit's random `commit_id` makes it idempotent (api.md §6.1). M4's `sendCommit` replays it after a network error or `commit_unknown`.

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
