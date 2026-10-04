# Milestone 4.5 plan: zen-db and the embedded broker (specs only)

## Context

Milestone 4 delivered `@zen/client`. It is the full set of server primitives as a library: KV with short/long transactions, topics, consumer groups with leases and fencing, the stream, and the CRDT filesystem. What apps need on top of it is still only sketched:
* **zen-db** (DESIGN-3 §2.5, the API.md draft): tables, indexes, queries.
* **A broker**: the messaging patterns apps build from topics and groups.

Both were discussed at length across the design docs, and they share one key property: **one commit can write KV and append events and consume an event, atomically** (DESIGN-2 §1, the transactional outbox "for free"; DESIGN-3 §3.1, read-process-write with fencing).

Milestone 4.5 **specifies** both, normatively, before any code: one client class that is a database and a broker at once. Milestone 5 then implements it, next to the zen-fs replica and the Loro adapter. 4.5 also defines a set of example apps (proofs of concept) that M5 builds to prove the design.

**Decisions (user):**
* **Specs only.** No code and no server changes in this milestone. If a pattern needs something the server doesn't have, it is recorded as a gap (GAPS.md, TECH_DEBT.md), not built.
* **An in-app broker**, inside the library and the app's process. It offers:
  * publish/subscribe
  * work queues
  * request/reply
  * delayed messages
  * sagas

  It is built only on the server's existing broker primitives and protocol (topics, keyed appends, consumer groups and their modes, leases and fencing, consume steps, the DLQ, the stream, ephemeral messages).
* **One class for both.** `ZenDb` does CRUD, emits events, subscribes to them and consumes them. Its objects (rows) and its **transactions** span the database and the broker, so db writes and messages commit together, or not at all.

**Out of scope:**
* any code
* any server change
* the zen-fs replica and Loro (M5)
* SQL
* the authenticated (Merkle) integrity tier (M6): the specs reserve its hooks
* standard broker protocols such as MQTT, AMQP or Kafka (a later sidecar could expose `ZenDb` through them)

**After compaction, re-read:**
* `docs/DESIGN-2.md` §1–2, `docs/DESIGN-3.md` §2–3, `docs/DESIGN-4.md` §1, `docs/API.md` (the client sketch), `docs/GAPS.md`
* `spec/api.md` §5–9, `spec/formats.md` §3–5 and §8, `spec/keyspace.md`
* `docs/CLIENT.md` §5–7: what M4 actually provides
* `packages/client/src/{kv,log,stream}.ts`: the primitives the specs compose

## Step 0: persist the plan
Write `docs/MILESTONE-4.5.md` (this plan) and add 4.5 to DESIGN-3 §6.

## Step 1: `spec/db.md`, zen-db (normative)

**Model**
* A **database** is a namespace inside an fs: `db(name)`, with the KV path `("db", name, …)`. It holds tables, indexes, a catalog, and its topics (step 2).
* **Tables:** `(db, table, pk) → row`.
  * The primary key is a tuple of typed elements: string, int, bytes or uuid.
  * A row is one sealed KV value (formats.md §4 kind 1). Its plaintext is a **row document**: canonical CBOR of `{pk, v: schema_version, cols: {…}}`. The pk is inside the row because stored keys are one-way (DESIGN-3 §2.2).
* **Large rows** (over `max_value_bytes`) are split into overflow values `(… pk, "ovf", i)` written in the same commit. A row's size limit is the commit's.
* **Optional padding** to size buckets (DESIGN-3 §2.3), per table.

**Catalog**
* An encrypted schema record per table, holding:
  * columns and types
  * the pk definition
  * indexes and their kinds
  * the event settings (step 2)
  * a version
* It lives in KV under `("db", name, "$catalog", table)`.
* Schema changes are transactions that bump the version. Rows carry the version they were written under, and readers upgrade on read with app-supplied migrations (lazy migration). There is no server-side DDL.

**Indexes** (DESIGN-3 §2.5)
* **Private** (the default): an encrypted B+tree. Its nodes are KV values under random ids, sealed, with the upper levels cached on the client. It supports equality, range and order, and hides how many rows share a value.
  * The spec fixes the node format, fan-out, split and merge rules, and how node edits join the row's transaction.
  * **Concurrency** comes from transaction conflicts on the nodes a write reads.
  * **Hot root:** a split or merge that touches the root conflicts with every concurrent writer, so the spec bounds this with node-level conflict ranges and documents the write throughput per index.
* **Fast** (opt-in): `(db, idx, PRF(value), PRF(pk)) → ∅`. One round trip for equality. It leaks how many rows share each value, and says so.
* **Unique**: a `(db, idx, PRF(value))` claim key written with `expect: absent`. It leaks equality only when there is a collision, which is an attempted duplicate. Documented.
* **Index maintenance happens in the row's own commit**, so an index is never stale relative to its rows.

