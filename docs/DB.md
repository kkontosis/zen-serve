# @zen/db

`@zen/db` is a client-side database over zen-serve's encrypted KV ([`spec/zendb.md`](../spec/zendb.md) Part A). Rows, indexes and the catalog are sealed KV values under PRF-token keys. The server sees opaque keys and values, and the access pattern that each index kind leaks (§5.6 below). Sections 1–9 cover the database (Part A), sections 10–18 the broker (Part B). Section 19 covers CRDT tables (Part D), section 20 Loro documents (`@zen/loro`).

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

Every `Db` keeps five caches (with CRDT states, §19). In memory, and with `DbOptions.local` on the sealed local store too (docs/CLIENT.md §10), so a restart starts warm:
* catalog records
* private-index nodes, which never change (an LRU of 4,096)
* sealed-index blobs, by digest
* rows: an LRU of `rowCache` rows (default 1,024; 0 turns it off)
* CRDT states (§19, read modes)

**Coherence (§7.7).** A transaction that uses a cached record or row puts its version into the read set (an `expect`). A stale copy therefore makes the commit fail with `conflict`; the copy is dropped, and the transaction re-runs with fresh data. It never commits based on stale data.
* **Read-only transactions** send no commit. Instead, the versions of the cached records they used are checked with one batched read before they return.
* **Committed writes** drop their rows from the cache.
* `db.stats` counts row-cache hits and misses.

**Kept on the local store.** Records are written behind (gathered every 100 ms, and at `close()`, which resolves once they are written). Rows and CRDT states are loaded at open; nodes and blobs are read when missed in memory. Every kept record is checked when used, as above, so a row that changed while the app was closed costs one retry, and a record that doesn't open (tampered with, another key) is dropped with a warning. `persist: false` keeps the caches in memory only (the queue of CRDT writes stays on the store). `Db.wipe(local, ns)` deletes it all.

**Offline.** A database opened with `local` once can open again offline, from the kept server info, DbRecord and catalog, as long as its schema is the app's (a due migration needs the server: the open fails with `offline`). `db.offline` says it did.
```ts
const local = sealedStore(indexedDbLocalStore('my-app'));
const fs = await session.fs(1).unlock(how, { local });       // the kept header, offline
const db = await Db.open(fs, 'app', { schema, migrations, local });
await db.table<Item>('items').get(id, { read: 'cache' });     // a cached row, unchecked when offline
```
* **Transactional rows offline** only with the read mode `cache` (`get(pk, {read: 'cache'})`, `db.transaction(fn, {read: 'cache'})`): online it reads as usual; when the server can't be reached, a read-only transaction answers from the rows it has cached, which may be stale, and a row not cached is `offline`. Without it, offline reads of transactional rows fail with the network error. Writes and queries on transactional tables need the server.
* **CRDT tables** read and queue writes offline as in §19.
* **The clock** ticks from the device's wall clock until the server is reached; queued edits are re-stamped if the server refuses their time. Give the session a `ClockStore` to keep the HLC and the counter actor across restarts.

## 8. Tabs, the owner and the proxy

In a browser, one tab per database is the **owner** (§9.2). It holds the Web Lock `zen/db/<fs>/<ns>`.
* **State:** `db.isOwner`; `db.onOwner(fn)` calls `fn` on becoming the owner (at once if it already is) and returns an unsubscribe function.
* **What runs only in the owner:** consumers, `serve`, the scheduler and saga orchestrators. Other tabs may declare them; they start when the tab becomes the owner.
* **The proxy.** Only the owner holds a stream. Other tabs send it their subscriptions (`on`, `onEphemeral`, the inbox of `request`) and ephemeral publishes over the `BroadcastChannel` of the same name. Log events come back sealed and each tab opens them itself; ephemeral messages come back opened.
* **Transactions** run in every tab; they are serializable on the server.
* **Handover:** `close()` (or a closed page) sends `bye` and releases the lock. The next waiting tab takes over, announces itself, and every tab re-subscribes from the last offset it delivered: no event is lost or repeated. Its consumers and scheduler start.
* **Without Web Locks** (Node, or `locks: null`), every instance is its own owner and streams itself. `channel: null` turns the proxy off; `owner: false` never campaigns.
* **Tests** inject `locks` (a `LockManagerLike`) and `channel` (a factory of `BroadcastChannelLike`).

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
| `closed` | a manual transaction used after `commit` or `abort`; a request pending when the database closed |
| `poisoned` | a message that doesn't decode, or whose body parts are gone |
| `timeout` | a request without a reply in time; no owner tab answering |
| `unsupported` | a CRDT table on a server without `"crdt_rows"` |
| `offline` | a CRDT read that needs the server while it can't be reached (§19) |

