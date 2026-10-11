# Milestone 5 plan: zen-db, the in-app broker, CRDT tables, the zen-fs replica, Loro, and five apps

## Context

Milestone 4.5 specified the client database and broker (`spec/zendb.md`, PR #10) and later added two things (PR #11, both merged):
* index options that leak less
* server-merged CRDT tables, whose server side was moved into milestone 5

The app targets are in `docs/EXAMPLES.md`. Milestone 5 (DESIGN-3 §6) builds:
1. the `Db` class of zendb.md, parts A, B and D: database, broker and CRDT tables
2. the server side of CRDT tables
3. the zen-fs local replica, with `zen-mount` moved onto it
4. the Loro adapter
5. the five proof-of-concept apps, as small web apps

**Decisions (user):**
* **Apps:** five small web apps (kanban, chat, booking, checkout, ledger), each with a minimal browser UI. The checkout services also run as Node workers.
* **Indexes:** unique, fast, private (prolly tree, with shards and decoys) and sealed. **Not oblivious:** it becomes TD-DB-OBLIVIOUS-INDEX.
* **Packages:**
  * `packages/db` (`@zen/db`): database, broker and CRDT tables, on `@zen/client`
  * `packages/fs` (`@zen/fs`): the replica
  * `packages/loro` (`@zen/loro`)
* **Replica:** a pluggable store: memory, Node disk, IndexedDB. `zen-mount` moves onto it, which resolves TD-FUSE-REPLICA.
* **CRDT tables:** the server side is built in this milestone (DESIGN-3 §6, TD-CRDT-ROWS-SERVER).

**Out of scope:**
* the oblivious index
* M6 integrity: the authenticated tier, signed roots
* event signatures
* the server-side timer, competing consumers and consumer push (their TDs stay open)
* log retention
* release notes
* SQL

**After compaction, re-read:**
* `docs/MILESTONE-5.md` (this plan)
* `spec/zendb.md` (whole), `docs/EXAMPLES.md`
* `spec/api.md` §1–2, §5–9, §12; `spec/fs.md`; `spec/formats.md` §3–5, §11; `spec/keyspace.md` §3.3, §3.6; `spec/labels.md`
* `docs/CLIENT.md`
* `packages/client/src/{kv,log,stream,tree,fs}.ts`
* `crates/zen-server/src/{commit,tree,txn,keys,lib}.rs`; `crates/zen-proto/src/lib.rs` (Commit, CrdtOp ~1389–2090)
* `crates/zen-wasm/src/{lib,wire,crypto}.rs`; `crates/zen-core/src/{kdf,token,seal,keys}.rs`
* `crates/zen-server/tests/{common/mod.rs,fs.rs}`; `packages/client/test/helpers.ts`; `vitest.config.ts`; `.github/workflows/ci.yml`
* `spec/OPS.md` §4

**Branch:** `claude/brave-knuth-gk03mm`. PRs #10 and #11 are merged, so first restart it: `git fetch origin main && git checkout -B claude/brave-knuth-gk03mm origin/main`, then push with `--force-with-lease`. Open a new short PR at the end.

## Progress and resuming

**Rhythm (user request): one step at a time, a PR per step, compact in between.** After each step:
1. Run the verification relevant to the step (below).
2. Update this section: the step's line in the log, with its PR and anything the next step must know.
3. Commit and push to `claude/brave-knuth-gk03mm`.
4. PR it:
   * If the branch's previous PR is merged, the next step starts from main: `git fetch origin main && git checkout -B claude/brave-knuth-gk03mm origin/main`, then push with `--force-with-lease`. Open a new short PR.
   * If it is still open, the step's commits join that PR; update its title and body to list the steps it holds.
5. Stop, so the user can compact. After compaction: re-read this file (the log first), then the files under "After compaction, re-read" that the next step touches.

**Log:**
* **Step 0** (plan): done, commit `2a15230`.
* **Step 1** (spec): done. keyspace.md §3.8 (`co`, not `cr`), api.md §6 CrdtOp variants and `set_dots`, api.md §13 (CRDT rows), zendb.md §1 CBOR rules, formats kind 7. Decisions beyond the plan (amended after review, see step 3): a `row`/`lww` op with a `ts` equal to the stored one is a no-op when identical and `stale_op` (rebase) when it differs; counter entries are per `(device, actor)`, the actor created once per installation, and a `seq` not newer is an already-applied re-send, ignored; GC needs a row register, so the class always inserts before writing fields.
* **Step 2** (zen-core, zen-wasm, vectors): done. See "As built" under Step 2. `spec/test-vectors/zendb.json` exists; the TS encoders of steps 5–7 must reproduce its `cbor`, `sort_keys`, `rows`, `prolly`, `sealed_index`, `crdt` and `broker` sections.
* **Step 3** (server CRDT rows): done; steps 0–3 are PR #12. Amended on review: equal-timestamp `row`/`lww` ops compare the whole record (identical → no-op, different → `stale_op`); `ctr` gained `actor` (op, `CtrEntry`, `cn` key), so a `seq` not newer is ignored. The client (step 4 on) creates the actor once per installation and keeps it with the HLC. `crates/zen-server/src/crdt.rs` (`RowEngine`, `/v1/crdt/get`, `/v1/crdt/range`, `sweep`); `tree::check_clock` is shared; the idempotency record carries `add_count`; `CrdtOp::target()` became `fs()` and `tree()`; feature `"crdt_rows"` always on. Tests `crates/zen-server/tests/crdt.rs` (9, embedded); FoundationDB runs in CI only (not installed in the session container), and the cross-node test runs only there. WASM is 2.06 MB after the new wire types (STATS updated in step 11). Steps 1–3 went into one PR, the first under this rhythm.
* **Step 4** (client prerequisites): done in PR #13, with the fdb fix of PR #12's sweeper test (it waits for commit versions, which trail the wall clock on an idle FoundationDB cluster). As designed below, with these details for the next steps:
  * `clock.ts`: `TreeClock` (alias `SessionClock`), `session.clock`, `session.useClock(clock)`, `clock.actor`, `observeUnchecked` (event timestamps more than 60 s ahead are ignored).
  * `Transaction`: `readVersion`, `snapshotGet`, `snapshotRange`/`snapshotRangeStored`, `expectKey(path | storedKey, version)`, `addCrdtOps`, `addChunks`, `onCommit`, `onError`.
  * `Tree`: `prepare`/`accept`/`rebase`, `writeIn(tx, ops, chunks)`, `TreeBatch.commitIn(tx)`, `upload` + `writeOp`.
  * Tests: 72 vitest tests (client and fuse).
* **Step 4** was merged with PR #13.
* **Step 5** (`@zen/db` database): planned (see Step 5: sub-steps 5a, 5b and 5c, a PR each).
  * **5a** (foundation): done, PR #14. `packages/db`: `cbor.ts`, `sortkey.ts`, `row.ts`, `keys.ts`, `catalog.ts`, `db.ts` (`Db.open`, `Table`, catalog cache), `txn.ts` (`DbTransaction`, `TableTx`, `changeIndex`), `index/{unique,fast,types}.ts`, `query.ts` (pk, unique, fast, scan), `migrate.ts` (`Migrator`, `migrate`). For the next sub-steps:
    * `changeIndex` (txn.ts) dispatches on `kind`; 5b adds `private` and `sealed` there, to `SUPPORTED` in migrate.ts, and a plan step to `query.ts`. `IndexChange` carries only the CBOR elements: 5b adds the raw values for sort keys.
    * A migration op is paged through `Migrator.run`: progress `{op, key}` in the MigrationRecord; each page re-reads it, so duplicate runners conflict.
    * The catalog cache: a TableRecord is read once per transaction, by its cached version with `expectKey`, and refreshed after a `conflict`. A read-only transaction commits nothing, so `DbTransaction.finish` checks its cached records with one `snapshotGet` and re-runs on a stale one (an extra round trip per read-only transaction; 5c can batch it).
    * Client changes: `Transaction.get/set/delete/range/clearPrefix` take stored keys; new `getAll`, `rangeStored(begin, end, {limit})` (records only the part read), `clearRange`; `transaction()` also re-runs on a retryable `ZenError` thrown inside `fn` (a read's `too_old`, or a check of the caller's).
    * Spec: zendb.md §4.3 clarified (the last part of a padded row is padded to a multiple of 16 KiB; a padded Row that its bucket would push past one value goes to parts).
    * Tests: `packages/db/test/vectors.test.ts` (11; the CBOR duplicate-key refusal can't be expressed in JavaScript and is skipped), `db.test.ts` (22). 109 vitest tests in all. CI typechecks `packages/db/tsconfig.test.json`; it maps `@zen/client` to the client's sources.
  * **PR #14 CI fix:** a test checked that writers maintain a building unique index by writing a value an old row held, which the backfill may not have reached yet (stored-key order is random). It now uses values of its own.
  * **PR #14 merged** before the 5b plan commit, so the branch restarted from main, and the plan commit and 5b are a new PR.
  * **5b** (private and sealed indexes, ordered queries): done, PR #15. As planned, with these details for the next steps:
    * **⚠️ Spec change (§5.1):** null's sort-key tag is now `0x01`, not `0x00`. A text field's terminator (`0x00`) followed by a descending null (`0xFF`) read like an escaped zero, so keys didn't decode and `"a"` sorted after `"a\0"`. No component encoding now starts with `0x00` or `0xFF`. zen-core's `sortkey` changed with a test of the failing cases, and the vectors gained the null-after-text cases.
    * **Other spec additions:** §5.4.6, every node of a decoy index is salted, and decoys need `max_value_bytes` ≥ 65,584 (`createIndex` refuses them otherwise, `bad_value`). §5.7, extra parts are deleted, and an empty sealed index keeps its head and one part.
    * **Code:**
      * `index/node.ts`: Node and RootRecord codecs, decoy padding.
      * `index/prolly.ts`: `rewrite(treeKeys, store, root, changes, {decoys})` is the incremental canonical rewrite. A rewritten node with unchanged entries keeps its old id, and the root is trimmed when the top has one entry. It also has `scan(store, root, lo, hi, reverse)`.
      * `index/private.ts`:
        * `TxNodeStore`: `snapshotGet` plus the `Db` node cache; a missing node → `conflict`.
        * `privateChange` queues changes in `DbTransaction.indexOps` per (index, shard); `applyPrivate` runs in `finish`.
        * `privateScan` merges shards and the transaction's own pending changes.
      * `index/sealed.ts`: `SealedState` is loaded once per transaction and changed in place, written in `finish`, cached per `Db` by digest.
      * `DbTransaction.indexRoot` reads each root once per transaction (its snapshot of the tree).
      * `query.ts`: plan step 4 (`planRange`). An index that gives the `orderBy` is preferred over fast equality. Cursors carry `{path, key}` for index paths.
      * `sortkey.ts`: `decodeSortKey`, `encodeComponent`, `encodePrefix`.
    * **Tests:**
      * `vectors.test.ts` (15): prolly, sealed, sort-key decoding.
      * `prolly.test.ts` (9, in memory against `IndexKeys.build`): random batches at fanout 2/4/64, the entry and byte caps, scans, decoys.
      * `indexes.test.ts` (18, server).
      * 140 vitest tests in all.
  * **5c** (transaction modes, cache coherence, files, bulk import, owner): done, PR #16 (merged; CI green). As planned, with these details for step 6:
    * **Client:**
      * `TxnOptions.retry` chooses which errors re-run `fn`.
      * `Transaction.getEntries` (with versions; none for own writes) and `owns(key)` are new.
      * `commitTransaction(tx)` is exported.
      * `transaction()` now runs the `onError` hooks on the last attempt too, so caches drop stale entries.
    * **`Db.transaction`:**
      * `auto` runs short with `retry` = conflict only. `DbTransaction.live()` (called at every read, query page and `finish`) throws `SwitchToLong` past the deadline. `SwitchToLong` or `too_old` → one long run with the remaining attempts.
      * In long mode, a `ZenError too_large` thrown inside `fn` becomes `DbError('too_large', '… split the work')`.
    * **`db.begin()`** → `ManualTransaction`. It overrides `live()` to throw `closed`, and runs the raw error hooks on a failed commit.
    * **Row cache:**
      * `Db.rows` is an LRU (`rowCache`, default 1,024). Copies are cloned in and out. `db.stats.rowHits/rowMisses`.
      * `TableTx.read`: a hit is `useCached` → `expectKey` + `DbTransaction.cached`. A miss is `getEntries` + `noteRow`. Query paths fill the cache too.
      * `onCommit` drops written keys (all rows if the commit clears a range). `conflict` drops every cached record used. A read-only `finish` checks catalog records and rows in one `snapshotGet`.
    * **Files:** `files.ts` `TxFiles` (`tx.fs(tree).write(upload, {replaces})`, `.ops(batch | PendingOp[])`) over `Tree.writeIn`. Duplicate node writes are refused via `DbTransaction.fileWrites`.
    * **`db.importRows`**, as planned.
    * **Owner:**
      * `tabs.ts` `Owner` with `LockManagerLike`. `DbOptions.locks` defaults to `navigator.locks`; `null` means none. `owner: false` never campaigns.
      * `db.isOwner`, `db.onOwner(fn)` (fires at once if already owner), `db.ownerChange`.
      * The lock name is `zen/db/<fs id>/<ns>` (the fs id is a number).
      * **Step 6 hangs consumers, the scheduler and the stream off `onOwner`.**
    * **Docs:** new `docs/DB.md` (database part; step 6 adds the broker), `docs/CLIENT.md` §5, TECH_DEBT: TD-DB-PERSISTENT-CACHE, TD-DB-TAB-PROXY, TD-DB-OBLIVIOUS-INDEX, TD-DB-SHAREDWORKER.
    * **Tests:**
      * `packages/db/test/txn.test.ts` (18), with fake Web Locks.
      * `kv.test.ts` +3.
      * `expectCanonical` moved to `packages/db/test/helpers.ts`.
      * 161 vitest tests in all.
* **Step 6** (broker): planned (see Step 6: sub-steps 6a, 6b and 6c, a PR each). PR #16 is merged, so the plan commit restarts the branch from main and opens a new PR, which 6a joins (PR #17).
  * **6a** (messages, emit, `on`, consume, change events, ephemeral): done, PR #17 (merged; CI green). As planned, with these details for 6b and 6c:
    * **Files:**
      * `msg.ts`: Msg and BodyRef codecs, `eventId`, `Message`.
      * `broker.ts`:
        * `appendMsg(tx, topic, msg, key, lean?)` writes large bodies to parts at `keys.msgPart`. With `lean`, the change events' row is dropped instead.
        * `openMessage`.
        * `StreamSource` / `DirectSource`. **6c puts the proxy behind this interface.**
        * `OnSubscription`.
        * `ConsumerHandle`:
          * owner-gated via `db.onOwner`
          * group id from `TopicKeys.groupId`
          * leased modes loop on `deliveries()`, `partitioned` with one loop per partition
          * `per_key` uses `next({limit: maxInflight})`
          * `not_leader` → forget the lease, wait ttl/3, campaign again
          * `cursor_moved` / `claim_lost` → drop the delivery; anything else → `nack`
          * a `group` getter (DLQ)
      * `system.ts`: the system tables. `init()` creates missing ones in the DbRecord transaction (one `getAll`). `createTable`, `renameTable` and `dropTable` refuse `$` names.
    * **API:**
      * `db.topic(...path)`, cached and closed by `close()`.
      * `db.emit` (its own transaction) and `tx.emit`, which return the message id. `zen.` types are refused for apps.
      * `tx.after(fn)`.
      * `tx.causation` (set by consumers; every emit carries it).
      * `db.on(target, handler, {after, cursor, batch, txn, onError})`. The `$cursors` row is `{device, name, topic, offset}`; a durable handler that throws stops the subscription (`active` false).
      * `db.consume(opts, (msg, tx))`, and `db.consumeManual(opts, (msg))`, which acks with `msg.ack()` or `db.begin({consumes: msg})`.
      * `db.publishEphemeral` / `db.onEphemeral`.
      * `table.on(handler)` (change events).
      * `db.device`. `db.tableNames()` leaves out the system tables.
      * New error codes `poisoned` and `timeout`.
    * **Change events:** `DbTransaction.noteChange` is recorded in `TableTx.write` and emitted in `finish()`: one per row, final state. A row inserted then deleted emits nothing.
    * **Client fix:** `Transaction.snapshotGet` and `getEntries` returned sparse arrays, so `some((e) => !e)` missed absent entries (missing row parts too). They are dense now, with a test in `kv.test.ts`.
    * **Tests:**
      * `packages/db/test/broker.test.ts` (15), with its own copy of `FakeLocks`.
      * `vectors.test.ts` +3 (Msg, BodyRef, event and group ids).
      * 180 vitest tests in all.
  * **6b** (scheduler, work queues, request/reply): done, PR #18. As planned, with these details for 6c:
    * **Files:**
      * `broker.ts`: `appendMsg` is `sealMsg` (returns `{payload, parts}`) plus the append. New: `checkHeaders` (`zen.` header names refused), `emitKey` (`spread`), `msgOf` (a received message as a Msg to send again), `pathBytes`, `attemptOf`.
      * `ConsumerHandle` takes an internal `FailHook`. On a handler throw, `failed()` runs a fresh transaction with the consume step: the hook (serve: `ReplyError` → error reply), else with `retry: 'delay'` `retryLater` (schedule the copy, or emit it to `topic.child('zen.dlq')` after `maxAttempts`). Otherwise `nack`. **6c's saga participants and orchestrator can reuse the hook.**
      * `sched.ts`: `scheduleMsg(tx, topic, msg, key, at)`, `scheduleGc(tx, paths, at)`, `dueAt`, and `Scheduler` (`start`, `stop({release})`, `wake`, `runOnce`, `leading`). `db.scheduler` exists unless `DbOptions.scheduler === false`, and starts on `onOwner`. The leader is the group `groupId('sched')` on `('zen', 'db', ns, 'sched')`. A `gc` path is cleared with everything under it (`keys.range`).
      * `reply.ts`: `ReplyError`, `Inbox` (`db.inbox`: `on` from 12 zero bytes plus `onEphemeral`, waiters by corr), `sendReply`/`replyMsg`, and dedup (`dedupSeen`, `dedupRecord`: an `$inbox` row plus a `gc` job for its row and parts keys).
    * **API:**
      * `tx/db.schedule`, `tx/db.cancel`, `tx.gc` (internal).
      * `emit({spread, ttlMs})`. A ttl's `gc` row is inserted at `finish`.
      * `tx.request`, `tx.requestAll`, `db.request`, `db.requestAll`, `tx.reply`, `db.reply(msg, body, {ephemeral})`.
      * `db.serve(opts & {reply: 'event' | 'ephemeral'})`.
      * `ConsumeOptions.retry`/`backoff`/`dedup`/`dedupTtlMs`. `EphemeralMsg` gained `corr`. `Reply = EphemeralMsg | Message`.
    * **Fix (5c row cache):** a read in flight could cache a row after a commit had dropped it, and `insert` trusted that copy and threw `exists` before the commit could catch it as a conflict. `insert` now reads the row from the server (`read(t, pk, fresh)`). Regression test in `txn.test.ts`.
    * **Test fixes:** `txn.test.ts`'s `itemsDb` sets `scheduler: false`, since its tests count reads. The decoy test in `indexes.test.ts` asserted more than 3 nodes, which a random index key misses about 0.3% of the time; it now asserts at least 3.
    * **⚠️ Spec:** §11.5, §12.2 and §12.3, as planned (a separate `spec:` commit).
    * **Tests:** `packages/db/test/patterns.test.ts` (17), 198 vitest tests in all. PR #18 merged, CI green (commit `a91c258`).
  * **6c** (sagas, tab proxy, fault suite, docs): planned (see 6c below); a new PR, since #18 is merged.

## Facts the work must respect (from the code survey)

**Wasm exposure.** JS can't compute any zendb.md derivation today:
* NK, `kdf`, `prf16` and `H(label, ·)` are not exported. NK is private in `zen-core/src/keys.rs:24`; `kdf` and `prf16` are in `kdf.rs:11,19`.
* `NameChain` is not exported. `kvKey` always starts at the KV root.
* Sealing is per kind; the generic `seal` (`seal.rs:104`) is crate-private.
* There is no CRDT dependency anywhere.

**CBOR.** No JS library handles arbitrary values. The test fixture `testing/cbor.ts` doesn't sort map keys and can't decode. zen-proto uses ciborium, and its determinism for arbitrary values isn't established.

**Client transactions.**
* `Transaction.get` and `range` always record a conflict or expectation. `readVersion` is private.
* `build()` lets `extra` set only `consume`, `chunks` and `crdt_ops`.
* `Tree.send` builds its own commit and owns the HLC tick, accept and rebase logic.

**Clocks.** The event HLC is a per-fs `zw.Clock` (`log.ts:62`), separate from the `TreeClock`.

**Server.**
* `crdt_ops` are validated in `commit.rs::validate` (149–184) and applied through `tree::Engine` (`apply` 670–741, `flush` 744–786) inside `txn_loop!`. The server retries only when there is no `read_version` (`txn.rs:23`).
* `CrdtOp::target()` returns `(fs, tree)`. `CommitResult.dots` comes from the fs write counter.
* `check_clock` (`tree.rs:473`) can be reused.
* The sweeper is `lib.rs:325–356`, then `tree::sweep`, in pages of 1000 with up to 100 rounds.
* `/v1/info` features are hard-coded at `lib.rs:67`. The router is `lib.rs:167–298`.
* **Key prefix clash:** `"cr"` is taken by `keys::chunk_refs` (`keys.rs:396`). zendb.md §19.5's row-register key needs another prefix (`"co"`).

**Tests and CI.**
* `fs.rs` has a xorshift property test against an in-memory reference (353–412). Tests run on both backends with `ZEN_TEST_BACKEND=fdb`.
* Vitest: each test file spawns its own server (`packages/client/test/helpers.ts:20`).
* The CI `client` job is `.github/workflows/ci.yml:89–131`.

## Step 0: persist the plan
* Restart the branch from `main` (above).
* Write `docs/MILESTONE-5.md` (this plan) and link it from DESIGN-3 §6 item 5.
* README status: milestone 5 "in progress".
* Commit and push.

## Step 1: spec fixes found while planning (⚠️ SPEC CHANGES, `spec:` commits)
1. **CRDT rows on the wire:**
   * the key prefixes of zendb.md §19.5 move into `keyspace.md` as §3.8, using `"co"` instead of `"cr"`
   * `api.md` §6 gets the `row`/`lww`/`ctr`/`add`/`rem` CrdtOp variants
   * a new `api.md` §13 holds `/v1/crdt/get` and `/v1/crdt/range` and the `"crdt_rows"` feature
   * `CommitResult` gains `set_dots: [bytes(12)]` for `add` ops. They are numbered among the commit's `add`s, separate from fs `dots`; zendb.md §19.3 is amended.
2. **Deterministic CBOR for zen-db values:** the exact rules (RFC 8949 §4.2.1), float encoding (always 64-bit), and the integer range, in zendb.md §1. The JS and Rust encoders must match the vectors.
3. **`formats.md` §4:** kind 7 is no longer "reserved" once implemented.
4. **Vectors:** `spec/test-vectors/zendb.json`, per zendb.md §17 and §19.9.

## Step 2: zen-core and zen-wasm (Rust)

**zen-core, new module `db`:**
* `DbKeys::new(fs_keys, ns)`, which derives K_db inside Rust, so NK never reaches JS. It provides:
  * `boundary(index_id)` and `node_id(index_id, node_bytes)`
  * `shard(index_id, pk, K)`
  * `crdt(table_id)` → field and element tokens
  * `parts_digest`
* `NameChain` from an arbitrary root, for KV paths under the database. The existing `kv_key` covers `("zen", "db", ns, …)`; check.
* Sort-key encoding (§5.1).
* The prolly canonical chunker: boundary test, levels, Node encoding.
* A deterministic CBOR `Value` encoder and decoder (a small, own implementation, or ciborium plus key sorting).
* `seal_crdt_value` and `open_crdt_value` (kind 7).
* Group id derivation (`zen/v1/broker-group`) on TopicKeys.
* Vectors added to `gen_vectors`.

**zen-wasm:**
* bindings for the above: `FsKeys.db(ns)` → `DbKeys` handle; `TopicKeys.groupId(name)`; `encodeValue`/`decodeValue` (JS value ↔ deterministic CBOR); `sortKey(fields, values, pk)`; `prollyBoundary`/`nodeId`; kind-7 seal and open
* new wire types in `wire.rs`

Every new export goes in a lean module. Re-measure the wasm size and update `docs/STATS.md`.

**As built:** the encodings that need no key (deterministic CBOR, sort keys, rows) are implemented in TypeScript in `@zen/db`, and the Rust ones are their reference: both must reproduce `spec/test-vectors/zendb.json`. Only what needs a key goes through wasm: `FsKeys.db(ns)` → `DbKeys` (`prefix`, `key`, `index` → `IndexKeys` with `isBoundary`, `boundaries`, `shard`, `nodeId`, `build`; `crdt` → `CrdtKeys`), `FsKeys.sealCrdtValue`/`openCrdtValue`, `TopicKeys.groupId`, `partsDigest`. The canonical tree builder is exported for tests of the incremental code. Small spec clarifications came with it: sort keys are capped at 4,096 bytes, the row pad rule at a CBOR head boundary, trailing-zero padding of parts (rows, sealed indexes), and `height = root level + 1`.


## Step 3: server side of CRDT tables (zendb.md §19, TD-CRDT-ROWS-SERVER)
* **zen-proto:**
  * the `CrdtOp` variants `Row`, `Lww`, `Ctr`, `Add`, `Rem`, with `target()` generalized to an enum (tree or object)
  * `CommitResult.set_dots`
  * `CrdtGet`, `CrdtRange` and `ObjState`, with the `ts` derives
* **zen-server:**
  * **`crdt.rs`:** an engine that applies row, lww, ctr, add and rem inside the commit transaction, reusing `check_clock` and `Ts` ordering, with the merge rules of §19.3
  * **`validate`:** permission, `max_value_bytes` and byte estimates per variant
  * **quotas:** quota deltas as KV writes
  * **keyspace:** the `co`/`cw`/`cn`/`cs`/`cv`/`cd` keys
  * **sweeper:** purges dead objects past the horizon, in batches
  * **routes:** `/v1/crdt/get` and `/v1/crdt/range`, with the `fs` read right and paging
  * **`/v1/info`:** advertises `"crdt_rows"`
* **Tests, `tests/crdt.rs`:**
  * the merge rules, skew and horizon, replays (`ctr` seq, idempotent `commit_id`), add-wins, delete vs. update
  * permissions, the sweeper purge
  * **property test:** three devices, random ops, shuffled arrival, compared against an in-memory reference, three seeds (same pattern as `fs.rs`)
  * a cross-node test on FDB
  * every test on both backends

## Step 4: `@zen/client` prerequisites
* **`Transaction`:**
  * `snapshotGet(paths)` and `snapshotRange`: reads at the transaction's read version, with no conflict or expect (for prolly nodes)
  * a public `readVersion`
  * `expectKey(path, version)`, to put cached records into the read set (G9)
* **`Tree`:** split `send` into `prepare(ops)` → `crdt_ops` + chunks, and `accept(result)`, so `tx.fs(tree)` can add ops to a transaction's commit and the rebase runs inside the transaction's retry. Add `Tree.writeIn(tx, …)` for an already-uploaded file.
* **Clocks:** one HLC per session, shared by events, trees and CRDT rows (`TreeClock`).
* `CommitResult.set_dots` passed through.

**Design (from reading `kv.ts`, `tree.ts`, `log.ts` and `commit.rs`):**
1. **`Transaction`** (`packages/client/src/kv.ts`):
   * `readVersion` becomes a public getter, still set by the first read.
   * `snapshotGet(paths)` → `(KvEntry | undefined)[]` and `snapshotRange(prefix | [begin, end), opts)` → an async generator. They read at `readVersion` in short mode (and set it if unset). They record neither a conflict nor an expect, and they overlay the transaction's own writes and clears.
     * To make them share code, `Kv.getEntries` and `rangeStored` also return the response's `read_version`, through an internal variant.
   * `expectKey(path | storedKey, version | undefined)` adds an `expect`. `undefined` means "absent". The server checks `expect` in both modes (`commit.rs:268`), so a cached record joins the read set even in short mode (G9).
   * Hooks for parts that add to the commit:
     * `tx.addCrdtOps(ops)` and `tx.addChunks(chunks)` append to the commit body (today `extra` is overwritten). The existing `extra` stays.
     * `tx.onCommit(fn(result))` runs after a successful commit.
     * `tx.onError(fn(e, attempt) → Promise<boolean>)` can ask for a re-run.
   * `transaction()` re-runs `fn` on a retryable error, or when an `onError` hook returns true. Either way it stays within `attempts`.
2. **`Tree`** (`tree.ts`): the body of `send` splits into three parts, and `send` is rebuilt on them with the same behaviour:
   * `async prepare(ops)` → `Prepared {ops: CrdtOp[], hlcs}`: it syncs the clock once and ticks the `move`/`meta` ops.
   * `accept(p)`: `clock.accept` plus `record` (onOp).
   * `async rebase(p, e, attempt) → boolean`: the existing `stale_op`/`clock_skew` recovery.
   * `async writeIn(tx, ops, chunks?)`:
     * It prepares the ops and adds them (and the chunks) to `tx`.
     * It registers `accept` as an `onCommit` hook and `rebase` as an `onError` hook, so the rebase happens on `transaction`'s re-run, with fresh ticks.
     * `TreeBatch.commitIn(tx)` uses it.
   * `upload(node, data, opts)` → `Upload {ids, manifest, sealedManifest, index, uploaded, commits}`: the chunk half of `writeFile`, with every chunk flushed in its own commits. Unreferenced chunks fall to the existing chunk-grace sweep.
     * `writeOp(node, upload, replaces)` turns an upload into a `write` PendingOp, so an uploaded file goes into a transaction with `writeIn(tx, [op])`.
     * `writeFile` keeps its single-commit fast path. Its last chunks still ride with the `write` op.
3. **One clock per session**: a new `clock.ts` holds `ClockStore`, `memoryClockStore` and `TreeClock` (the name is kept and re-exported, and `SessionClock` is added as an alias).
   * `sessionClock(session)` replaces the `WeakMap` in `tree.ts`. `Session.useClock(clock)`, called before first use, sets a persisted one.
   * `log.ts` drops its per-fs `zw.Clock`: `Topic.seal` ticks the session clock and `open` observes into it. Event HLCs are then ordered with tree ops and CRDT rows.
4. **Counter actor**: `ClockStore` gains optional `loadActor()`/`saveActor(a)`. `TreeClock.actor` (16 bytes) is loaded, or else created randomly once and saved.
   * With the memory store, every process is its own installation, which is correct because its HLC also restarts.
   * Documented: a store must persist the actor together with the HLC, and copying a store to another installation is a bug.
   * (Step 7 can use `clock.tick()` as the `ctr` seq, since it is monotonic per installation.)
5. **`set_dots`**: generated from zen-proto. `tx.result.set_dots` and `send`'s result carry it after `scripts/build-wasm.sh`, and a test checks it.
6. **Docs**: `docs/CLIENT.md` gets snapshot reads, `expectKey`, `writeIn`/`upload`, and the session clock and actor.

**Tests** (`packages/client/test`):
* `kv.test.ts`:
  * a snapshot read followed by a concurrent write commits, where `get` would conflict
  * `readVersion` is exposed
  * a stale `expectKey` conflicts and re-runs; a current one commits
  * an absent key `expectKey(…, undefined)` conflicts once the key is written
  * `addCrdtOps` with a raw `add` op returns `set_dots`
* `tree.test.ts`:
  * `writeIn` together with a KV write commits atomically, and a conflict re-runs both
  * a `stale_op` inside a transaction is rebased (the existing skewed-clock setup)
  * `upload` + `writeOp` + `writeIn`
  * the existing `send` tests unchanged
* `log.test.ts`: an event's HLC lies between two tree ops' HLCs on the same session; the actor persists through a store and is fresh with the memory store.

**Verification:** `scripts/build-wasm.sh`, then `npx biome check`, `npx tsc --noEmit` (client, fuse) and `npx vitest run` (client, fuse).

## Step 5: `@zen/db` part A, the database (zendb.md §1–9, §17)

Three sub-steps, **one PR each**, with the same rhythm as the steps: log, commit, push, PR, then stop for compaction.

**What exists.**
* `packages/db` doesn't exist yet.
* wasm (`crates/zen-wasm/src/db.rs`) gives only what needs a key:
  * `FsKeys.db(ns)` → `DbKeys {prefix, key(elements), index(id), crdt(id)}`
  * `IndexKeys {isBoundary, boundaries(level, keys, fanout), shard, nodeId, build}`
  * `partsDigest`, `TopicKeys.groupId`
* Everything keyless gets written in TS and must match `spec/test-vectors/zendb.json`:
  * deterministic CBOR (`cbor.ok`/`refused_*`)
  * `sort_keys`
  * `rows` (plain, parts, padded, buckets)
  * `prolly`
  * `sealed_index`
  * `database`
* The client `Transaction` (`packages/client/src/kv.ts`) takes stored keys (`Uint8Array`) in `snapshotGet`/`expectKey` but not in `get`/`set`/`delete`/`range`/`clearPrefix`. Its `transaction()` has `short` and `long` but no `auto`, and nothing reports `too_old`→long.

**Common decisions.**
* **Stored keys.** zen-db computes its stored keys with `DbKeys.key([...elements])`. Elements are UTF-8 text, CBOR bytes, 16-byte ids and `u32`/`u64` BE. A small client change widens `Transaction.get`/`set`/`delete`/`clearPrefix`/`range` to `Path | Uint8Array`, the same way `key()` already does (no behaviour change for paths). Sealing stays kind-1, through `fs.sealKeys().sealValue(storedKey, …)`.
* **Errors.** `DbError extends Error {code}`. The codes are `exists`, `not_found`, `unique_violation`, `bad_type`, `schema_newer`, `needs_index`, `too_large`, `corrupt`, `format` and `building` (a duplicate found by a unique backfill). Server errors pass through as `ZenError`.
* **Persistent cache (§9.3, "milestone 5 decides").** Memory only. Recorded in DB.md; the encrypted persistent cache becomes TD-DB-PERSISTENT-CACHE.
* **Oblivious (§5.8).** `kind: "oblivious"` is refused (`format`) at createIndex, per the milestone decision (TD-DB-OBLIVIOUS-INDEX).
* **Change topics (`ChangeDef`).** Stored in the TableRecord now; the appends are emitted by step 6.
* **HLCs.** `created_hlc` and the MigrationRecord `hlc` come from `fs.session.clock.tick()`.
* **Package.** `packages/db`, modelled on `packages/client`'s `package.json`, with a dependency on `@zen/client`, a vitest alias in `vitest.config.ts`, and the TS project wiring the client package uses. Tests go in `packages/db/test`, reusing `packages/client/test/helpers.ts` (`world`, `signInDevice`) through a relative import.

### 5a: foundation, rows, unique and fast indexes, catalog and migrations
**Files** (`packages/db/src/`):
* `cbor.ts`: deterministic encode/decode (§1): shortest heads, integers as numbers in ±2^53 and `bigint` beyond, 64-bit floats, sorted map keys, refusals.
* `sortkey.ts`: §5.1, by declared type, `desc`, escaping, the pk suffix, the 4,096-byte cap.
* `keys.ts`: the `D ‖ …` element builders on `DbKeys`, and their prefix ranges (`prefixEnd`).
* `row.ts`: Row encode/decode (§4.1), parts and digest (§4.2), padding buckets (§4.3), zero-tail checks.
* `catalog.ts`: DbRecord, TableRecord, IndexDef and MigrationRecord codecs (integer-keyed CBOR), plus the in-memory catalog cache with versions.
* `db.ts`: the `Db` class.
  * `zen.db(fs, ns, {schema, migrations})` and `Db.open` (§3.1): unknown format or non-basic integrity → `format`; a newer schema → `schema_newer`; create at 0, then migrate.
  * `db.table<T>(name)` returns a `Table` with `get`/`insert`/`put`/`update`/`delete`.
* `txn.ts`: `DbTransaction`, wrapping a client `Transaction`.
  * `tx.table(t)` CRUD (§4.4), reading the TableRecord into the read set (§7.4) and the old row first.
  * Index maintenance (§5.5) for unique (§5.2: short = conflict read, long = expect-absent) and fast (§5.3).
  * A limits budget (§7.5) checked against `/v1/info` `max_commit_ops`/`max_commit_bytes`/`max_value_bytes` before sending → `too_large`.
  * `db.transaction(fn, {mode: 'short'|'long', attempts})` over client `transaction()`.
* `index/unique.ts`, `index/fast.ts`.
* `query.ts` (first part): plan steps 1–3 and 5 of §6.1 (pk, unique and fast equality, then a scan with a warning and a client-side filter), the other predicates applied client-side, and a cursor for scans (§6.2).
* `migrate.ts` (§8): the runner, with MigrationRecord logging and resume from `progress`.
  * Ops: `createTable`, `dropTable`, `renameTable`, `createIndex`, `dropIndex`, `transform`.
  * Paged backfill with `built_to`, for unique and fast in 5a. The kind-specific "add entries for a page" is a hook that 5b fills in for private and sealed.
  * A unique duplicate → stays `building` with the duplicates reported.
  * Paged drops through `clear_ranges` within `max_range_items`.

**Tests 5a:**
* The TS encoders reproduce the vectors: cbor, sort_keys, rows and database keys.
* CRUD with `exists` and `not_found`; large rows with parts (a lowered `max_value_bytes` in `world({limits})`); padding sizes.
* Unique: a violation; two clients inserting the same value concurrently, where one wins and the other gets `unique_violation` after a retry, in both short and long mode.
* Fast-index equality.
* Long mode.
* Migrations:
  * create, rename and drop
  * `transform`
  * a resume after a simulated kill (throw mid-step, reopen)
  * a unique backfill over existing duplicates
  * a writer during a backfill keeps the index complete
* A schema change concurrent with a write → the write retries.
* `schema_newer`.
* A commit-limit `too_large`.

### 5b: private and sealed indexes, ordered queries (detailed plan)

**Context.** 5a (PR #14) has unique and fast indexes and equality queries. 5b adds the two indexes that hide values and support ranges and ordering: `private` (a prolly tree, §5.4) and `sealed` (one blob, §5.7). With them come range and ordered queries with paging (§6.1 step 4). The Rust reference is `crates/zen-core/src/db/prolly.rs`; wasm gives `IndexKeys` (`isBoundary`, `boundaries`, `shard`, `nodeId`, and `build`, the reference tree builder).

**After compaction, re-read:** this section; `spec/zendb.md` §5.1, §5.4–5.7, §5.9, §6, §7.4, §8.2; `crates/zen-core/src/db/prolly.rs`; the `prolly` and `sealed_index` sections of `crates/zen-core/src/db_vectors.rs`; `packages/db/src/{txn,query,migrate,keys,db}.ts` and `index/*.ts`; `packages/db/test/db.test.ts` (setup and helpers).

**Branch/PR.** If PR #14 is merged, restart the branch from main and open a new PR. Otherwise 5b joins #14 (update its title and body).

#### Files
1. **`sortkey.ts`: `decodeSortKey(fields, bytes) → {values, pk}`.** It parses each field component by its declared type (un-inverting `desc`), then the pk components by tag (`0x10/0x11`, `0x20/0x21`, `0x28`, `0x30`, `0x40`; unescaping `00 ff`). The pk is one value or an array, as `pkOf`. Range bounds also need `encodeComponent(value, type, desc)` and a "text/bytes prefix" encoding: the tag plus escaped bytes, without the `0x00` terminator. Test: a round trip on every vector case.
2. **`index/node.ts`: Node and RootRecord codecs** (`{1: level, 2: entries, 3?: salt, 4?: pad}`, `{1: root, 2: height, 3: count}`). Entry encoded length and `nodeLen`, as in prolly.rs (`MAX_NODE_ENTRIES` 1024, `MAX_NODE_BYTES` 60,000, both on the unsalted, unpadded encoding). Decoy padding buckets are 1, 2 and 4 KiB, then multiples of 4 KiB.
3. **`index/prolly.ts`: the tree, written against a store interface** so it runs in memory for tests and on a transaction in the database.
   * `NodeStore { get(id): Promise<Node> (throws "missing"), put(id, bytes), del(id) }`. `TreeKeys` wraps `IndexKeys`, with `boundaries` batched per level and `nodeId`.
   * **`chunk(level, entries)`:** exactly prolly.rs `chunk` (hash boundary, entry cap, byte cap, last entry).
   * **`apply(root | undefined, adds: sortKey[], removes: sortKey[]) → {root | undefined, put: Map<id, bytes>, del: Set<id>}`, the incremental canonical rewrite (§5.4.4).** It is idempotent: adding a present key or removing an absent one is a no-op.
     * Level 0: the changes, sorted. Each change belongs to the first node whose last key is ≥ the change key, or to the last node. A cursor (a path stack from the root) finds that node and walks right to sibling nodes, crossing parents.
     * Merge the node's entries with the changes, then re-chunk from the start of the first affected node, pulling in the following old nodes until two things hold: a new node closes on the same entry as an old node, and no change is left. Chunking restarts at every node end, so everything to the right is unchanged.
     * Replaced old nodes go to `del`, new nodes to `put`, with an id from `nodeId(encode(node))`.
     * The level above gets the matching changes: remove `[first_key, old id, count]`, add `[first_key, new id, count]`. Repeat level by level.
     * The root is the first level with exactly one node. The height grows when the old root level splits, and shrinks when an upper level is left with one entry: nodes above the new root are deleted. An empty tree has no root.
     * A node content that is unchanged keeps its id, so it is neither put nor deleted.
   * **Reads:** `cursor(root, from, {reverse})` → an async iterator of sort keys in order, from a bound. It loads the path, then leaves to the right (or left). `count` fields give a random entry for decoys.
   * **Decoys (§5.4.6):** with `decoys: k`, every node written gets a fresh random 16-byte `salt` and a `pad` to its size bucket. After the real changes, `k` times: pick a random level-0 position by descending with `count`, then rewrite that leaf with a new salt, and its ancestors up to the root, through the same path rewrite. Node ids are then not history-independent; the vectors use unsalted nodes.
4. **`index/private.ts`: the private index inside a transaction.**
   * `IndexChange` gains `oldValues`/`newValues` (raw indexed values) and `pkParts`. Sort keys come from `sortKey`, and nulls are included (§5.4.1).
   * **Changes are collected, not applied at once:** `DbTransaction` keeps `pending: Map<indexId, Map<shard, {adds, removes}>>`. The same key added then removed cancels out.
   * `DbTransaction.finish()` first applies the pending changes of every touched shard, then runs `checkLimits` (§5.5: one rewrite per touched path per commit). For each shard:
     * read the RootRecord with `raw.get` (into the read set: a conflict in short mode, an `expect` in long mode)
     * read nodes through a `NodeStore` on `raw.snapshotGet` and the node cache
     * `raw.set` the new nodes, `raw.delete` the old ones, and `set` or `delete` the root
   * **Shard** = `IndexKeys.shard(pkElement, shards)`.
   * **Node cache:** per `Db`, by id, immutable, bounded (LRU of about 4,096 nodes). `IndexKeys` is cached per index in `Db` and freed by `close()`.
   * **Missing node (§5.4.3):** throw `ZenError(409, 'conflict')`. Since 5a, `transaction()` re-runs on it, which is the restart. Outside a transaction too, because every query runs in one.
   * **Reading your own writes:** a query on an index with pending changes in the same transaction merges its tree iterator with the pending adds (sorted) and skips the pending removes.
   * **unique + private:** unique entries (`uniqueChange`), plus the tree.
5. **`index/sealed.ts` (§5.7):**
   * **Blob:** `{1: [sortKey…]}`, cut into parts of `max_value_bytes − 1024`. The part count is rounded up to a power of two and zero-padded. Head: `{1: parts, 2: digest, 3: count}`.
   * **Read:** `raw.get(head)` puts the head in the read set. The blob is cached per `Db` by the head's digest, so an unchanged index costs that one `get`. Otherwise all parts are fetched with `snapshotGet` and checked with `joinParts`. A missing or torn part is a conflict in long mode, as for rows.
   * **Write (at `finish`, like private):** apply the pending changes to the sorted list. An encoded blob over `maxBytes` (default 1,048,576) gives `too_large`. Write every part and the head, and delete the parts beyond the new count. An empty index keeps a head with `count: 0` and one part (decision; noted in the spec).
   * **Unique:** checked against the list. A key whose field part (the prefix before the pk components) equals the new value's but whose pk differs gives `unique_violation`. Values with a null are not checked, as for unique entries. No `u` entries are written.
6. **`txn.ts` `changeIndex`:** dispatches `private` (unique entries if unique, plus pending) and `sealed` (pending). `checkTypes` already runs.
7. **`migrate.ts`:**
   * `SUPPORTED` gains `private` and `sealed`.
   * createIndex checks options: `fanout ≥ 2` (default 64), `shards ≥ 1` (default 1), `decoys ≥ 0`, and `maxBytes` for sealed only.
   * The backfill already calls `changeIndex` per row. The page transaction's `finish` applies the tree or blob. For sealed unique, a duplicate during the backfill is reported like a unique entry (`building`).
   * `dropIndex` already clears `n`, `r` and `s`.
8. **`query.ts`: plan step 4 (§6.1, §5.9).**
   * Among active `private`/`sealed` indexes, pick one whose leading fields are bound by `=` (non-null) and whose next field carries a range (`<`, `<=`, `>`, `>=`, `prefix`) or the `orderBy`. An index with every field bound by `=` also qualifies. Prefer the most bound fields, and private over sealed.
   * **Bounds:** P = the encoded `=` components. On the next field: `>= v` → lo = P‖e(v); `> v` → lo = prefixEnd(P‖e(v)); `< v` → hi = P‖e(v); `<= v` → hi = prefixEnd(P‖e(v)); `prefix` → [P‖e′(v), prefixEnd(P‖e′(v))). For a `desc` field, each bound is computed on inverted bytes, which swaps lo and hi. No range → [P, prefixEnd(P)).
   * The bound value is encoded by the declared type; a mismatch gives `bad_type`. Nulls sort first, so `< v` includes nulls on asc, as `compareValues` does.
   * **Order:** a scan is ascending in sort-key order. `orderBy` on the range field with direction ≠ that field's `desc` → reverse scan. Equality-bound fields don't affect order. An `orderBy` the index provides never needs `needs_index`.
   * **Rows:** decode each entry's pk → `raw.getAll` row keys in batches → re-check every predicate on the row (`keep`) → `limit`.
   * **Cursor:** `{path: 'p:<id>' or 's:<id>', key: sortKey}`, with `lo = keyAfter(key)` forward or `hi = key` reverse.
   * **Shards:** K iterators merged by sort key (min-heap; reversed for reverse scans), each root in the read set.
   * **Sealed:** the whole list, filtered by the bounds in memory.
   * **Isolation (§6.3):** the roots or head are in the read set, so any concurrent write to the index retries the transaction.
9. **Spec (⚠️ `spec:` commit, only if needed):** zendb.md §5.4.6 says "every node written by a decoy index carries a salt and pad"; §5.7 says an empty sealed index keeps a head with `count: 0`. Also a note that a node padded with decoys must fit one value: with `max_value_bytes` under about 62 KB, refuse `decoys` at createIndex (`bad_value`).

#### Tests
* **Vectors** (`vectors.test.ts`, no server; fixture keys `FsKeys.fromBundle` → `db('app').index(22…22)`):
  * `prolly.boundaries` and `prolly.shards`
  * `chunk`/`apply` from empty on `prolly.entries` gives every node's bytes and id and the RootRecord of `prolly.tree`
  * `apply(tree, inserted, deleted)` gives `prolly.after`, with exact put and delete sets: new nodes minus old, old minus new
  * the sealed blob, parts and head with part size 64
  * a `decodeSortKey` round trip
* **In-memory property test (`prolly.test.ts`, no server):**
  * a memory `NodeStore`, seeded xorshift, several seeds, fanout 4 and 64
  * random batches of adds and removes, idempotent repeats included
  * after each batch, the root and node set equal `IndexKeys.build(all)`
  * the store holds exactly the reachable nodes (no leaks, nothing missing)
  * one run with fanout 2^31 and over 2,000 keys hits the 1,024-entry cap; one with ~4 KB keys hits the 60,000-byte cap
  * a cursor forward and reverse from random bounds equals the sorted model
* **Server (`db.test.ts`, new describes):**
  * a private index on `(status text, created int desc)`:
    * equality plus range, `orderBy` both ways, `prefix`, paging with cursors
    * nulls ordered first
    * your own writes visible in the same transaction
  * `unique` + private
  * **concurrent writers:** N parallel inserts into one index all land, and the stored root equals `IndexKeys.build` of the TS sort keys of every row (an exact canonical check)
  * **shards:** writers to rows in different shards (pks chosen by `IndexKeys.shard`) with a barrier don't conflict (one run each), and a range query merges the shards in order
  * **decoys:** every stored node's plaintext is a bucket size, and queries stay correct after many writes
  * **missing node:** in a long-mode transaction, read a root and cache nothing, let another `Db` rewrite the tree, then read on: a node is missing, and the transaction re-runs and succeeds (count the runs)
  * **sealed:** the same queries as private, a unique violation, and `max_bytes` giving `too_large`
  * **backfill:** a private index and a sealed one, built over existing rows with `pageSize` 2, give exactly the canonical tree (private) or list (sealed). The private index is dropped, and its `n` and `r` keys are gone.
  * **property:** about 80 random rows (with nulls) and about 40 random queries (`=` / range / prefix / `orderBy` / limit with paging) over fast, private, sealed and scan, all equal to a naive in-memory model (`compareValues`, sorted by the sort-key order of the chosen index), with private and sealed giving the same answers
* **Verification:** `npx biome check`, `npx tsc` for client, fuse and db (`packages/db/tsconfig.test.json`), `npx vitest run` several times (stored-key order is random per run), and CI green on the PR before stopping.

#### Rhythm
Log the 5b line in `docs/MILESTONE-5.md` (and here), commit (spec separately if any), push, update or open the PR, then stop for compaction. 5c is next.

<details><summary>Original 5b outline</summary>

**Files:**
* `index/prolly.ts`:
  * The Node and RootRecord codecs.
  * **Incremental canonical writes (§5.4.4):** re-chunk the touched leaves until a boundary lines up with an old one, level by level, with boundaries batched per level through `IndexKeys.boundaries`. Then write new nodes, delete old ones, and write or delete the root.
  * **Reads (§5.4.3):** the root in the read set, nodes by `snapshotGet` with no conflict, and an immutable per-index node cache by id. A missing node → restart (outside a transaction) or a retry (`conflict`-like re-run via an `onError`/throw inside one).
  * **Shards (§5.4.5):** the shard by `IndexKeys.shard`, and a K-way merge for range reads.
  * **Decoys (§5.4.6):** a random leaf by `count` descent, a fresh salt, the node padding buckets.
  * Changes from one transaction are applied together at commit build (§5.5 last paragraph): writes are collected per index and the tree is rewritten in a pre-commit step of `DbTransaction`.
* `index/sealed.ts` (§5.7): the Blob, parts padded to a power of two, SealedHead with digest and count, a cache validated by the head version, uniqueness checked in the list, `max_bytes` → `too_large`.
* `query.ts`: plan step 4 (a private or sealed range: leading `=` fields plus a range or `orderBy`), `orderBy` on the client otherwise or `needs_index`, `limit`, a sort-key cursor with `after`, `prefix` and `in`, and isolation per §6.3.
* Backfill hooks for private and sealed.

**Tests 5b:**
* TS incremental trees equal the vector tree and `IndexKeys.build` (the Rust reference) after every step.
* **Property:** random insert and delete orders → an identical root and node set.
* Shard selection matches the vectors, and two shards' writers don't conflict.
* Decoys: the node count and sizes stay in buckets, and the tree stays correct.
* A sealed index matches the vectors and its cap.
* **Property:** query results equal a naive in-memory model, over private, sealed and fast; sealed answers like private.
* A missing node under a concurrent writer → a restart.
* Concurrent writers never lose an entry.

</details>

### 5c: transaction modes, cache coherence, files, bulk import, tabs (detailed plan)

**Context.** 5a and 5b (PRs #14, #15) give `@zen/db` its rows, indexes and queries, in `short` and `long` transactions with automatic retry. 5c finishes Part A of zendb.md:
* the `auto` mode and manual transactions (§7.1–7.2)
* cached rows (§7.7)
* file operations in a transaction (§7.6)
* bulk import (§7.5)
* the owner tab (§9.2)
* the database half of `docs/DB.md`

**User decisions:**
* **Tabs:** owner election only. The BroadcastChannel proxy of transactions and subscriptions moves to step 6, where subscriptions need it; until then every tab runs its own operations (safe: transactions are serializable).
* **Row cache:** build a small per-`Db` LRU.

**After compaction, re-read:** this section; `spec/zendb.md` §7, §9; `packages/db/src/{db,txn,query}.ts`; `packages/client/src/kv.ts` (`Transaction`, `transaction`, `sendCommit`); `packages/client/src/tree.ts` (`upload`, `writeOp`, `writeIn`, `TreeBatch.commitIn`, `Prepared`); `packages/client/test/tree.test.ts` (tree setup); `packages/db/test/db.test.ts` (setup, `barrier`, `code`).

**Branch/PR.** If PR #15 is merged, restart the branch from main and open a new PR; otherwise 5c joins #15 (update its title and body).

#### Client changes (`packages/client/src/kv.ts`)
1. **`TxnOptions.retry?: (e: unknown) => boolean`:** which errors re-run `fn` (default `ZenError.retryable`). `auto` uses it to stop the short phase at `too_old`.
2. **`Transaction.getEntries(keys)`:** like `getAll`, recorded the same way (conflict or `expect`), but returning `KvEntry | undefined` with the version. Own writes come back with no version, and are never cached. `getAll` becomes a wrapper of it.
3. **`commitTransaction(tx): Promise<CommitResult | undefined>`:** the body of the commit step of `transaction()`: nothing to send → undefined; otherwise `sendCommit`, `tx.result`, then the commit hooks. `transaction()` uses it. Exported for manual transactions; on a failure the caller runs `tx.errorHooks`.
4. Tests in `kv.test.ts`:
   * `retry` stops at a chosen code
   * `getEntries` versions equal `snapshotGet`'s
   * `commitTransaction` runs the hooks

#### `@zen/db`
1. **`auto` mode (§7.1), in `Db.transaction`:**
   * **Modes:** `mode: 'short' | 'long' | 'auto'`, with `DbOptions.autoSwitchMs` (default 3,000) and a per-call `autoSwitchMs`.
   * **Short phase:** `fs.transaction` in short mode with `retry` = conflict only. The `DbTransaction` gets a `deadline`, and `checkDeadline()` throws an internal `SwitchToLong` once it has passed. It is called at every table read, query page, index read, and in `finish` before the commit.
   * **Switch:** on `SwitchToLong` or a `too_old` (from a read or the commit), re-run the whole function once in long mode, with the remaining attempts.
   * **Long phase:** a `ZenError` `too_large` from a long-mode range becomes `DbError('too_large', 'a range too large for a long transaction: split the work')`. That conversion applies in plain `long` mode too.
   * **Report:** `DbTransaction.mode` gives the phase, so tests and callers can see it.
2. **Manual transactions (§7.2): `db.begin({mode: 'short' | 'long'})` → `ManualTransaction`.**
   * It extends `DbTransaction` with a raw `new Transaction(fs, mode)` and has `commit()` and `abort()`.
   * **`commit()`:** runs `finish()`, then `commitTransaction(raw)`. On an error it runs the raw error hooks, which refresh the caches, and rethrows: `conflict` reaches the caller, with no retry.
   * **After `commit()` or `abort()`:** every method throws `DbError('closed')`, a new code.
   * `consumes` comes in step 6. `mode: 'auto'` is refused (`bad_value`).
3. **Row cache (§7.7):**
   * **Storage:** `Db` keeps an LRU of `row stored key hex → {version, fields, parts}`. `DbOptions.rowCache` sets its size (default 1,024; 0 turns it off).
   * **`TableTx.read`, in this order:**
     * a key this transaction wrote → read as now
     * a cached entry → `raw.expectKey(key, version)` (into the read set in both modes) and record it in `DbTransaction.cached` (shared with the catalog records)
     * otherwise `raw.getEntries([key])` and cache the result (a row with parts is cached decoded)
     * Absent rows are not cached.
   * **Queries:** the pk lookup goes through `read`. Rows fetched by index and fast paths use `getEntries` and fill the cache. Scans fill it too (`rangeStored` gives versions).
   * **Invalidation:**
     * After a commit, the `onCommit` hook drops every row key the commit wrote. Versions aren't known before commit, so they aren't updated in place.
     * On `conflict`, the `onError` hook drops every cached row the transaction used.
     * `Db.close` clears the cache.
   * **Read-only transactions:** `finish` checks every cached record used (catalog and rows) with one `snapshotGet`. A stale one is dropped and `conflict` re-runs the transaction (this generalizes 5a's catalog check).
4. **Files (§7.6): `tx.fs(tree)` → `TxFiles`, in a new `files.ts`.**
   * `write(upload, {replaces?})` turns an upload into its op with `tree.writeOp(upload, replaces)`.
   * `ops(batch: TreeBatch | PendingOp[])` adds a batch's ops.
   * Both call `tree.writeIn(tx.raw, ops)`, which brings the tree's own `onCommit`/`onError` hooks (accept, and rebase on `stale_op`/`clock_skew`).
   * A second `write` to one node in one transaction is refused (`bad_value`; fs.md: one write per node per commit).
   * Chunks are uploaded before the transaction with `tree.upload(node, data)`, which docs and tests show; a write therefore costs only its op inside the transaction.
   * The ops count in `checkLimits` already (`crdt_ops`, `chunks`).
5. **`db.importRows(table, rows, {replace?, onCommit?})` (§7.5):**
   * **Batches:** commits of `n` rows, starting at 256. Each uses `insert`, or `put` with `replace`.
   * **On `too_large`** (the client's estimate, or the server's 413): halve `n` and retry the same rows. A single row that is too large rethrows.
   * **After 4 successes in a row:** double `n`, up to 4,096.
   * **Result:** the number of rows written. Each commit is atomic, the load isn't, as documented. `onCommit(done)` reports progress.
6. **Owner (§9.2): `tabs.ts`.**
   * **Election:** `Db.open(…, {locks?, owner?: boolean})`. With `navigator.locks` (or injected `locks`), a background `locks.request('zen/db/<fs id hex>/<ns>', {mode: 'exclusive'}, held)` holds the lock until `db.close()`.
   * **Events:**
     * `db.isOwner` is the current state.
     * `db.onOwner(fn)` (and `fn` unsubscribes) fires on becoming owner.
     * `db.ownerChange` is a promise for tests.
   * **Without Web Locks:** every instance is its own owner at once.
   * **Handover:** `close()` releases the lock (resolves the held promise), and the next waiting instance becomes owner.
   * Step 6 hangs consumers, the scheduler and the stream off `onOwner`. Migrations still run in every instance, which is safe (paged, conflict-checked), as now.
7. **Errors:** `DbError` codes gain `closed`. A long-mode range `too_large` maps as above.
8. **Docs:**
   * New `docs/DB.md` (the database part):
     * opening, schema and migrations
     * tables and rows
     * indexes: kinds, options, leakage (the §5.6 table)
     * queries and paging
     * transactions: modes, `begin`, limits, `importRows`
     * files in a transaction
     * caches and coherence
     * tabs and the owner
     * errors
   * `docs/CLIENT.md`: the new client methods.
   * `spec/TECH_DEBT.md`:
     * TD-DB-PERSISTENT-CACHE: caches are memory only; an encrypted persistent cache is undecided (§9.3)
     * TD-DB-TAB-PROXY: the proxy moves to step 6; resolved there
     * TD-DB-OBLIVIOUS-INDEX
     * TD-DB-SHAREDWORKER: a SharedWorker owner

#### Tests (`packages/db/test/txn.test.ts`, a new file, its own server)
* **auto:**
  * A function that awaits past a lowered `autoSwitchMs` on its first run commits in long mode on the second (runs = 2; `tx.mode` short then long).
  * A forced `too_old` (a short read held past the server's window via a test limit, or a thrown `ZenError(409, 'too_old')` from inside `fn` on the first run) switches to long.
  * A long run over a range larger than `max_range_items` fails with `too_large`.
* **begin:**
  * commit works
  * a concurrent write makes `commit()` throw `conflict` once, with no retry
  * use after `commit` or `abort` throws `closed`
  * `abort` sends nothing
* **Row cache:**
  * A second `get` of a row in a new transaction costs no read. A counter on `raw.getEntries` via a wrapping test hook, or the cache hit count `db.stats.rowHits`.
  * A row changed by another `Db` → a write transaction using the cached copy conflicts once, re-runs with fresh data and commits the right value.
  * A read-only transaction with a stale cached row re-runs and returns the new value.
  * `rowCache: 0` disables it.
* **Files:**
  * Upload, then a transaction that inserts a row and writes the file → both visible.
  * A conflict on the row (a concurrent writer, with a barrier) re-runs both, and the file has one version.
  * Two writes to one node → `bad_value`.
* **importRows:**
  * 1,000 rows into a table with a private and a unique index under a low `max_commit_ops`: several commits (count `onCommit`), every row present, the tree canonical (`expectCanonical` moved to a shared test helper).
  * A duplicate in the input → `unique_violation`, with the earlier commits kept.
* **Owner:**
  * Fake `locks` (an in-memory exclusive queue): the first `Db` owns, the second waits; `close()` on the first makes the second owner (`onOwner` fires once).
  * Without `locks`, `isOwner` is true at once.

**Verification:**
* `npx biome check`
* tsc for client, fuse, db
* `npx vitest run` (several times)
* CI green on the PR before stopping

#### Rhythm
Log the 5c line in `docs/MILESTONE-5.md` (and here), commit, push, update or open the PR, then stop for compaction. Step 6 (the broker) is next.

## Step 6: `@zen/db` part B, the broker (zendb.md §10–13)

Three sub-steps, **one PR each**, with the same rhythm as step 5.

**Context.** Part A of zendb.md (step 5, PRs #14–#16) is in place. Part B makes `Db` a message broker over the M4 client's topics, groups and stream. No server changes are needed. Every broker write rides in the same `/v1/commit` as the rows.

**User decisions:**
* split into 6a (primitives), 6b (patterns), 6c (sagas, tab proxy, fault suite)
* **tab proxy:** consumers, `serve`, the scheduler and saga orchestrators run only in the owner. Transactions stay local in every tab. **`on` subscriptions (and ephemeral and inbox ones) are proxied:** only the owner holds a stream, and it forwards events to the other tabs over a `BroadcastChannel` (6c).

**After compaction, re-read:**
* this section
* `spec/zendb.md` §2.3, §9.2, §10–13, §16
* `packages/client/src/{log,stream,kv,clock}.ts`
* `packages/db/src/{db,txn,catalog,migrate,keys,row,tabs}.ts`
* `packages/db/test/{txn,db}.test.ts` (setup, `FakeLocks`, `barrier`)
* `packages/client/test/{log,stream}.test.ts` (second device, `take`)

**What exists (survey):**
* **Topics** (`log.ts`):
  * `fs.topic(...path)` → `Topic`.
  * `append(payload, {key, causation})` and `appendIn(tx, payload, opts)`.
  * `read({after, key, limit})`.
  * `TopicEvent {offset, keyToken?, sender, hlc, causation, payload}`. It carries only the key token, never the key.
* **Groups:**
  * `topic.group(name: bytes | string, GroupOptions)` sends the name as raw bytes. `TopicKeys.groupId(name)` (wasm) has no caller yet.
  * `Consumer.deliveries({signal, ttlMs})` (leases renewed; it ends with a 412 `not_leader`), `next`, `ack(d, tx)` (pushes onto `tx.extra.consume`), `nack(d)` (outside a transaction), `dlq`.
  * `Delivery` adds `from`, `token`, `attempts`, `partition?` and `error?` (undecryptable).
* **Leaders:** `topic.leader(name, {ttlMs, onLost})` with `campaign(signal)`, `renew` and `stop`.
* **Stream:**
  * `session.stream()`, then `subscribe(Topic | {fs, prefix, topics}, {after, onEvent, onError})`. Without `after` it starts at the read version.
  * Reconnects resume after the last offset received.
  * Also `publish(topic, data)` and `subscribeEphemeral`.
* **Errors:** 412 `cursor_moved`, `claim_lost` and `not_leader` arrive as `ZenError`.
* **Time:** `session.clock.now()` is corrected toward the server after `clock.sync(client)`.
* **Vectors:** `zendb.json` `broker` holds `msg`, `body_ref`, `event_id` and `group_ids`, none of them tested yet.
* **db package:**
  * `ChangeDef {topic, image}` is stored in the TableRecord, but nothing emits change events yet.
  * No system tables exist, and apps may still name tables `$…`.
  * `TableTx.write(t, pk, pkEl, old, row)` (txn.ts) is the single place where every row is written.
  * `checkLimits` already counts `append` and `consume`.

### 6a: messages, emit, `on`, consume, change events, ephemeral

**Files** (`packages/db/src/`):
1. **`msg.ts`:**
   * Msg codec (§10.1): integer keys 1–10. `type` is required, and `zen.` prefixes are refused for apps (`bad_value`) except the class's own types. `id` is 16 random bytes.
   * `BodyRef` codec.
   * `eventId(topic, offset)` = `topic.id ‖ offset` (§10.2).
   * `Message<B>`: `{type, id, body, corr?, replyTo?, deliverAt?, saga?, step?, headers, topic, offset, keyToken?, sender, hlc, causation, eventId}`.
   * `openMsg(db, event)`:
     * decodes the Msg
     * resolves a `body_ref` by `snapshotGet` of the parts and `joinParts` (digest check)
     * a payload that doesn't decode, or missing parts, → `DbError('poisoned')`, a new code
2. **`keys.ts`:** `msgPart(id, i)` = `D ‖ ("m", id, u32(i))`. Check it against the `database` vectors if they list it.
3. **`broker.ts`:**
   * `db.topic(...path)`, cached per `Db` and closed by `close()`.
   * **`emit(topic, type, body, {key?, id?, headers?})`** on `DbTransaction` and on `Db`. On `Db` it is its own transaction.
     * It encodes the Msg. If the estimated envelope is over `max_envelope_bytes` (from `/v1/info` limits), the body goes to parts at `msgPart` in the same commit, and the Msg carries a `body_ref` (§10.3).
     * `appendIn(tx.raw, …, {key, causation: tx.causation})`.
     * A `ttl` (part deletion) comes with the scheduler in 6b.
   * **`on(target, handler, opts)`**, where `target` is a `Topic`, or `{prefix: Topic, topics?: Topic[]}`, and `opts` is `{after?, cursor?, batch?}`. It returns a `Subscription` with `close()`.
     * **Plain:** a stream subscription that decodes Msgs and calls `handler(msg)`. A poisoned message goes to `opts.onError` and is skipped.
     * **`cursor: name` (§11.2):** `handler(msg, tx)`.
       * The `$cursors` row `[device fp, name]` gives the start offset (read before subscribing).
       * Each batch (default 1) runs in one `db.transaction`. It reads the cursor row, skips events at or before it, runs the handler and writes the new offset.
       * A handler that throws stops the subscription and reports to `onError`. The cursor isn't advanced, so a restart retries.
     * It runs over a `StreamSource` interface: `subscribe(target, after, onEvent)` and `publish`/`subscribeEphemeral`. 6a's source is a direct `session.stream()`, opened lazily per `Db`. 6c adds the proxy behind the same interface.
   * **`consume(opts, handler)` (§11.3)** returns a `ConsumerHandle {stop(), running}`.
     * **Options:** `{group, topic, mode, partitions?, key?, maxInflight?, maxAttempts?, onPoison?, start?, txMode?}`.
     * **Group:** the server name is `topic.keysAt(epoch).groupId(group)`, checked against the vectors (§11.3). Creation through `topic.group(idBytes, def)`; `group_exists` passes through.
     * **Owner gating:** the loop starts on `db.onOwner` and stops on `close()` or `stop()`. Without locks the instance owns at once, as now.
     * **Loop per delivery:**
       * Run `db.transaction(tx => { handler(msg, tx); consumer.ack(d, tx.raw) })` with `tx.causation = eventId`.
       * `conflict` and `too_old` retry inside `transaction()`.
       * A handler throw, or a poisoned or undecryptable delivery, → `nack` (DLQ after `maxAttempts`, or `block`).
       * `cursor_moved` / `claim_lost` → drop the delivery.
       * `not_leader` → restart `deliveries()` (re-lease).
       * Network errors → back off and continue.
     * Leased modes use `deliveries()`. `per_key` uses `next()` with `maxInflight` handlers in parallel.
   * **`msg.ack()`** sends a commit holding only the consume step.
   * **`db.begin({mode, consumes: msg})`:** `ManualTransaction` gets the delivery. `commit()` adds the consume step, and its emits get the causation.
   * **`tx.after(fn)`** runs `fn` from `raw.onCommit`, at most once per transaction; errors are only logged.
   * **Ephemeral (§11.4):** `db.publishEphemeral(topic, type, body)` and `db.onEphemeral(topic, handler)` carry a Msg (type, id, body) through the stream source.
4. **System tables (§11.5):**
   * `$cursors`, `$sched` (with a private index `by_at` on `at int`), `$sagas`, `$inbox` and `$ixrows`, as TableRecords.
   * They are created in `init()` when missing: with the DbRecord for new databases, and idempotently for 5a–5c databases.
   * `createTable` and `renameTable` refuse app names starting with `$` (`bad_value`).
   * Migrations and schema numbers ignore them.
5. **Change events (§12.7):**
   * `DbTransaction.changes`: a `Map<tableId‖pkEl, {t, pk, first old, last new}>`, recorded in `TableTx.write` for tables with `changes`.
   * In `finish()`, before `checkLimits`, append one `zen.change` Msg per row whose state changed. Body `{op: 'put' | 'delete', table, pk, row?}`; key = pk element; topic from the `ChangeDef`.
     * `image: 'full'` includes the row when the envelope fits, and leaves it out otherwise.
     * A row inserted then deleted in one transaction emits nothing.
   * `table.on(handler, opts)` is an `on` of the change topic.
6. **Errors:** new codes `poisoned`, and `timeout` (used from 6b).

**Tests 6a** (`packages/db/test/broker.test.ts`, its own server, `world({limits: {claim_ttl_ms, …}})`):
* **Vectors:** `msg` and `body_ref` encode byte-exactly; `event_id`; `group_ids` via `groupId`.
* **emit:**
  * Inside a transaction the message exists only if the transaction commits (a thrown `fn` leaves the topic empty).
  * A conflict re-run emits exactly once.
  * Outside a transaction it is its own commit.
  * A large body uses parts and round-trips; deleted parts → `poisoned`.
* **on:**
  * Plain: from now, and with `after` (history, then live).
  * `cursor`: effects exactly once across a simulated crash. Close mid-batch, reopen another `Db` with the same device: no duplicate rows, nothing missed. Also `batch: 3`.
  * Prefix with `topics`.
* **consume:**
  * `sequential`: handler effects plus emits with causation = the consumed event id.
  * A throw → nack → DLQ after `maxAttempts`.
  * Two `Db`s on one `per_key` group share the keys, with no duplicates.
  * A non-owner (`FakeLocks`) doesn't consume until the owner closes.
  * `begin({consumes})` and `msg.ack()`.
* **Change events:** put, update and delete produce one event per row per transaction, with key order per row; `image: 'full'`; `table.on`.
* **Ephemeral:** two sessions.
* **Tables:** system tables exist after open (also on a database created before them); `$x` app tables are refused.

### 6b: scheduler, work queues, request/reply (detailed plan)

**Context.** 6a (PR #17, merged) gives messages, `emit`, `on`, consumer groups (owner only), change events and the system tables. 6b adds the §12 patterns that need more than one primitive:
* the scheduler (delayed messages and `gc` jobs)
* work-queue retries
* request/reply

They are all built from 6a's pieces and `$sched` / `$inbox` rows. No server changes.

**After compaction, re-read:**
* this section and the 6a log line
* `spec/zendb.md` §10.3, §11.3, §11.5, §12.2–12.4, §12.6
* `packages/db/src/{broker,msg,system,txn,db}.ts`
* `packages/db/test/broker.test.ts` (setup, `until`, `events`, `FakeLocks`)
* `packages/client/src/log.ts` (`Leader`), `clock.ts` (`now`, `sync`)

**Branch/PR.** PR #17 is merged: restart the branch from main and open a new PR. The plan commit goes in first.

#### Files
1. **`broker.ts`, a refactor:** `appendMsg` splits into `sealMsg(tx, m, lean?) → payload` (body parts written in the transaction, as now) and the append. Schedules store payloads already sealed this way, so a large scheduled body already sits in parts.
2. **`sched.ts`, delayed messages (§12.4):**
   * **The `$sched` row:** `{id: bytes16, at: int, kind: 'emit' | 'gc', topic?: [bytes], key?: bytes, msg?: bytes, gc?: [[bytes]]}`.
     * `msg` is the Msg's CBOR, `deliver_at` included.
     * `key` is the event key's bytes (a string key as UTF-8).
     * `gc` paths are element lists **under D** (decision, see Spec below), deleted as `keys.key(...elements)`.
   * **`tx.schedule(topic, type, body, {at | delayMs, key?, id?, headers?}) → id`** and `db.schedule(…)`, which is its own transaction.
     * `at` is in unix ms, by `session.clock.now()`.
     * The row is inserted through `tx.table('$sched')`, so it goes through the private index.
     * On commit, the local scheduler is woken if `at` comes before its next wake.
   * **`tx.cancel(id)` / `db.cancel(id)` → boolean**: deletes the row, and returns whether it was still there.
   * **`tx.gc(paths, {at | delayMs})`** (internal) inserts a `kind: 'gc'` row.
   * **`emit(…, {ttlMs})`:** when the body went to parts, a `gc` row for those parts is added at `now + ttlMs` (§10.3).
   * **`Scheduler`:**
     * **Owner:** `db.scheduler`, started from `onOwner` when `DbOptions.scheduler !== false`. Options `DbOptions.schedulerOptions {pollMs = 1000, ttlMs = 10_000, batch = 100}`.
     * **Leader:** `db.topic('zen', 'db', ns, 'sched').leader(groupId('sched'), {ttlMs, onLost})`, campaigning in the background.
     * **Loop, while leading:**
       1. Once at start (and every 10 minutes), `await clock.sync(client)`.
       2. A short transaction: `tx.table('$sched').query().where('at', '<=', now).orderBy('at').limit(batch).page()`. For each row, `emit` appends the stored payload with its key (`topic.appendIn`, causation empty) and `gc` deletes the paths; then it deletes the row.
       3. A `too_large` halves the batch.
       4. Then it sleeps until `min(next at, now + pollMs)`. The next `at` comes from a one-row query; a local wake (schedule on this `Db`) cuts the sleep short.
     * **Correctness** never depends on the lease (§12.4): two schedulers conflict on the index root and the rows, and one re-runs.
     * **`stop({release = true})`:** `release: false` drops the lease without releasing it, which simulates a crash in tests.
     * `db.close()` stops it.
3. **Work queues (§12.2), in `broker.ts`:**
   * **`EmitOptions.spread`:** a fresh random 16-byte key (with `key` given → `bad_value`).
   * **`ConsumeOptions.retry: 'nack' | 'delay'`** (default `nack`) and **`backoff?: (attempt) => ms`** (default `min(1000 · 2^(attempt−1), 300_000)`).
     * With `delay`, a handler throw runs a transaction: ack the delivery, then either schedule a copy, or dead-letter it.
     * **The copy:** same Msg `id`, `body`, `corr` and `reply_to`, with header `zen.attempt` = attempt + 1. It is scheduled at `now + backoff(attempt)` with the same key.
     * **The attempt** of a delivery is `headers['zen.attempt'] ?? 1`.
     * **After `maxAttempts`** (default 5): an immediate `tx.emit` of the copy to `topic.child('zen.dlq')` (decision: immediate, not scheduled).
     * Headers starting with `zen.` are refused in app `emit` (`bad_value`).
4. **`reply.ts`, request/reply (§12.3):**
   * **`ReplyError(message, data?)`** is exported. An error reply has type `<type>.error` and body `{message, data?}`. On the requester's side, `request` rejects with a `ReplyError` built from it.
   * **The inbox:** `db.inbox`, created on first use.
     * `instance` = 16 random bytes per `Db`; topic `('zen', 'inbox', instance)`.
     * An `on` subscription from the beginning (`after` = 12 zero bytes). The topic is new and only this instance's, so there is no history to replay, and no race between subscribing and the first reply.
     * Plus an `onEphemeral` subscription.
     * `pending: Map<corr hex, Waiter>`. A reply whose `corr` is unknown is dropped (late or foreign).
   * **`db.request(topic, type, body, {timeoutMs = 10_000, key?, corr?}) → Promise<Message>`:**
     1. It awaits the inbox's start.
     2. It emits `{type, id, corr (default random), reply_to: inbox path, body}`.
     3. It waits for one reply. The timer starts after the commit; at the end, `DbError('timeout')`.
     * A retry after a timeout passes the same `corr` (dedup).
   * **`tx.request(…) → Promise<Message>`:**
     * It emits in the transaction and registers the waiter in `onCommit`. The inbox starts at once, so its subscription runs before the reply can exist.
     * If the attempt doesn't commit, its promise never settles: a re-run makes a new one. It must be awaited after the transaction, never inside it (documented).
   * **`db.requestAll(…, {timeoutMs, max?}) → Promise<Message[]>`:** scatter-gather. It collects replies with the `corr` until the timeout, or until `max` replies, and resolves with what it has (no `timeout` error).
   * **`db.serve(opts: ConsumeOptions & {dedup?, reply?: 'event' | 'ephemeral'}, handler(msg, tx) → body)`:**
     * a `consume` whose transaction emits the reply to `reply_to` (`db.topic(...replyTo)`): `{type: type + '.reply', id, corr: msg.corr ?? msg.id, body}`
     * `ReplyError` → a `.error` reply in the same transaction, with the request consumed (not nacked)
     * any other throw → `nack` / `retry`, as for `consume`
     * a request without `reply_to` → the handler runs, and no reply is sent
     * **`reply: 'ephemeral'`:** the reply is published with `publishEphemeral` from `tx.after`
   * **`db.reply(msg, body, {ephemeral?})` / `tx.reply(msg, body)`:** for plain `on` responders (scatter-gather) and manual consumers.
5. **Dedup (§12.3, §12.6), `ConsumeOptions.dedup?: boolean` and `dedupTtlMs` (default 7 days):**
   * **Row:** `$inbox [scope = group name, id]`, where `id` = `msg.corr ?? msg.id` for `serve` and `msg.id` for `consume`. Fields `{at, reply?}`.
   * **In the handling transaction:**
     * The row is read first. If present: skip the handler, ack, and (`serve`) send the stored reply again.
     * Otherwise run the handler, then insert the row (with the reply for `serve`), and `tx.gc` its own row key at `now + dedupTtlMs`.
6. **Spec (`spec:` commit, ⚠️ SPEC CHANGES):**
   * §11.5: `$inbox` gains `reply?: any` (the reply a dedup responder sends again). `$sched` `gc` paths are element lists under `D`; `key` is the event key's bytes.
   * §12.2: header `zen.attempt`; the class DLQ is an immediate emit to `<topic> ‖ "zen.dlq"`. App headers may not start with `zen.`.
   * §12.3:
     * the inbox subscribes from the start of its topic
     * error replies have body `{message, data?}`
     * `tx.request` settles only if its transaction commits
     * `requestAll` resolves with what arrived by the timeout
   * §12.4: `stop` and the wake-up on a local schedule are informative only. No change.
7. **Docs:** none here (6c writes the broker part of DB.md). TECH_DEBT is unchanged.

#### Tests (`packages/db/test/patterns.test.ts`, its own server)
`world({limits: {claim_ttl_ms: 1000}})`. Scheduler options `{pollMs: 100, ttlMs: 1000}`, so failover takes about a second.
* **Scheduler:**
  * Rows at `now+300`, `now+600` and `now` are emitted in `at` order, each exactly once, and none before its `at` (compare the event's sealed HLC time, or the receive time, against `deliver_at`).
  * `cancel` before delivery → true, and nothing is emitted. After delivery → false.
  * A transaction that schedules, then throws, leaves no row.
  * `emit(…, {ttlMs: 200})` with a large body: the parts are gone after the gc, and `openMessage` → `poisoned`.
  * **Two schedulers racing** (two `Db`s with `locks: null`, both owners, and the lease bypassed by calling the internal `runOnce()` on both concurrently over 50 due rows): every row is emitted exactly once.
  * **Failover:** A leads; `a.scheduler.stop({release: false})`; B takes the lease after the TTL and delivers rows scheduled meanwhile.
* **Work queues:**
  * `spread` messages on one `per_key` group with two `Db` consumers: both handle some, and all are handled once.
  * `retry: 'delay'` with a fixed backoff of 100 ms and `maxAttempts: 3`:
    * the handler sees attempts 1, 2, 3 (`zen.attempt`), then the message lands on `topic/zen.dlq` with the same `id`
    * a handler that succeeds on attempt 2 stops the retries
  * App headers or types starting with `zen.` are refused.
* **Request/reply:**
  * A round trip: `serve` does a row write plus the reply, and the requester gets the body.
  * Timeout, then a late reply is dropped: the responder delays past the timeout, and a second request still works.
  * `ReplyError` → the requester rejects with `ReplyError`, and the request is consumed (no redelivery).
  * **Dedup:** a request repeated with the same `corr` (after a timeout) runs the handler once (row count) and gets the same reply twice. An `$inbox` row exists with its `gc` row in `$sched`.
  * The ephemeral reply arrives.
  * Scatter-gather: two `on` responders (two `Db`s) each `db.reply`, and `requestAll` returns 2.
  * A request in a transaction that throws is never sent: the responder sees nothing, and the promise doesn't settle.
* **Verification:**
  * `npx biome check`
  * tsc: client, fuse, db test
  * `npx vitest run` three times
  * CI green on the new PR before stopping

#### Rhythm
1. Restart from main.
2. Commit the plan (`docs: milestone 5 step 6b plan`), push, open a new PR.
3. Build 6b on the user's go.
4. `spec:` commit, then the code commit.
5. Log the 6b line, push, update the PR, confirm CI is green, stop for compaction.

### 6c: sagas, the tab proxy, the fault suite, docs (detailed plan)

**Context.** 6a and 6b (PRs #17, #18) give messages, `on`, consumers, change events, the scheduler, work queues and request/reply. 6c finishes Part B of zendb.md:
* sagas (§12.5)
* the tab proxy (§9.2, TD-DB-TAB-PROXY), as the user decided: only the owner holds a stream, other tabs' `on`, inbox and ephemeral subscriptions go through it
* the fault suite (§13.2)
* the broker half of `docs/DB.md`

**After compaction, re-read:**
* this section, and the 6a and 6b log lines
* `spec/zendb.md` §9.2, §12.3, §12.5, §13
* `packages/db/src/{broker,reply,sched,db,txn,tabs}.ts`
* `packages/db/test/{patterns,broker,txn}.test.ts` (setup, `until`, `events`, `FakeLocks`)
* `packages/client/src/stream.ts` (`StreamEvent`, `start`, `offsetAfterVersion`), `log.ts` (`Topic.decode`)

**Branch/PR.** #18 is merged: the branch restarts from main, and the plan commit and 6c are a new PR.

#### Files
1. **`saga.ts` (§12.5).**
   * **API:** `db.saga<D>(name, steps, opts?)` → `Saga {start(data, tx?) → id, get(id) → SagaState | undefined, stop()}`.
     * A step is `{name, command: Topic, compensate?: Topic, timeoutMs?}` (`timeoutMs` default 60,000).
     * `opts.group` is the orchestrator's group name (default `"orchestrator"`).
     * `SagaState` is the `$sagas` row: `{id, saga, step, status, data, done, timeout?, corr?}`.
   * **Topics and keys:**
     * The reply topic is `("zen", "saga", ns, name)`.
     * Commands and replies are keyed by the saga id, so a `per_key` orchestrator sees each instance in order.
   * **Commands:**
     * Shape: `Msg {type: name + "." + step, id, corr: random, saga, step, reply_to: reply topic, body: data}`.
     * A compensation has type `name + "." + step + ".compensate"`, sent to `compensate`.
     * The row's `corr` is the corr of the command (or compensation) in flight.
   * **Replies:** `replyMsg` / `sendReply` (reply.ts) copy `saga` and `step` from the request, and key the reply by the saga id. So participants are plain `serve` handlers: return a body (ok), or throw `ReplyError` (fail).
   * **`start(data, tx?)`** runs in `tx` or a transaction of its own:
     1. insert the row (`status: running`, `step: 0`, `done: []`)
     2. emit command 0
     3. schedule the timeout to the reply topic: type `zen.saga.timeout`, same `saga`, `step` and `corr`, keyed by the saga id; its id goes in `timeout`
   * **The orchestrator** is a `consume({group, topic: reply topic, mode: 'per_key'})` (owner only). It handles each event in one transaction:
     * **No row, or a `corr` that isn't the row's:** stale. Ack only.
     * **Running, reply `<type>.reply`:**
       * cancel the timeout
       * merge an object body into `data` (shallow)
       * push the step to `done`
       * then emit the next command and its timeout, or set `status: done`
     * **Running, `<type>.error` or a timeout:** set `status: compensating` and start the compensations: the `done` steps that have a `compensate` topic, newest first.
       * none left → `failed`
       * an error reply's body is stored in `data.error`
     * **Compensating, `.compensate.reply`:** the next compensation, or `failed` when none is left.
     * **Compensating, `.compensate.error` or a timeout:** `stuck`. Decision: compensations run until their participant gives up, so a timeout there means an operator is needed.
   * **DLQ (stuck):**
     * The orchestrator also consumes, for each distinct compensate topic, its class DLQ `<compensate>/zen.dlq`: a `sequential` group `"<group>.dlq"`.
     * A dead-lettered compensation of this saga name whose corr matches its row's corr → `stuck`.
     * Documented: stuck detection needs participants with `retry: 'delay'`. With `nack`, the server DLQ isn't visible to the orchestrator, and the compensation's timeout covers it.
   * **Transitions:** each one is one commit, so it happens exactly once (§12.5). The row update, the next command, the timeout schedule, the cancel and the consume step are all in it.
2. **`proxy.ts`, the tab proxy (TD-DB-TAB-PROXY).**
   * **Injection:**
     * `DbOptions.channel?: ((name) => BroadcastChannelLike) | null`, defaulting to `globalThis.BroadcastChannel`.
     * The proxy is used only when Web Locks are used too (with `locks: null` every instance is its own owner).
     * The channel name is `zen/db/<fs id>/<ns>`.
     * Messages are structured-clone objects carrying `{from: tabId, to?}`, with a random tab id.
   * **`TabSource implements StreamSource`** replaces `DirectSource` as `db.source` when the proxy is on.
     * **Logical subscriptions:** `{sid, kind: 'sub' | 'esub', target, last, onEvent, onError}`.
     * **Two routes:**
       * **direct** while this tab owns: `DirectSource`
       * **remote** otherwise, to the owner tab named by the last `owner` announce
     * **Switching routes** (`onOwner`, an `owner` announce, the owner's `bye`) re-subscribes every subscription with `after = last`. Offsets at or before `last` are dropped, so a handover loses nothing and repeats nothing.
     * **Start offset:** a `sub` without `after` takes `offsetAfterVersion(/v1/grv)` in the tab itself (export `offsetAfterVersion` from `@zen/client`, or add `Session.readVersion()`). So the start doesn't depend on which tab owns.
     * **Remote subscribe** resolves on the owner's `ok {sid}`, and rejects on `err {sid, message}`. Before any owner is known, it sends `hello` and waits for an `owner` announce.
   * **Protocol:**
     * Requests from a tab: `sub {sid, prefix: topic id bytes, after}`, `esub {sid, path}`, `unsub {sid}`, `epub {path, data}`, `hello`, `bye`.
     * Replies from the owner: `ok`, `err`, and `owner {id}` (announced on becoming owner, and in answer to `hello`).
   * **Events stay sealed (log):** the owner subscribes with `{fs, prefix: topic id}` and no `topics`, so events arrive unopened, and forwards `ev {sid, topic, offset, keyToken, envelope}`. The tab opens each event with its own `Topic.decode` (exact topic id for a `Topic` target, the known topics for a prefix), which also observes the HLC.
   * **Ephemeral messages** are forwarded opened (`eev {sid, topic, sender, hlc, data}`): the stream has no sealed form for them. Same-origin only, like IndexedDB, as documented.
   * **`ProxyHost`, in the owner:**
     * serves the other tabs' subscriptions from its `DirectSource`
     * drops a tab's subscriptions on `unsub` or `bye` (sent on `pagehide` and `db.close()`)
     * publishes `epub` for other tabs
   * **Unchanged:** transactions stay local in every tab. Consumers, `serve`, the scheduler and the saga orchestrator already run only in the owner (`onOwner`).
3. **reply.ts and broker changes:** `replyMsg`/`sendReply` carry `saga`/`step` and the saga key (above). The inbox comment says "each `Db`".
4. **Spec (⚠️ `spec:` commit):**
   * §11.5 `$sagas`: `corr?: bytes`, and `timeout` is the `$sched` id.
   * §12.5:
     * the command, compensation and timeout message types
     * replies carry `saga`/`step` and the saga key
     * the stale rule by `corr`
     * ok bodies merged into `data`, and an error body in `data.error`
     * a compensation error or timeout → `stuck`
     * DLQ watching through the class DLQ, which needs `retry: 'delay'`
   * §9.2: the proxy protocol in brief. Events forwarded sealed, ephemeral ones opened. The start offset is resolved by the subscribing tab.
5. **Docs:**
   * `docs/DB.md` gains the broker part: messages and `emit`, `on`, consumers, ephemeral messages, the scheduler, work queues, request/reply, sagas, change events, tabs and the proxy, guarantees and failures (the §13 tables in practice).
   * `spec/TECH_DEBT.md`: TD-DB-TAB-PROXY resolved; TD-DB-SHAREDWORKER stays.

#### Tests
* **`packages/db/test/saga.test.ts`** (its own server, `claim_ttl_ms` 1000, fast scheduler). Two participant `serve`s (`reserve` with compensate `release`, `charge` with compensate `refund`) on another `Db`, `retry: 'delay'`.
  * **Happy path:** `done`, data merged, each participant ran once.
  * **A fail at step 2** (`ReplyError`): `release` runs once, then `failed`, with `data.error`.
  * **A timeout:** the step-2 participant never replies, `timeoutMs` 300 → compensation → `failed`. The late reply is stale: acked, and the state is unchanged.
  * **`start` inside a transaction that throws:** no row, and no command sent.
  * **`stuck`:** a compensation that always throws, with `maxAttempts` 2, reaches its DLQ → `stuck`.
  * **Two instances in parallel:** both finish.
* **`packages/db/test/proxy.test.ts`:**
  * **Setup:** a `FakeBus` (an in-memory `BroadcastChannel` by name, async delivery, `structuredClone`) plus `FakeLocks`. Two or three `Db`s on one fs and ns as tabs, with `DirectSource` stream opens counted.
  * **Only the owner opens a stream;** the non-owner's `on` receives events, both plain and `cursor`.
  * The non-owner's `request` gets its reply through its inbox via the owner, and its `onEphemeral` and `publishEphemeral` work.
  * **Handover:** the owner closes while events flow, and the second tab becomes owner. Its own and the third tab's subscriptions continue with no gap or duplicate (count and order of offsets), and its consumers start.
  * Without a channel (or with `locks: null`), the source is direct.
* **`packages/db/test/faults.test.ts`** (§13.2):
  * **A handler killed mid-transaction:** `stop()` with the handler blocked and uncommitted. Another instance gets the event after the lease or claim expires, and its effect happens once.
  * **A deposed leader commits late:** its ack fails `not_leader`, and none of its writes or emits land.
  * **Two consumers racing on a key:** `cursor_moved` / `claim_lost` drops one, and the effect happens once.
  * **A dropped stream** (`reconnect()` on the session stream): `on` misses nothing and repeats nothing.
  * **A replayed `commit_id`** (the same built commit sent twice): one effect.
  * **Scheduler failover mid-batch:** the leader stops between `runOnce` calls, and the rows are delivered once by the other.
  * **The owner tab closing** (proxy): covered in `proxy.test.ts`. Only referenced here.
  * **A responder that never answers:** `timeout`, then a retry with the same `corr` once the responder is back; dedup → one effect.
* **Verification:**
  * `npx biome check`
  * tsc: client, fuse, db test
  * `npx vitest run`, three times
  * CI green on the PR before stopping

#### Rhythm
1. Restart from main, log the 6c plan line, commit (`docs: milestone 5 step 6c plan`), push, and open a new PR.
2. Build 6c on the user's go: the `spec:` commit, then the code commit.
3. Log the 6c line (step 6 done), push, update the PR, confirm CI is green, and stop for compaction.

Step 7 (CRDT tables) is next.

### Original outline
* **Base:** `msg.ts` (Msg encoding, BodyRef, causation), `emit`, `on` (plain, `after`, `cursor`), `consume` (group id, the loop table of §11.3, `ack`, `begin({consumes})`), the system tables, ephemeral messages.
* **Patterns:**
  * work queues: spread mode; `retry: "delay"`
  * request/reply: inbox, `serve`, dedup, ephemeral replies, scatter-gather
  * scheduler leader: `$sched`, `gc` jobs, `cancel`
  * sagas: orchestrator, compensations, timeouts
  * `dedup`, `tx.after`, change events
* **Tests:** the guarantees and failures tables of §13, with fault injection:
  * killed handlers mid-transaction, deposed leaders, dropped streams
  * `commit_id` replays, scheduler failover, an owner tab closing (simulated owners)

## Step 7: `@zen/db` part D, CRDT tables (zendb.md §19)
* `crdt.ts`: `patch`, `incr`, `add`, `remove`, `delete`, the merged view through `/v1/crdt/*`
* change-event appends, the `$index` indexer group and `$ixrows`
* the offline queue: memory, or the persisted store of step 8, with rebase on `stale_op`
* `"crdt_rows"` feature detection

**Tests:** offline edits merging, counters across devices, add-wins sets, indexer lag and re-check, an indexer restart.

## Step 8: `@zen/fs`, the local replica (DESIGN-3 §6, TD-FUSE-REPLICA)
* **Model.** The node table and chunk store are kept current through `tree.changes`. Local ops are applied optimistically and queued while offline, then sent (with rebase).
* **POSIX-ish API:** `readFile`, `writeFile` (changed chunks only), `readdir`, `stat`, `mkdir`, `rename`, `unlink`, `rmdir`, `symlink`/`readlink`, xattrs in sealed meta, conflict copies.
* **Stores**, behind an interface:
  * `memory`
  * `node-disk`: a directory of sealed chunk files plus a node-table journal; no native deps
  * `indexeddb`: browser
  
  At rest, everything stays sealed as on the server.
* **`zen-mount`:**
  * moves onto `@zen/fs`
  * offline reads of cached files
  * `--cache-dir` becomes real
  * symlinks and xattrs
  * writes of changed chunks only
  * the existing mount tests stay green, plus offline and symlink tests
* **TDs:** resolve TD-FUSE-REPLICA.

## Step 9: `@zen/loro`, the Loro adapter (DESIGN-3 §0, §1)
* Dependency: `loro-crdt`.
* `zen.loro(db, path)`: a snapshot in a zen-db row, updates as events on a topic keyed by the document.
  * **Load:** snapshot + `read(after snapshot offset)`.
  * **Live:** `on` subscription.
  * **Local edits:** `emit` of Loro updates.
  * **Compaction:** periodic snapshots by a leader (DESIGN-2 §2.7).
* **Tests:** two clients converge; offline edits; a snapshot plus a tail load.

## Step 10: five small web apps (docs/EXAMPLES.md)
* **Layout:** `apps/{kanban,chat,booking,checkout,ledger}`, plain TypeScript and DOM with no framework, bundled with esbuild (a new dev dependency).
* **Serving:** each app is served as static files. In tests, a static server with zen-serve CORS, as in the M4 browser smoke test. They can be deployed into zen-serve's `/unencrypted`.
* **Sign-in:** password or passkey.

| App | Uses |
|---|---|
| Kanban | rows plus attachments in one commit; change events; drag; card text as a Loro doc; offline variant through CRDT fields |
| Chat | `on` with resume; ephemeral typing; presence scatter-gather; photo files; reactions as a CRDT counter |
| Booking | unique slot; waitlist `per_key` worker; reminders through the scheduler |
| Checkout | saga across stock, payment and shipping workers, as Node scripts in `apps/checkout/workers/`, with a mock payment provider that checks idempotency keys |
| Ledger | transfers with invariants; audit outbox; statement projection; paged history |

**Tests:** one Playwright end-to-end test per app, with two browser contexts where the demo needs them, following each app's "Milestone 5 demo" in EXAMPLES.md. The fault parts (killing workers, schedulers, tabs) run as vitest scenarios.

## Step 11: CI, docs, outcome
* **CI:**
  * the `rust` job covers CRDT rows on both backends (the existing fdb job)
  * the `client` job builds and tests `@zen/db`, `@zen/fs` and `@zen/loro`, the apps' bundles, and Playwright for all apps
  * check run time; split jobs if it passes ~15 minutes
* **Docs:**
  * `docs/DB.md`: usage guide of the `Db` class
  * `docs/CLIENT.md`: fs replica, zen-mount, Loro
  * `docs/STATS.md`: wasm size, prolly and sealed costs, broker latencies, CRDT row costs
  * `spec/OPS.md` pitfalls; README
* **TECH_DEBT:**
  * resolve TD-CRDT-ROWS-SERVER and TD-FUSE-REPLICA
  * add TD-DB-OBLIVIOUS-INDEX, plus anything deferred
* **Wrap-up:** the Outcome section, then a PR.

## Commits (push after each green step)
1. plan doc
2. `spec:` step 1
3. zen-core and zen-wasm db primitives + vectors
4. server CRDT rows + tests
5. client prerequisites
6. `@zen/db` database
7. `@zen/db` broker
8. `@zen/db` CRDT tables
9. `@zen/fs` + zen-mount
10. `@zen/loro`
11. apps (one commit per app)
12. CI, docs, outcome

**Parallel agents.** Steps 5–6, 8 and 9 can run as agents on disjoint packages once steps 2–4 have landed. They share one `target/` (OPS.md §4.9) and never push: I verify and push.

## Verification
* **Rust:**
  * `cargo fmt --check`
  * `cargo clippy --all-targets -- -D warnings`, default and `--no-default-features`
  * `cargo test --all`
  * `ZEN_TEST_BACKEND=fdb cargo test -p zen-store -p zen-server --features fdb` on the local cluster
  * the wasm32 checks
  * `gen_vectors` leaves `spec/test-vectors` unchanged
* **JS:** `scripts/build-wasm.sh && npm ci && npx biome check && npx tsc --noEmit` (all packages), `npx vitest run`, `npx playwright test` (the M4 smoke test plus five apps)
* **Manually:** each app's demo script from EXAMPLES.md
* **CI:** green on rust, pure-rust, fdb and client
* **Disk:** `CARGO_INCREMENTAL=0`; watch disk (8 GB free at the last check).
