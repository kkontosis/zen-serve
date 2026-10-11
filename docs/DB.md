# @zen/db

`@zen/db` is a client-side database over zen-serve's encrypted KV ([`spec/zendb.md`](../spec/zendb.md) Part A). Rows, indexes and the catalog are sealed KV values under PRF-token keys. The server sees opaque keys and values, and the access pattern that each index kind leaks (§5.6 below). This guide covers the database. The broker (`emit`, `on`, `consume`, …) comes in milestone 5 step 6, and CRDT tables in step 7.

## 1. Opening

```ts
import { Db } from '@zen/db';

const db = await Db.open(fs, 'shop', {        // fs: an UnlockedFs (docs/CLIENT.md §4)
  schema: 2,
  migrations: {
    1: async (m) => {
      await m.createTable('orders', { pk: ['id'] });
      await m.createIndex('orders', 'by_status', {
        fields: [['status', 'text'], ['created', 'int', 'desc']],
      });
    },
    2: async (m) => m.createIndex('orders', 'by_email', {
      fields: [['email', 'text']], unique: true, kind: 'none',
    }),
  },
});
```

* A database is `(fs, ns)`. Its keys derive from the fs keys and `ns`, so two namespaces share nothing.
* `Db.open` creates the database at schema 0 and runs migration steps up to `schema`. A stored schema newer than the app's fails with `schema_newer`. An unknown format or integrity level fails with `format`.
* **Migrations (§8).** The steps are:
  * `createTable(name, {pk, pad?})`, `renameTable`, `dropTable`
  * `createIndex(table, name, opts)`, `dropIndex`
  * `transform(table, fn)`, where `fn` returns the new row, or null to delete it

  Paged steps (backfills, transforms, drops) record their progress, so a step that was interrupted resumes where it stopped. Two clients running the same step conflict page by page, and only one advances each page.
* `pageSize` (default 256) sets the rows per migration page. `warn` receives warnings such as table scans.
* `db.close()` frees the keys and caches, and hands the owner role over (§8).

## 2. Tables and rows

```ts
const orders = db.table<Order>('orders');
await orders.insert(o);            // `exists` if the pk is taken
await orders.put(o);               // insert or replace
await orders.get(id);              // undefined if absent
await orders.update(id, (o) => ({ ...o, status: 'paid' }));   // `not_found` if absent
await orders.delete(id);           // whether there was one
```

* **Values** are deterministic CBOR (§1): null, booleans, integers (numbers within ±2^53, `bigint` beyond), floats, text, `Uint8Array`, arrays and plain objects.
* **pk.** A row's primary key is its `pk` fields: one value, or an array for several.
* **Large rows** are split into parts (§4.2).
* **Padding.** `pad: true` pads rows to size buckets (§4.3).
* **Transactions.** Each table method is a transaction of its own. Inside `db.transaction`, use `tx.table(name)` (§5).

## 3. Indexes

| Need | Options | Leaks to the server |
|---|---|---|
| lookup by a unique value | `unique: true, kind: 'none'` | existence of the value only |
| equality on a frequent value, fast | `kind: 'fast'` | how many rows share each value |
| equality, ranges, ordering, paging | `kind: 'private'` (default) | tree shape and write locality |
| the same, with locality blurred | `kind: 'private', decoys: k` | as above, statistically weaker |
| the same, small index, no locality leak | `kind: 'sealed'` | the size class; every write rewrites it whole |

* **Fields.** `fields` lists `[name, type]` or `[name, type, 'desc']`, with the types `text`, `bytes`, `int`, `float` and `bool`.
  * Writing a value of another type fails with `bad_type`.
  * A missing field indexes as null, and nulls sort first.
* **`unique: true`** works with `none`, `fast`, `private` and `sealed`.
  * A second row with the same value fails with `unique_violation`. Values containing a null are not checked.
  * If a backfill finds duplicates, the index stays `building` and the duplicates are reported.
* **Private (prolly tree, §5.4).** Its options:
  * `fanout` (default 64) sets the average node size.
  * `shards` (default 1) splits it into K trees, so writers to different shards don't conflict. A range then reads all K.
  * `decoys: k` rewrites k extra random paths per write, with salted nodes padded to size buckets. It needs `max_value_bytes` ≥ 65,584.
* **Sealed (§5.7)** holds the whole index in one blob.
  * Its `maxBytes` defaults to 1 MiB; a write past it fails with `too_large`.
  * It suits small indexes, such as a few thousand entries.
* **Oblivious** indexes (§5.8) are not built yet (TD-DB-OBLIVIOUS-INDEX).

## 4. Queries and paging

```ts
const { rows, cursor } = await orders.query()
  .where('status', '=', 'open')
  .where('created', '>=', since)
  .orderBy('created', 'desc')
  .limit(20)
  .page();
const next = await orders.query()/* same conditions */.after(cursor).page();
```

* **Operators:** `=`, `<`, `<=`, `>`, `>=`, `prefix` (text or bytes) and `in`.
* **Access path.** A query uses one access path (§6.1), in this order:
  1. a pk lookup
  2. a unique lookup
  3. a private or sealed range whose leading fields are bound by `=`, and whose next field carries the range or the `orderBy`
  4. fast equality
  5. a table scan, with a warning

  The remaining conditions are checked on the rows.