---

# The broker

## 10. Messages and `emit`

A message is a `Msg {type, id, body, …}` (§10.1) in an event of a topic. Types and header names starting with `zen.` are reserved.

```ts
const orders = db.topic('shop', 'orders');
await db.transaction(async (tx) => {
  await tx.table('orders').insert(order);
  tx.emit(orders, 'order.created', order, { key: order.id });   // the outbox: appended iff this commits
});
await db.emit(orders, 'order.note', 'hi');                      // a transaction of its own
```

* **Options:** `key` (order within `per_key` and `partitioned` groups), `spread` (a random key: a spread work queue), `id` (the idempotency key; default random), `headers`, `ttlMs`.
* **Large bodies** go to parts of the database in the same commit, and the event carries a reference (§10.3). With `ttlMs`, the scheduler deletes the parts later.
* **Causation:** inside a consumer, every emit carries the consumed event's id.
* `tx.after(fn)` runs `fn` after a successful commit (UI updates only: it may never run).

## 11. Subscribing: `on`

```ts
const sub = await db.on(orders, (msg) => render(msg.body));                 // from now
await db.on(orders, (msg) => …, { after: offset });                          // history, then live
await db.on(orders, async (msg, tx) => { … }, { cursor: 'projection' });     // durable
await db.on({ prefix: db.topic('shop'), topics: [orders, payments] }, …);
```

* **Plain:** every event while the subscription runs, in order. A handler's throw is reported and skipped.
* **Durable (`cursor`):** the handler runs in a transaction that advances a `$cursors` row, so its database effects happen exactly once per device and cursor name. `batch` events share a transaction. A throw stops the subscription (`sub.active`); a restart retries from the cursor.
* **A dropped connection** resumes from the last offset received: nothing missed, nothing repeated.

## 12. Consumers

```ts
db.consume({ group: 'billing', topic: orders, mode: 'per_key', maxInflight: 8 }, async (msg, tx) => {
  await tx.table('invoices').insert({ id: msg.body.id, … });
});
```

* **Modes:** `sequential`, `partitioned` (`partitions`), `per_key` (`maxInflight` at once), `single_key` (`key`).
* **Each delivery** is one transaction with the handler's writes, its emits and the consume step: exactly once per group.
* **A throw:** `retry: 'nack'` (default) redelivers at once, then the server DLQ after `maxAttempts` (or `onPoison: 'block'`). `retry: 'delay'` consumes the message and schedules a copy after `backoff(attempt)` (header `zen.attempt`); after `maxAttempts` it goes to the class DLQ `<topic>/zen.dlq` with the same id.
* **`dedup: true`** records each message id in `$inbox` and skips ids seen before (`dedupTtlMs`, default 7 days).
* **Manual acks:** `db.consumeManual(opts, (msg) => …)` with `msg.ack()`, or `db.begin({ consumes: msg })` and `commit()`.
* **Leases and claims** are renewed while the loop runs. A stalled handler loses them; another holder gets the event, and the stalled one's late commit fails (`not_leader`, `claim_lost`) with none of its writes.
* `handle.stop()` stops a consumer; `db.close()` stops all.

## 13. Ephemeral messages

`db.publishEphemeral(topic, type, body)` and `db.onEphemeral(topic, handler)`: never stored, best effort, rate-limited, only to current subscribers. For presence and typing indicators.

## 14. The scheduler and delayed messages

```ts
const id = await db.schedule(reminders, 'remind', { booking }, { at: start - 3_600_000 });
await db.cancel(id);                         // false once delivered
```

* A `$sched` row, inserted only if the transaction commits. The owner's scheduler (`DbOptions.scheduler`, on by default) delivers due rows while it holds the lease of `("zen", "db", ns, "sched")`: each row is appended in the transaction that deletes it, so it is emitted exactly once, never before `at` (by the session clock corrected to the server's).
* **Timing:** usually late by up to `pollMs` (1 s); after a leader dies, by its lease (`ttlMs`, 10 s). With no scheduler running, messages wait.
* The same rows carry `gc` jobs (body parts after `ttlMs`, dedup rows after `dedupTtlMs`).

## 15. Request/reply

```ts
db.serve({ group: 'quotes', topic: quotes, mode: 'sequential', dedup: true }, async (msg, tx) => {
  if (!msg.body.sku) throw new ReplyError('no sku');      // an error reply; the request is consumed
  return { price: await price(tx, msg.body.sku) };         // the reply, in the same commit
});
const r = await db.request(quotes, 'quote', { sku }, { timeoutMs: 5000 });
const all = await db.requestAll(presence, 'who', null, { timeoutMs: 500 });   // scatter-gather
```