**Transactions**
* Both modes of DESIGN-3 §2.4, plus `auto`: try short first, and fall back to long when the work outlives the window.
* Retries re-run the function, so it must have no side effects outside the transaction.
* **Reads see the transaction's own writes.**
* A query inside a transaction registers the ranges it read: index nodes and rows in short mode, expects and range hashes in long mode. That gives serializability with phantoms covered.

**Queries:** a client-side builder with `where` (equality and ranges on indexed columns, other filters applied after fetching), `orderBy` (index order only), `limit` and cursor paging.
* The plan is chosen by fixed rules, not a cost model: a unique index, then the fast index, then the private index, then a scan.
* `explain()` shows the plan and what it leaks.

**Integrity:** the Basic level (AEAD; single-row rollback is possible, DESIGN-3 §2.5) now. The catalog reserves an `integrity: "authenticated"` flag for M6.

**Leakage section:** table and row counts, row sizes (unless padded), access patterns, fast-index equality, unique-index collisions, B+tree shape (node count, depth).

## Step 2: `spec/broker.md`, the embedded broker (normative)

The broker is a **client-side layer over the existing primitives**. Each pattern maps to server operations listed in the spec, with no new endpoints. Topics belong to a database: `db.topic(path)` uses the db's naming chain, so its events sit under the same keys and ACL as its rows.

**Event envelope.** One canonical event body is defined on top of formats.md §5's `{sender, hlc, causation, payload}`. The payload holds CBOR `{type, id, data, headers?}`:
* **`id`** is a random 16-byte message id, used for deduplication by receivers.
* **Headers** carry:
  * `correlation`
  * `reply_to` (a topic id)
  * `deliver_at` (unix ms)
  * `attempt`
  * `saga` (instance id and step)
  * `expires`
* The `causation` of an event emitted while consuming another is set automatically to the consumed event's offset.

**Patterns**

| Pattern | Built from | Guarantee |
|---|---|---|
| **Publish/subscribe** | `topic.emit` (append, inside a db transaction or alone); `subscribe` over the stream, resuming from a saved cursor; `broadcast` groups for durable subscribers | at-least-once delivery to subscribers, in order per topic; exactly-once *effects* when the subscriber's handler commits through `consume` |
| **Work queues** | `per_key` groups (parallel across keys, ordered per key) or `partitioned(N)`; competing consumers in-app and across devices; `nack` with backoff, the DLQ after `max_attempts` | each message's effect lands exactly once (the consume step in the handler's transaction); poison messages go to the DLQ |
| **Request/reply** | the requester owns a reply topic (`reply_to`); the request goes to a service queue; the responder's handler commits its writes, the reply append and the consume step together; the requester matches by `correlation`; optional `ephemeral` replies for low latency (best effort) | a reply exists iff the request's effects committed; timeouts on the requester |
| **Delayed messages** | a db-owned **schedule table** `(due_ms, id) → message`, written in the sender's transaction; a **scheduler leader** (a sequential group's lease) scans due entries and, in one commit, deletes them and appends to the target topic | delivered once, not before `deliver_at`; a lag bound set by the scan interval; survives restarts. The server has no timers: a client leader is the clock (recorded in GAPS) |
| **Sagas** (process managers) | saga state is a row; each step is a handler consuming an event, updating the row, and emitting the next command or a compensation, in one transaction; timeouts are delayed messages | each step happens exactly once; compensations run on failure or timeout; the state is always consistent with the messages sent |
| **Outbox / inbox** | built in: emitting inside a db transaction is the outbox; `consume` in the handler's transaction is the inbox | no dual-write problem |
| **Change events (CDC)** | a table can declare `events: true`: every put or delete also appends a change event (`{type: "row.put" / "row.delete", pk, row?}`) to the table's topic, in the same commit | subscribers can rebuild or sync a table (replicas, caches, views) from its topic, in commit order |

**Handlers** (`db.on(group, topic, handler, {mode, concurrency, retry})`)
* The broker takes the lease or claims, long-polls with `next`, and runs `handler(event, tx)` inside a `ZenDb` transaction that already carries the consume step.
* After the handler returns, the commit applies the handler's row writes, its emits and the cursor advance atomically.
* Fencing (DESIGN-3 §3.1) makes a deposed leader's commit fail as a whole.
* Ordering, concurrency limits, backoff and the DLQ follow the group's mode (DESIGN-4 §1.2).

**Exactly-once, stated precisely:** effects inside zen-serve (rows, emits, cursors) happen exactly once. External side effects are at-least-once, with the idempotency key `(topic, offset)` or the message `id` (DESIGN-3 §3.3).