* **Ordering.** An `orderBy` that no index provides is sorted in memory. With a `limit` and more rows than the limit, it fails with `needs_index`.
* **Cursors** hold plaintext values. Keep them in the client: never send one to a server or put it in a URL.
* **Isolation.** A query runs in a transaction, and the index roots or heads it reads are in the read set, so a query sees one consistent state.

## 5. Transactions

```ts
await db.transaction(async (tx) => {
  const o = await tx.table<Order>('orders').get(id);
  await tx.table('orders').put({ ...o!, status: 'paid' });
  await tx.table('payments').insert({ id: payId, order: id });
}, { mode: 'auto' });
```

* **Modes (§7.1):**
  * **`short`** (default) is serializable through read conflicts, and must finish within about 5 s.
  * **`long`** has no time limit. Its reads become `expect`s checked at commit, so a range read must fit one response (`max_range_items`). Otherwise it fails with `too_large: … split the work`.
  * **`auto`** runs short first. It re-runs the whole function once in long mode when the function is still running after `autoSwitchMs` (default 3,000; a `DbOptions` or per-call option), or when the server answers `too_old`. `tx.mode` tells which phase is running.
* **Retries.** The function may run several times: `conflict` and `too_old` re-run it, up to `attempts` (default 8). It must have no side effects except through `tx`.
* **Manual transactions (§7.2).** `db.begin({mode})` returns a transaction the app commits with `await tx.commit()` or drops with `tx.abort()`.
  * There is no retry: `conflict` reaches the caller.
  * After either call, any use fails with `closed`.
* **Limits (§7.5).** A commit is checked against the server's `max_commit_ops` and `max_commit_bytes` before it is sent, and fails early with `too_large`.
  * A row write costs about one op, plus its parts, plus two per unique or fast index.
  * Private-index paths are shared by the rows of one commit.
* **Bulk loads.** `db.importRows(table, rows, {replace?, onCommit?})` writes in batches of commits.
  * Batches start at 256 rows, halve on `too_large`, and double (up to 4,096) after 4 commits in a row.
  * Each commit is atomic; the load as a whole is not. If it fails partway, the earlier commits stay, and `onCommit(done)` has reported how far it got.

## 6. Files in a transaction

```ts
const up = await tree.upload(node, bytes);        // chunks, in commits of their own
await db.transaction(async (tx) => {
  await tx.fs(tree).write(up);                    // the write op, in this commit
  const b = tree.batch(); b.mkdir(ROOT, 'attachments');
  await tx.fs(tree).ops(b);                       // any tree operations
  await tx.table('cards').put({ ...card, attachment: up.index.hashes[0] });
});
```

* **Atomicity.** File operations and rows commit together (§7.6).
* **Uploads first.** Upload the chunks before the transaction, so a re-run costs only the operation. Unreferenced chunks are collected after `chunk_grace_secs`.
* **Rebase.** On `stale_op` or `clock_skew` the operations are rebased and the transaction runs again (spec/fs.md §3.4).
* **One write per file per commit.** A second `write` to the same file in one transaction fails with `bad_value`.

## 7. Caches and coherence

Every `Db` keeps four caches, all in memory only (§9.3; a persistent cache is TD-DB-PERSISTENT-CACHE):
* catalog records
* private-index nodes, which never change (an LRU of 4,096)
* sealed-index blobs, by digest
* rows: an LRU of `rowCache` rows (default 1,024; 0 turns it off)

**Coherence (§7.7).** A transaction that uses a cached record or row puts its version into the read set (an `expect`). A stale copy therefore makes the commit fail with `conflict`; the copy is dropped, and the transaction re-runs with fresh data. It never commits based on stale data.
* **Read-only transactions** send no commit. Instead, the versions of the cached records they used are checked with one batched read before they return.
* **Committed writes** drop their rows from the cache.
* `db.stats` counts row-cache hits and misses.

## 8. Tabs and the owner

In a browser, one tab per database is the **owner** (§9.2). It holds the Web Lock `zen/db/<fs>/<ns>`.
* **State:** `db.isOwner`; `db.onOwner(fn)` calls `fn` on becoming the owner (at once if it already is) and returns an unsubscribe function.
* **Handover:** `close()` releases the lock, and the next waiting instance takes over.
* **Without Web Locks** (Node, or `locks: null`), every instance is its own owner. With `owner: false`, an instance never is.
* **What the owner will run (step 6):** consumers, the scheduler and the stream. Until then every tab runs its own transactions, which is safe because they are serializable on the server. Forwarding other tabs' work to the owner is TD-DB-TAB-PROXY.

## 9. Errors

`DbError` has a `code`; server errors arrive as `ZenError` (`conflict` after the retries ran out, `forbidden`, …).

| Code | When |
|---|---|
| `exists` | `insert` of a pk that is taken |
| `not_found` | `update` of a missing row, an unknown table |
| `unique_violation` | a unique value held by another row (`detail.pk`) |
| `bad_type` | an indexed value of the wrong type |
| `bad_value` | a value CBOR can't hold, a row without its pk, a bad option, a second write to one file |
| `schema_newer` | the stored schema is newer than the app's |
| `needs_index` | a limited `orderBy` that no index provides |
| `too_large` | a commit over the server's limits, a sort key over 4,096 bytes, a sealed index over `maxBytes`, a long-mode range over one response |
| `corrupt` | a stored value that fails its checks |
| `format` | an unknown format, integrity level or index kind |
| `building` | a unique index whose backfill found duplicates |
| `closed` | a manual transaction used after `commit` or `abort` |