* **Inbox:** each `Db` subscribes to its own inbox topic on first use. A reply's `corr` matches its request; late replies are dropped.
* **Errors:** `timeout` (counted from the commit), a `ReplyError` for an error reply.
* **Retries:** send again with the same `corr`; a `dedup` responder replies with its stored reply without running the handler again.
* **Ephemeral replies:** `serve({ reply: 'ephemeral' })`, or `db.reply(msg, body, { ephemeral: true })` from a plain `on` responder.
* **Inside a transaction:** `tx.request(…)` is sent only if the transaction commits. Await its promise after the transaction; an attempt that re-runs never settles.

## 16. Sagas

```ts
const order = db.saga('order', [
  { name: 'reserve', command: stock, compensate: release },
  { name: 'charge', command: payments, compensate: refund, timeoutMs: 30_000 },
  { name: 'ship', command: shipping },
]);
await db.transaction(async (tx) => {
  await tx.table('orders').insert(o);
  await order.start({ order: o.id }, tx);
});
const state = await order.get(id);    // {status, step, data, done, …}
```

* **Participants** are `serve` handlers on the command and compensate topics: return a body (merged into `data`), or throw `ReplyError` (stored in `data.error`).
* **On an error reply or a timeout**, the completed steps are compensated newest first, then the saga is `failed`. A compensation that fails, times out or reaches the class DLQ (participants with `retry: 'delay'`) leaves it `stuck` for an operator.
* **Every transition is one commit**, exactly once; replies to an older message (another `corr`) are ignored.
* The orchestrator runs in the owner; `saga.stop()` stops it.

## 17. Change events

A table created with `changes: {topic?, image?}` emits one `zen.change` message per changed row per transaction (`{op, table, pk, row?}`), keyed by the pk. `table.on(handler)` subscribes to them; consumers build projections from them. A CRDT table's events never carry the row (§19).

## 18. Guarantees and failures

| What | Guarantee |
|---|---|
| rows, index entries, emits, schedules and file ops of one transaction | all or nothing, once |
| a consumer handler's database effects | exactly once per group |
| a durable `on` handler's effects | exactly once per device and cursor |
| a scheduled message | emitted once, never early |
| a saga transition | exactly once |
| an external effect | at least once: use the message `id` as the idempotency key |