**Leakage section:** topic ids and sizes, key-token equality per topic, group and lease activity (who processes what, and when), schedule-table size, and the timing of due entries (the server sees `(due_ms)` only if it is in a plaintext key: the spec puts it inside a sealed value and uses a PRF-ordered bucket instead, and states the trade-off).

## Step 3: the `ZenDb` class (API reference, non-normative)

One TypeScript surface, written as `.d.ts`-style signatures with semantics, in `docs/ZENDB.md`. It extends the draft in API.md:

```ts
const db = await zen.db('shop', { fs: 1 });
const orders = db.table<Order>('orders', { pk: ['id'], indexes: { byUser: { on: 'userId' } }, events: true });
const payments = db.queue<PayCmd>('payments', { mode: 'per_key', key: (m) => m.orderId });

await db.transact(async (tx) => {
  await tx.put(orders, order);                  // row + its change event
  tx.emit(payments, { orderId: order.id, amount }, { deliverAt: Date.now() + 60_000 });
});

db.on('billing', payments, async (msg, tx) => { … tx.put(…); tx.reply(msg, result); });
const res = await db.request(payments, cmd, { timeoutMs: 5000 });
db.saga('checkout', { start: 'order.placed', steps: { … }, compensate: { … }, timeoutMs: … });
orders.changes({ after }).subscribe(…);          // CDC
```

* Each call is mapped to the M4 client calls it uses (`Transaction`, `Topic`, `Consumer`, `Leader`, `Stream`), and to the server operations behind them.
* An appendix lists, for each method, the commit it produces: its keys, appends, consume steps and expects.

## Step 4: proofs of concept (`docs/POC.md`)

Each target is an app M5 builds on `ZenDb` (and zen-fs where noted). For each one the doc gives:
* what it proves
* the features and patterns it exercises
* acceptance tests
* what the server learns

| PoC | Proves |
|---|---|
| **1. Encrypted notes and todos** (browser, passkey sign-in with PRF unlock) | CRUD, private indexes (by tag, by due date), live sync across devices through table change events, offline edits retried on reconnect |
| **2. Family chat** | publish/subscribe, history after a cursor, read receipts as rows, ephemeral typing indicators, member add/remove with key rotation |
| **3. Shop checkout** | the saga (reserve stock, then charge, then ship, with compensations); exactly-once payment handling; a delayed message for the payment timeout; idempotent calls to a fake external payment API |
| **4. Background jobs** | a work queue with competing workers in Node and browser tabs (Web Locks for one leader per browser); retries with backoff; DLQ and replay; a delayed and recurring job scheduler |
| **5. Password vault** | unique and fast index trade-offs, made explicit; keyslots per device and passkey; revocation by rotation; a recovery key |
| **6. Photo library** | zen-mount for the files plus a zen-db index of their metadata (dates, albums), kept consistent through file change events; a request/reply thumbnail service |
| **7. Telemetry ingest** | `partitioned(N)` consumers aggregating per device into rows; request/reply commands to devices; throughput and latency numbers for STATS |

## Step 5: gaps and the server check

Walk every pattern and PoC against `spec/api.md`, and record each thing the server lacks, rather than designing around it silently. Expected candidates:
* event retention and compaction behind a snapshot (`TD-LOG-RETENTION`, which a CDC topic makes pressing)
* server-side timers for delayed messages (the client scheduler is the M5 answer)
* group names scoped per fs rather than per topic
* the limit of 64 groups per topic for fan-out
* a lease held by one device per group (for in-app concurrency the broker uses `per_key` or several groups)

Each gap is written up in GAPS.md with a proposal, or as a TD with its workaround.

## Commits
1. plan doc and the DESIGN-3 §6 entry
2. `spec:` db.md (zen-db), plus the formats.md §8 pointer (catalog and row encodings now specified)
3. `spec:` broker.md (envelope, patterns, handlers, leakage)
4. docs/ZENDB.md (the class API) and docs/POC.md
5. gaps and TDs found, glossary terms (row document, catalog, overflow, schedule table, saga, CDC)

## Verification (a review, as there is no code)
* Every API method in ZENDB.md maps to M4 client calls and server endpoints that exist today, or to a recorded gap.
* Every byte layout in db.md is defined precisely enough for test vectors (M5 generates them in `spec/test-vectors/db.json`, as for the other formats).
* Every pattern's guarantee is argued from the commit it produces (which consume step, which expects), including the failure cases of DESIGN-3 §3.2: stall, late commit, race, partition.
* Each PoC has acceptance tests that M5 can run against the embedded server.
* Each new spec has a leakage section.
