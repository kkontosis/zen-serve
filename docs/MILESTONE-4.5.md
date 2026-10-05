# Milestone 4.5 plan: zen-db and the embedded broker, one client class (specs only)

## Context

M4 shipped `@zen/client`: KV with both transaction modes, topics, consumer groups, leaders, the stream and the CRDT filesystem. M5 builds zen-db on it. The user wants a step in between that only writes the specification:
* zen-db
* an **in-app broker**
* one client class that unifies them, with app examples.

**The user's words:**
* "add new milestone in docs: zendb and embedded broker class client specs and some app examples / poc targets"
* "New M4.5, between 4 and 5. The difference with 5 is that this milestone is spec-only."
* "in-app broker class. the library itself, offering messaging patterns built on topics and consumer groups: publish/subscribe, work queues, request/reply, delayed messages and sagas. It runs inside the app's own process. However it uses the already designed server side broker API primitives and protocol."
* "ideally implemented in the same class as zendb so zendb can act both as CRUD and as emit and subscribe and consume events and its objects and TRANSACTIONS … synchronize the db and the broker messages simultaneously."

**What that means for the specs:**
* **One commit per transaction.** Row writes, emitted events, consume steps (and fs ops) of a transaction go into one `/v1/commit`. They apply all together or not at all (api.md §6; DESIGN-2 §1 "transactional outbox for free").
* **No server changes.** Every pattern is built from the primitives that exist today. Anything missing is recorded as a gap or TD; it is not designed around silently and not built.

**Decisions (user):**
* **Files.**
  * One normative spec, `spec/zendb.md`, covering db, broker and the unified API.
  * App examples in `docs/EXAMPLES.md`.
  * New labels are registered in `spec/labels.md`. `formats.md` §8 points to zendb.md.
* **Proof-of-concept targets** (specified here, built in M5):
  1. kanban board with attachments (files)
  2. family chat
  3. slot booking
  4. checkout saga
  5. credits ledger