The failure cases of §13.2 are tested in `packages/db/test/faults.test.ts` (a stalled or deposed handler, racing claims, a dropped stream, a lost commit response, a scheduler dying mid-batch, a responder that doesn't answer) and `proxy.test.ts` (the owner tab closing).

---

# CRDT tables

## 19. Rows the server merges

A table created with `merge: 'crdt'` holds rows that the server merges field by field, on ciphertext (zendb.md §19). Writes never conflict and concurrent edits of different fields both survive; in exchange there are no unique indexes, no `put` or `update` (they read before writing), and no invariants across fields or rows. A transaction can still carry CRDT operations atomically with transactional rows, emits and consumes. It needs a server that lists `"crdt_rows"` in `/v1/info` (otherwise `unsupported`).

```ts
await m.createTable('cards', {
  pk: ['id'],
  merge: 'crdt',
  fields: { votes: 'counter', labels: 'set' },   // others are lww
  changes: {},                                     // image 'keys' only
});
const cards = db.table<Card>('cards');
await cards.insert({ id, title: 'Buy milk', votes: 0, labels: ['home'] });
await cards.patch(id, { title: 'Buy oat milk' });  // undefined unsets a field
await cards.incr(id, 'votes', 1);                   // may be negative
await cards.add(id, 'labels', 'urgent');
await cards.remove(id, 'labels', 'home');
await cards.delete(id);
```

| Field type | Merge |
|---|---|
| `lww` (default) | the latest write wins, by `(hlc, device)` |
| `counter` | each installation's total; the value is their sum |
| `set` | add-wins: `remove` takes away the copies it saw, an add it didn't see stays |
| the row | `insert` makes it exist, `delete` hides it; only `insert` brings it back |

* **`insert`** sets the fields given: `lww` fields, counters incremented by the value, set elements added. Fields not given keep their merged values.
* **Reads** (`get`, `query`) merge the server's state on the client: declared counters read 0 and sets `[]` until written. They see this instance's queued writes and the transaction's own operations, and are **not** in its read set: a transaction that reads a CRDT row never conflicts on it.
* **Queries** look up by pk, use an index (below), or scan the table (the predicates, `orderBy` and paging as for a scan).
* **Timestamps** of `insert`, `patch` and `delete` are the time of the edit, from the session clock. A commit refused with `stale_op` or `clock_skew` is sent again with fresh ones, after reading the server clock: an edit held offline past the server's horizon then wins over edits made since.
* **Counters** need an installation id (the clock's `actor`): a persisted clock store must keep it with the HLC (docs/CLIENT.md). One installation's counter commits go one at a time.
* **Change events** carry `{op, table, pk}`: `delete` when the transaction last deleted the row, `put` otherwise.
* **Dropping** a CRDT table deletes its rows in pages, with their index entries; the server purges the rows after its horizon.

### Writes and the queue

CRDT writes work offline: they go through a queue (`db.crdt`) that sends them when the server can be reached.

```ts
await cards.patch(id, { title: 'x' });                       // resolves once queued
await db.table<Card>('cards', { wait: true }).incr(id, 'votes'); // resolves once committed
await db.transaction(async (tx) => {                          // one entry, sent whole
  await tx.table<Card>('cards').patch(a, { status: 'done' });
  await tx.table<Card>('cards').incr(b, 'votes', 1);
}, { wait: true });
db.crdt.pending;          // entries not yet committed
db.crdt.online;           // whether the last attempt reached the server
await db.crdt.flush();    // the queue is empty (rejects `offline` while offline)
db.crdt.onSync((e) => render(e.online, e.sent, e.failed));
```

* **What is queued.** Each call on a CRDT `Table` is an entry; so is a transaction with CRDT operations only (and reads of its tables), which is sent whole, in one commit. A transaction with anything else (a transactional row, an emit, a consume) commits directly, after sending the queue first, so this instance's commits stay in order: offline it fails. `{queue: false}` on a transaction, or `DbOptions.crdtQueue: false`, commits CRDT-only transactions directly too.
* **Batches.** Entries go out in order, in as few commits as the server's limits allow (`crdtBatchMs`, default 10 ms, gathers them). Single calls next to each other are coalesced: the last `patch` of a field wins, increments add up, an `add` then a `remove` of one element cancel out.
* **Retries.** A commit that may not have arrived (a network error) is sent again as is, with the same `commit_id`, so it applies once. A `conflict` rebuilds it; `stale_op` re-stamps it (above). Any other error drops the entries it held: their `wait`s reject and `onSync` reports `failed`.
* **A dropped table**'s queued writes are dropped with a warning.
* **Removes** take the copies seen when called; the queue adds the copies its own earlier adds got, so removing an element added offline works.
* **The store** is in memory by default: the queue lasts as long as the process. `DbOptions.local` puts it on a sealed local store, so it survives restarts (below); `DbOptions.crdtStore` takes any other `QueueStore`.
* **`tx.after`** of a queued transaction runs once queued.

### A queue that survives restarts

```ts
import { indexedDbLocalStore, sealedStore } from '@zen/client';
const local = sealedStore(indexedDbLocalStore('my-app'));      // docs/CLIENT.md §10
const db = await Db.open(fs, 'app', { schema, migrations, local });
// …at sign-out:
db.close();
await Db.wipe(local, 'app');                                  // unsent writes too
```

* **What is kept:** every queued entry, from before its write resolves until it is committed or fails, and the **commit in flight**. A restart sends that commit again as it was, with the same `commit_id`, before anything else: if the first send applied and only its reply was lost, it still applies once.
* **Counter totals are never kept.** The server holds each installation's total; the class reads it when a send needs it. A total restored from a backup could otherwise roll a counter back.
* **Tabs.** Tabs that share an IndexedDB store share the queue: each stores its writes and shows them in its reads, and only the owner tab sends, telling the others over a `BroadcastChannel` when their writes are committed. When the owner closes, the next tab sends what is left. Without Web Locks every instance sends, so don't share a store then.
* **Counters across tabs.** A browser's tabs are one installation: they share its counter actor. With Web Locks, its counter commits take the Web Lock `zen/actor/<actor>` from stamping to the outcome, so two tabs incrementing one counter at once both count; each such commit reads the installation's totals (and their `seq`, so its own is newer) from the server first.
* **Layers.** The store's layers are the app's choice (device by default, fs too, or plaintext): docs/CLIENT.md §10. A record that doesn't open (another fs, another device key, tampered with) is dropped with a warning, which loses its write.

### Read modes

CRDT reads keep the states they fetch in a cache (`DbOptions.crdtCache`, default 1,024 rows). How a read uses it is its mode: `DbOptions.crdtRead`, `db.transaction(fn, {read})`, `get(pk, {read})`, `query().read(mode)`.

| Mode | Online | Offline |
|---|---|---|
| `auto` (default) | a state younger than `crdtCacheMs` (default 5 s) as cached; an older one checked by version (`/v1/crdt/versions`), and fetched again only if it changed | the cached state, however old |
| `cache` | the cached state, however old; the server only for rows not cached | the cached state |
| `server` | always the server | fails |

* **Misses.** Offline, a row that isn't cached is `offline`, unless only the queue writes it: then it reads from its queued writes.
* **Queries** in `auto` and `server` ask the server as usual; `auto` answers offline as `cache` does. A `cache` query answers from the cached rows and the rows only the queue holds, filtered, ordered and paged locally: it may miss rows never read.
* **This instance's commits** mark their rows stale, so its next `auto` read fetches them; another installation's writes show once the entry is checked.

### Indexes on CRDT tables

`fast`, `private` and `sealed` indexes work on CRDT tables, never `unique`, and the table must have `changes`. Writers can't maintain them, since the merged value exists only on the server: an **indexer** does. It is a `per_key` consumer group `$index` on the change topic, run by the owner like every consumer. For each change event it reads the row's merged state and moves its entries from the values it last indexed (kept in the system table `$ixrows`).

* **They lag.** A query through an index sees what the indexer has indexed: a row changed a moment ago may be missing from the result, or sit at its old place in the order, until the indexer catches up. With no instance running the indexer, the indexes stop.
* **They never lie.** Every predicate is re-checked on the merged row the query fetches, so a stale entry never returns a row that doesn't match.
* **A value of the wrong type** (text where the index says `int`) can't be refused, since the write already happened: that row is left out of that index, with a warning.
* **Creating an index** backfills the existing rows in pages; rows the indexer reached first are skipped.
* **`DbOptions.indexer: false`** keeps an instance from running the indexer (tests use it to make lag). An instance runs it for the tables that had indexes when it opened; migrations run at open, so that is the normal case.
* **In tests**, wait for the indexer by polling a query (or `$ixrows`) until it shows the change.


## 20. Loro documents

`@zen/loro` keeps [Loro](https://loro.dev) documents (rich text, lists, maps, movable trees) in a database ([`spec/zendb.md`](../spec/zendb.md) §20). They merge on the client: the server stores sealed updates and snapshots, and sees neither the content nor what an edit did.

```ts
import { openLoro } from '@zen/loro';

const card = await openLoro(db, `card/${id}`);   // db: an open Db
await card.ready;                                // caught up (or offline)
card.doc.getText('body').insert(0, 'Hello');
card.doc.commit();                               // emitted shortly after
card.onChange(() => render(card.doc.toJSON()));  // edits from elsewhere
await card.flush();                              // optional: wait until emitted
await card.close();
```

* **Where it lives.** Each document has its own topic, `("zen", "loro", ns, name)`, carrying `zen.loro.update` messages. The `$loro` system table holds a snapshot per document and the offset of the last event it covers.
* **Loading.** The kept state on the device, then the `$loro` snapshot if it is newer, then the events after it, then live ones. `ready` resolves once that is done, or once the server couldn't be reached; the handle keeps trying with backoff (and on `flush()`). `online` tells which.
* **Edits** are stored in an outbox before they are sent, gathered for `batchMs` (default 50 ms), merged into one update, and emitted in one commit. `pending` counts the unsent ones; `flush()` waits for them, and rejects with `offline` when the server can't be reached.
* **Offline.** With `DbOptions.local`, the document's state and its outbox are kept in the store `loro/<ns>` (even with `persist: false`): a document opens offline, and edits made offline survive restarts and go out once online. Without a local store the outbox is in memory, so `flush()` before `close()`. `Db.wipe` clears it.
* **Resends are harmless.** An edit sent twice (a crash or a lost reply between the commit and the outbox delete) is ignored by Loro the second time.
* **Compaction.** The owner of each open database writes a snapshot to `$loro` after `snapshotEvery` events (default 100) or `snapshotBytes` of updates (default 1 MiB), and `compact()` writes one now. The offset only moves forward, so two writers never undo each other and no leader is needed.
* **Limits.**
  * A snapshot must fit one commit (`max_commit_bytes`, 8 MB by default). A larger one keeps the stored snapshot, warns once and stops compacting that document; loads then read more of the log (TD-LORO-LARGE-SNAPSHOT). `compact()` fails with `too_large`.
  * A single update too large for a commit (a very large paste) can't be sent: it is dropped with a warning.
  * Events stay in the log after a snapshot covers them (TD-LOG-RETENTION).
* **Tabs.** Each tab has its own `LoroDoc` and sends its own edits; live events reach other tabs through the owner's stream (§9.2), and only the owner compacts.