* **Delayed messages: a scheduler leader.**
  * A delayed message is a row in a due-time index, written in the sender's transaction.
  * A leader (a `sequential` group's lease) polls for due rows. In one fenced transaction it appends them and deletes the rows.
  * Server timers are recorded as a gap.
* **Change events: opt-in per table.** A table may declare a change topic. Each put or delete then appends a sealed change event in the same commit.
* **Unique indexes: token key + expect.**
  * Key: `(ns, "u", idx, PRF(value))` → pk.
  * Inserts check that the key is absent (short mode: read conflict; long mode: `expect` version null).
  * Leakage is the same class as a primary key.
* **Private index: a prolly / Merkle-search tree now** (instead of a plain B+tree). It is content-defined and history-independent, and M6's authenticated tier can reuse it.
* **Naming: emit / on.**
  * `db.transaction`, `tx.emit`, `db.on(topic)`, `db.consume(...)`.
  * This replaces docs/API.md's `transact`/`publish` draft names. A note goes in API.md.
* **In scope as well:**
  * the `auto` transaction mode
  * reserved hooks for M6 integrity
  * request/reply inboxes
  * multi-tab (G16)

**Out of scope:**
* code, the server, test-vector generation (M5, through zen-core `gen_vectors`)
* zen-fs replica and Loro (M5)
* the Merkle tier's design (M6; this milestone only reserves hooks)
* SQL
* release notes
* the earlier rejected draft (`977aa82`): not reused

**After compaction, re-read:**
* `docs/MILESTONE-4.5.md` (this plan)
* docs/API.md §2; DESIGN-2 §1, §2.3–2.7; DESIGN-3 §2–3, §5
* DESIGN-4 §1; GAPS G6–G9, G11, G16, G21, G23
* spec/api.md §1–2, §5–9
* spec/formats.md §3–5, §8; spec/keyspace.md §3.2–3.3; spec/labels.md; spec/TECH_DEBT.md
* docs/CLIENT.md §5–7
* packages/client/src/{kv,log,stream}.ts

**Branch:** `claude/brave-knuth-gk03mm`, which has PR kkontosis/zen-serve#9 open. Commits land there; no new PR unless asked.

## Hard facts the specs must respect (verified in code)

* **Keys and values**
  * KV keys are one-way PRF tokens, 16 B per element. Rows carry their pk inside the sealed value.
  * The server has no value order, so order queries need the private index.
* **Limits**
  * `max_value_bytes` 90,000 (row, node and event size; overflow by chunking). `max_commit_bytes` 8 MB. `max_commit_ops` 10,000.
  * `max_range_items` 10,000, which also caps clears and long-mode `expect_ranges`. A long-mode range must fit one response.
* **Transactions**
  * Short mode lives about 5 s. The client re-runs the whole function on `conflict`/`too_old`, so handlers must be side-effect free.
* **Consumer groups**
  * Group names are per fs (`pack("cg", fs, group)`), so the class must derive names that are unique per topic.
  * Groups are immutable (G8), at most 64 per topic. A `per_key` group at `earliest` returns 413 past 100,000 events.
  * `claim_ttl` is 30 s.
* **Leases and claims**
  * They are held per **device**, not per process or tab.
  * Expiry is in commit versions, which may run ~2 s late.
* **Delivery and fencing**
  * Delivery is pull (`/v1/consume/next`, `wait_ms` ≤ 30 s), not pushed over the WebSocket (DESIGN-4 §1.4's push was not built).
  * Only a commit that carries a consume step is fenced.
* **Missing server features**
  * No timers.
  * The log is never trimmed (TD-LOG-RETENTION).
* **Sealing**
  * The event AEAD binds fs, topic and key_token, so a DLQ retry re-appends the envelope unchanged.
  * The event body is `{sender, hlc, causation, payload}` (formats.md §5).
* **Fs ops in a commit**
  * Chunks and `crdt_ops` can ride in the same commit (G11: in short mode they conflict like reads).
  * At most one `write` per node per commit.

## Step 0: persist the plan
* Write `docs/MILESTONE-4.5.md` from this plan.
* Replace the DESIGN-3 §6 item 4.5 placeholder with a link, like 3.5 and 4.
* README status stays "planned".
* Commit.

## Step 1: `spec/zendb.md` part A, the database (⚠️ SPEC CHANGES, `spec:` commits)

1. **Model.** A database is `(fs, ns)`. All keys sit under the KV naming chain `(ns, …)` (formats.md §3.2).
2. **Catalog (G6).**
   * Reserved element `("cat")`, holding sealed records for: tables, indexes (kind, fields, unique, change topic), schema version, migration log.
   * Clients check the schema version on open and refuse a newer one.
   * Migrations are resumable client jobs:
     1. build the index in pages, each page one transaction
     2. flip it to active in one transaction
   * Leases are optional, to avoid duplicate builders.
3. **Rows.**
   * Key `(ns, "t", table, pk)`.
   * Value: deterministic CBOR (G3) `{pk, fields…, v}`, sealed as kind 1.
   * Optional size-bucket padding.
   * Oversize rows go to overflow keys `(ns, "o", table, pk, i)`, written and read in the same transaction.
4. **Indexes.**
   * **Unique.**
     * Layout: `(ns, "u", idx, PRF(value)) → pk`.
     * Rules: absence is checked on insert; the old key is deleted and the new one written on update.
     * Leakage: existence only.
   * **Fast.**
     * Layout: `(ns, "f", idx, PRF(value), PRF(pk)) → ∅`, one range read per equality lookup.
     * Leakage: the count of rows per value (DESIGN-3 §2.5).
   * **Private: prolly / Merkle-search tree.**
     * Entries are sorted by an order-preserving encoding of `(value, pk)`, inside sealed nodes.
     * Boundaries are chosen by a keyed hash of the entry, so the server can't predict them.
     * A node's id is a keyed hash of its plaintext, which makes the tree history-independent and shares unchanged subtrees.
     * Node size targets ~4 KiB and must stay under `max_value_bytes`.
     * The root pointer lives in the catalog.
     * A write rewrites one root-to-leaf path and deletes the replaced nodes in the same commit (client-side GC). Old nodes are never shared across indexes.
     * The spec states the **contention trade-off**: every writer of one index conflicts on the root pointer. Mitigations include optional sharding of the index into K trees by `PRF(pk) mod K`.
     * **Leakage:** node counts, tree depth, and which nodes change per write.
5. **Queries.**
   * A builder with `where` (eq, range, prefix), `orderBy` (index order only), `limit` and cursor paging.
   * Plans are chosen client-side: pk lookup → unique → fast → private → full table scan, with a warning on the scan.
   * No joins; the examples show app-side joins.
6. **Transactions.**
   * **Short and long modes** are mapped to M4's `Transaction`. Every index node read goes into the read set, or into an `expect` (G9).
   * **`auto`:** start short; on `too_old`, or once elapsed time passes a threshold, re-run in long mode. Long mode's one-response ranges limit what `auto` can switch.
   * **Retry** rules come from api.md §1.
   * **Cache coherence (G9):** cached nodes and rows keep their version. A stale cache costs a retry, never a wrong commit.
   * An optional invalidation subscription can be added on the change topic.
7. **Multi-tab (G16).**
   * Per browser origin, one tab owns the session connection, the leases and the caches, elected through `navigator.locks`. A SharedWorker is used where available.
   * Other tabs proxy calls through `BroadcastChannel`.
   * Leases are per device, so two tabs that each campaign would be the same holder. The spec forbids that and routes all leasing through the lock owner.
8. **M6 hooks.**
   * Reserve a catalog flag `integrity: "basic" | "authenticated"`.
   * Reserve the label `zen/v1/sig/db-root`.
   * Note that prolly node ids become the Merkle hashes.
   * No design.
9. **Leakage section** for the database: keys per table, row sizes, index shapes, access timing.

## Step 2: `spec/zendb.md` part B, the broker

1. **Message envelope.** A CBOR header inside the event `payload` (formats.md §5):
   ```
   {type, id(16), corr?, reply_to?, deliver_at?, saga?, step?, attempt?}
   ‖ body
   ```
   * `causation` is filled from the consumed event automatically.
   * Large bodies overflow to fs chunks or KV rows.
2. **Group naming.** A group's server name is `PRF(topic_id ‖ app_group_name)`, so the same app name works on any topic.
3. **Primitives of the class:**

   | Method | What it does |
   |---|---|
   | `tx.emit(topic, msg, {key})` | an `append` in the transaction's commit |
   | `db.emit` | an autocommit emit |
   | `db.on(topic, handler, {after})` | broadcast: stream subscription with a persisted cursor |
   | `db.consume({group, topic, mode, …}, handler(msg, tx))` | a consumer group: `next` long-poll, then the handler's transaction carries the consume step; nack and the DLQ on throw |

4. **Patterns.** Each one names the primitives it uses, its commits, and its guarantee, argued against the DESIGN-3 §3.2 failure table:
   * **publish/subscribe:** broadcast via `on`; durable fan-out via one group per subscriber.
   * **work queues:** `per_key` for keyed work, or `partitioned(N)` with N workers. A single unkeyed queue is limited to one active worker (`sequential`); that limit is recorded as a gap: competing consumers for unkeyed events.
   * **request/reply:**
     * Each device or app instance has an inbox topic under `inbox/<instance>`, created with a grant shape.
     * The request carries `reply_to` and `corr`. The responder consumes, then emits the reply in the same transaction.
     * The requester waits through `on(inbox)`, with a client timeout and a correlation map.
   * **delayed messages:** the scheduler leader (see Decisions). It is idempotent through a `(row → append)` single commit. The schedule index uses the zen-db private index on `deliver_at`.
   * **sagas:**
     * Saga state is a zen-db row.
     * Each step = consume + state update + emit next command, in one transaction.
     * Compensations run on failure or DLQ.
     * Timeouts are delayed messages.
     * External effects are at-least-once with the idempotency key `(topic, offset)` (DESIGN-3 §3.3).
   * **outbox and inbox:** both are built in, through commit atomicity and consume-step dedup.
   * **change events:** opt-in per table, as in the Decisions.
5. **Guarantees table:** effectively-once for db-side effects, at-least-once for external effects, ordering per mode.
6. **Leakage section** for the broker: topic shape, event counts and sizes, key_token linkage, inbox topology, schedule timing.

## Step 3: `spec/zendb.md` part C, the unified class API (informative)

* A TypeScript surface: `zen.db(fs, ns)` → `Db`, with:
  * `table`
  * `transaction(fn, {mode})`
  * `begin({mode, consumes})`
  * `emit`, `on`, `consume`
  * `request`, `schedule`, `saga`
* Mapping tables:
  * every method → the M4 calls it uses (`Transaction`, `Topic.appendIn`, `Consumer.ack(d, tx)`, `Stream.subscribe`, `Leader`)
  * every method → the api.md endpoints and commit fields it produces
* One worked example where a single commit carries row writes, index node writes, an emit, a consume step and a fs `write` op.

## Step 4: `docs/EXAMPLES.md`, the five proof-of-concept targets
For each target:
* the schema (tables and indexes)
* the topics and groups
* the transactions, as commit contents
* the patterns used, the ACL grants needed, the failure behaviour
* what M5 must demo

| Target | What it exercises |
|---|---|
| **Kanban with attachments** | cards and columns as rows (ordered by the private index); attachment upload as chunks + fs `write` in the same commit as the card row; change events for live boards |
| **Family chat** | rooms as topics, `on` with resume, presence over ephemeral, read receipts as rows, request/reply |
| **Slot booking** | unique index for the slot, a waitlist work queue, reminders via delayed messages |
| **Checkout saga** | orders → payment → shipping, compensations, timeouts, idempotency keys for external effects |
| **Credits ledger** | balances with invariants (short txn), an audit outbox, a projection from change events |

## Step 5: gap and tech-debt sweep, docs
* **New TDs** (names to confirm while writing):
  * `TD-BROKER-SERVER-TIMERS`: delayed delivery without polling
  * `TD-CONSUME-COMPETING`: unkeyed competing consumers
  * `TD-CONSUME-PUSH`: consumer delivery over the WebSocket (DESIGN-4 §1.4)
  * `TD-CONSUME-PROCESS-HOLDER`: leases per device, not per process
* Note that change events and outboxes make `TD-LOG-RETENTION` more pressing.
* **GAPS:** G6 and G9 marked decided (pointing to zendb.md); G16 decided.
* **Other files:**
  * `formats.md` §8 points to zendb.md
  * `spec/labels.md`: new labels
  * `spec/glossary.md`: database, catalog, message, saga
  * `docs/API.md` §2: a note on the renamed methods
* **Milestone docs:** `docs/MILESTONE-4.5.md` Outcome; README marks 4.5 done; the DESIGN-3 §6 M5 line notes "implements spec/zendb.md".

## Commits
1. plan doc + the DESIGN-3 link
2. `spec:` zendb.md part A (database) + labels
3. `spec:` zendb.md part B (broker) + part C (API)
4. docs/EXAMPLES.md
5. TDs, GAPS, glossary, formats §8, API.md note, Outcome, README

Push after each commit.

## Verification (review checklist; spec-only)
* **Patterns use existing primitives only.**
  * Every pattern maps only to endpoints and commit fields in api.md §5–9.
  * No new server field is needed; each gap has a TD.
* **Limits.** Every commit shape stays within the limits (ops, bytes, value size, long-mode range size).
* **Failure cases.** Every guarantee is argued against DESIGN-3 §3.2's cases: stall, late leader, race, partition, plus a client crash between the commit and the response (`commit_id` replay).
* **Byte layouts.** Each new layout has a test-vector plan for M5, and each new label is registered.
* **Leakage.** There is a leakage section per part.
* **Respected facts.** Per-fs group names, per-device leases and no WS push are all respected.
* **Docs build.** Links resolve (`grep` the anchors); the docs have no model names.
