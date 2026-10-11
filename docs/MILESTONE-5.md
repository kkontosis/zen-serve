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
  * **6c** (sagas, tab proxy, fault suite, docs): done, PR #19 (merged; CI green). **Step 6 is done.** As planned, with these details for step 7 on:
    * **Files:**
      * `saga.ts`: `Saga` (`db.saga(name, steps, {group, ttlMs, onError})` → `start(data, tx?)`, `get`, `stop`, `topic`), `SAGA_TIMEOUT`. The orchestrator is a `per_key` consumer of `('zen', 'saga', ns, name)` plus a `sequential` group `<group>.<name>.dlq` on each compensate topic's class DLQ. A timeout's `data.error` is `{message: 'timeout', step}`.
      * `proxy.ts`: `TabSource` (a `StreamSource`; `streaming` tells whether the tab holds a stream), `ProxyHost` (owner side), `BroadcastChannelLike`, `ChannelFactory`. Used when both `locks` and a channel exist (`DbOptions.channel`, default `BroadcastChannel`; `null` turns it off). The host filters an exact-topic subscription by topic id (it subscribes to the id as a prefix).
      * `reply.ts`: `RequestMsg` gains `saga`/`step`; replies copy them and are keyed by the saga id.
      * `DirectSource.reconnect()` (internal, tests).
    * **Client:** `offsetAfterVersion` is exported and `Session.readVersion()` is new (`/v1/grv`).
    * **⚠️ Spec:** §9.2 (the proxy protocol), §11.5 (`$sagas.corr`, `timeout`), §12.3 (an inbox per `Db`), §12.5 (rewritten: messages, the stale rule by `corr`, transitions, stuck through the class DLQ).
    * **Docs:** `docs/DB.md` sections 8 and 10–18 (the broker); TD-DB-TAB-PROXY resolved.
    * **Tests:** `saga.test.ts` (7), `proxy.test.ts` (4, `FakeBus` + `FakeLocks`), `faults.test.ts` (6; commits are intercepted by wrapping `session.call`). 215 vitest tests in all.
* **Step 7** (CRDT tables): planned (see Step 7: sub-steps 7a and 7b, a PR each). PR #19 is merged, so the plan commit restarts the branch from main and opens a new PR (#20, merged with the plan only: 7a was pushed after GitHub had registered its head).
  * **7a** (CRDT tables online): done, PR #21. As planned, with these details for 7b:
    * **Files:**
      * `crdt.ts`:
        * `mergeState(db, t, ObjState) → Merged`, `overlay(merged, pk, pending)`, `viewOf(t, merged)`. **7b's local view of queued operations reuses `overlay`.**
        * `crdtGet` (batches of 1,000) and `crdtRange` (one page).
        * `CrdtWrites`: a transaction's pending operations per row, in call order (`Pending`: row/lww/ctr/add/rem). `stamp()` runs in `finish` and stamps them. `release()` undoes it when the commit is never sent.
        * Counter totals and their lock live per session clock (a `WeakMap` on `TreeClock`), so every `Db` of one installation shares them. A failed commit forgets the totals it touched, which are then re-read with `crdt/get`.
        * `rebase` handles `stale_op` and `clock_skew` with `clock.sync(force)` (and `reset` when the clock ran ahead), at most 3 times.
      * `TableTx` dispatches on `merge`. It has `patch`, `incr`, `add`, `remove` and `crdtRows`; `put` and `update` are refused; `delete` returns true without reading.
      * `query.ts` `crdtScan` (path `'crdt'`, the cursor is the last pk); a bound pk goes through `get`.
      * `migrate.ts`:
        * `createTable({merge, fields})` checks the server and the options.
        * `dropTable` deletes live rows in pages, with the last object as progress.
        * `createIndex` on CRDT tables: `unique` → `bad_value`, the rest → `unsupported` (**7b replaces this**).
        * `transform` is refused.
      * `Db.features` and `Db.crdtKeys(id)` are new; `DbError` gained the code `unsupported`.
    * **Change events:** `noteChange(t, pk, pkEl, {}, alive ? {} : undefined)`. `emitChanges` never adds `row` for CRDT tables.
    * **⚠️ Spec:** §3.2, §8.3 (dropping a CRDT table), §12.7, §19.2 (an undeclared field's name, `{1: value, 2: name}`), §19.3 (the class side). The vectors are unchanged.
    * **Docs:** `docs/DB.md` §19 (CRDT tables) and the `unsupported` error.
    * **Tests:**
      * `crdt.test.ts` (15). The second installation is `w.client.signInDevice(w.admin)` plus `unlock({device})`. `commitsDuring` records and injects `/v1/commit` errors.
      * `vectors.test.ts` +1 (tokens, the sealed values, a merged ObjState).
      * 231 vitest tests in all.

  * **Replanned after 7a (merged, #21):** the rest of step 7 is 7b (the indexer, indexes on CRDT tables) and 7c (the offline queue, read modes with a thin `/v1/crdt/versions` check, the property test), a PR each. See Step 7.
  * **7b** (the indexer, indexes on CRDT tables): done, PR #22. As planned, with these details for 7c:
    * **Files:**
      * `ixer.ts`: `reindex(tx, t, pk, pkEl, {only, row})` and `startIndexers(db, tables)` (one `$index` group per distinct change topic, started in `Db.open` after the migrations; `DbOptions.indexer: false` turns it off).
      * `migrate.ts`: `createIndex` on a CRDT table needs `changes` and refuses `unique`; `backfillCrdt` pages `crdtRange` at the transaction's read version; `dropCrdtPage` also deletes `$ixrows` rows (`dropIxRows`, one `getEntries`), then clears the index ranges.
      * `query.ts`: CRDT tables go through the planner; `fetchRows` fetches index candidates (`crdtRows` for CRDT tables); `crdtScan` runs only when no index fits.
    * **`$ixrows` `values`** is keyed by the index id in hex text: CBOR map keys are text or unsigned integers (§1).
    * **⚠️ Spec:** §19.6 (the indexer, wrong types left out, the backfill rule, freshness), §11.5 (`$ixrows` values), §8.3 (dropping a CRDT table clears `$ixrows` and its index entries).
    * **Docs:** `docs/DB.md` §19 "Indexes on CRDT tables".
    * **Tests:** `crdtindex.test.ts` (9): refusals, fast/private/sealed queries, merged counters from two installations, lag and re-check, resume after a stop, deletes and wrong types, backfills (also during writes with the indexer on), drops. The `unsupported` createIndex case left `crdt.test.ts`.
    * PR #22 merged, CI green (commit `96672cb`).
  * **7c** (offline queue, read modes, property test): done, PR #23. **Step 7 is done.** As planned, with these details for step 8 on:
    * **Server:** `POST /v1/crdt/versions` (`CrdtVersionsReq` / `CrdtVersions`, empty = no object), test `versions_track_object_changes`.
    * **Client:** `Transaction.commitId` (fixed per transaction); `commitTransaction(tx, {attempts})`.
    * **Files:**
      * `crdtcache.ts`: `CrdtCache` (LRU of ObjStates, `markStale` with a generation so a fetch in flight stays stale), `crdtStates(db, objects, mode, rv, known)`, `crdtVersions`, `cacheStates`, `isNetwork`. `known(h)` lets a miss offline read as absent (rows only the queue writes; `remove`).
      * `crdtq.ts`: `CrdtQueue` (`db.crdt`: `pending`, `online`, `flush`, `onSync`), `QueueEntry` / `QueueRow` / `QueueStore` / `memoryQueueStore`. Sender: `pack` (limits, the add-then-remove cut), `build` (a `db.begin()` transaction, runs of singles coalesced, `CrdtWrites.load`), resend with the same commit id, `restamp` on `stale_op`, `done` learns `set_dots`.
      * `crdt.ts`: `Pending` `row`/`lww` carry an optional `hlc` (queue time); `stamp` keeps it, adds learned dots to `rem`, marks objects stale on commit and on error.
      * `txn.ts`: `DbTransaction.crdtRead`, `queueAs`, `direct`, `queued`; `finish` queues a CRDT-only transaction (`crdtOnly()`), and flushes the queue before a direct one with CRDT operations; offline, a read-only transaction that used only catalog records skips its check. `tx.after` runs once queued.
      * `query.ts`: `query().read(mode)`; `cacheScan` (path `cache`, paged by pk); `auto` falls back to it offline.
      * `db.ts`: options `crdtCache`, `crdtCacheMs`, `crdtRead`, `crdtQueue` (boolean, default true), `crdtStore` (the `QueueStore`; renamed from the plan's `crdtQueue`), `crdtBatchMs`; `db.table(name, {wait})`; `db.transaction(fn, {queue, wait, read})`.
    * **Decisions beyond the plan:** `DbOptions.crdtQueue: false` commits CRDT-only transactions directly (the 7a/7b tests use it with `crdtRead: 'server'`); a mixed transaction that flushes the queue may re-run once (its snapshot moved on).
    * **Step 8 must:** give `QueueStore` a persistent implementation (and keep the built commit, so a restart resends the same `commit_id`); run the sender in the owner once the store is shared between tabs.
    * **⚠️ Spec:** api.md §13.3 (`versions`), zendb.md §19.4 (cache, read modes), §19.7 (rewritten: entries, stamping, batches, sending, store, API), §16, the `offline` error.
    * **Docs:** `docs/DB.md` §19 "Writes and the queue", "Read modes", the `offline` error; `docs/CLIENT.md` `commitId`.
    * **Tests:** `crdtoffline.test.ts` (15: read modes, the queue, a lost response, a rebase past a 1 s horizon, mixed flushes, a dropped table, learned dots, the 3-installation property test with three seeds); `kv.test.ts` +1; `tests/crdt.rs` +1. 256 vitest tests in all.
    * PR #23 merged.
  * **Replanned after 7c (merged, #23):** step 7 gains 7d (a sealed local store, the persisted queue) and 7e (persisted caches, offline open), a PR each. See Step 7, 7d and 7e.
  * **7d** (sealed local store, persisted queue): done, PR #24 (merged; the plan commit restarted the branch from main). As planned, with these details for 7e:
    * **Rust:** seal kind 8 (`Kind::Local`, label `zen/v1/aad/local`), zen-core `db::seal_local` / `open_local`, wasm `FsKeys.sealLocal` / `openLocal`, the `local` vectors section.
    * **`@zen/client` `src/local/`:**
      * `store.ts`: `LocalStore` (`get`, `put`, `delete`, `list`, `clear`, `deviceKey`, `close`) and `memoryLocalStore`.
      * `idb.ts`: `indexedDbLocalStore(name, {factory, keyRange})`.
      * `node.ts`: `nodeDiskLocalStore(dir)`, exported from `@zen/client/node` (new `src/node.ts`, a package export, a vitest alias and a db tsconfig path).
      * `sealed.ts`: `sealedStore(raw, {fs, device, plaintext, warn})` → `SealedStore` (`raw`, `layers`). The fs layer seals with `fs.keys` and opens with `fs.keysFor`.
      * **7e puts the caches on the same `SealedStore`**, in stores named per database (`localStores(ns)` in `localq.ts` lists them for `Db.wipe`).
    * **`@zen/db`:**
      * `localq.ts`: `localQueueStore(sealed, ns)` (store `queue/<ns>`: `e<id>` entries and `inflight`), `localStores(ns)`.
      * `crdtq.ts`: `Inflight`; `QueueStore.putInflight/loadInflight/removeInflight` (the memory store has them too); the loop stores the commit before its first send and `resend`s a stored one first; owner-only sending (`start()` on `onOwner`) with `queued` / `flush` / `sync` messages on `zen/db/<fs>/<ns>/queue`.
      * `crdt.ts`: `actorLock(db)` (the in-tab chain plus the Web Lock `zen/actor/<actor hex>`), `webLock`, `forgetTotals`.
      * `db.ts`: `DbOptions.local`, `db.locks` (internal), `Db.wipe(local, ns)`. The queue is shared (owner-only) when Web Locks exist and the store is `local` or `crdtStore`.
    * **Decisions beyond the plan:**
      * **The counter lock across tabs** doesn't track the last holder over the channel, as planned: that message can arrive after the next tab already holds the lock. A tab with Web Locks forgets its totals on taking the lock and reads them again (one `crdt/get` per counter commit).
      * **The queue builds its commits in long mode** (catalog reads become `expect`s; no read version). A commit with a read version, sent again after more than the read window, gets `too_old` before the server checks its `commit_id`, so one that had applied would be built again. This also fixes 7c's resend after a long time offline.
      * **The counter lock is taken at load** when a commit is in flight, before any other commit can stamp.
      * **IndexedDB** keeps every store in one object store keyed by `[store, hex(key)]`: adding a store would otherwise need a version change, which waits for every other tab to close.
      * **Failures the owner reports** reach other tabs as `ZenError(0, code, message)`.
    * **⚠️ Spec:** formats.md §4 (kind 8), labels.md, zendb.md §9.3 (rewritten), §19.3 (the counter lock named), §19.7 (the commit in flight, totals never stored, tabs, the lock across tabs), §17.
    * **Docs:** `docs/CLIENT.md` §10 (the local store; security notes are §11 now), `docs/DB.md` §19 "A queue that survives restarts", TECH_DEBT (TD-DB-PERSISTENT-CACHE stays open for 7e).
    * **Tests:**
      * `packages/client/test/local.test.ts` (19): each backend, the device key (one per backend; a 0600 file in Node), each layer combination, settings that don't mix, tampering and moved records, another fs, an app's key, plaintext on disk.
      * `vectors.test.ts` +1 (local records).
      * `crdtpersist.test.ts` (10): a restart, a crash after a lost reply (the same `commit_id`, counted once), a crash before any send, three layer settings, another fs, `Db.wipe`, owner-only sending with handover, and two tabs of one installation incrementing at once. Both crash and counter tests were checked to fail without the change.
      * New dev dependency `fake-indexeddb` 6.2.5.
  * **7e** (persisted caches, offline open): done, PR #25 (merged). **Step 7 is done.** Step 8 starts from main: restart the branch, plan it, open a new PR. As planned, with these details for step 8 on:
    * **Client:** `Fs.header({local})` / `Fs.unlock(how, {local})` keep the header in store `fs/<fs id>` (key `header`: `u32 len ‖ header ‖ version`) and fall back to it on `network`; a store with the fs layer is refused; `Fs.forget(local)`; `UnlockedFs.local` refreshes the kept copy in `editHeader`. `HeaderOptions` is exported.
    * **`@zen/db`:**
      * `localcache.ts`: `LocalCache` (write-behind per store: `put`, `delete`, `clear`, `get` with pending writes first, `list`, `flush`) and the record codecs (rows, CRDT entries, tables, sealed blobs).
      * `db.ts`: `DbOptions.persist`, `db.offline`, `db.flushLocal()`; `close()` now returns a promise that resolves once the caches are written. `open` falls back to the kept state on a `network` error anywhere in the online open (`/v1/info` goes through the client, not `session.call`). `openKept`, `keepOpen`, `loadLocal`, `keptNode` (checked with `IndexKeys.nodeId`), `keptSealed` (checked with `partsDigest`).
      * `crdtcache.ts`: `CacheSink` (`sink.put` / `delete`), `restore`, `Entry` exported.
      * `index/private.ts` reads kept nodes before the server; `index/sealed.ts` `readSealed` takes an async `cached`.
      * `txn.ts`: in read mode `cache`, an offline read-only `finish` accepts its cached rows, and a row miss offline is `offline`. `Table.get(pk, {read})` passes the mode to its transaction.
      * `localStores(ns)` lists `queue`, `db`, `rows`, `crdt`, `nodes`, `sealed`.
    * **Decisions beyond the plan:**
      * Offline, a default (strict) read of a cached transactional row fails with the `network` error, not `offline`.
      * Kept rows are loaded in no particular order (no recency is stored); the excess over `rowCache` is deleted.
    * **⚠️ Spec:** zendb.md §3.1 (opening offline), §7.7 (kept caches, `read: 'cache'`), §9.3 (the stores table, writes behind, checks on use, the fs header), §19.4.
    * **Docs:** `docs/CLIENT.md` §4 (unlocking offline), `docs/DB.md` §7 (kept caches, offline), TECH_DEBT: TD-DB-PERSISTENT-CACHE resolved.
    * **Tests:** `packages/client/test/unlock.test.ts` (5), `packages/db/test/dbpersist.test.ts` (9: a cold start offline, a due migration offline, nothing kept, a warm start (checked to fail without kept nodes and blobs), a stale kept row, CRDT `auto` after a restart, tampering, `persist: false`, wipe and forget). 300 vitest tests in all.
  * **Replanned after 7e (merged, #25): four features added** (see "Added features: milestones 8–11"). They are placeholder milestones 8–11, one doc each. What is central moves forward into step 7, ahead of step 8:
    * **7f:** the large-file layout on the Rust side (spec, zen-core, server, vectors).
    * **7g:** the large-file layout in the client and zen-mount.
    * **7h:** a credential vault on the sealed local store.
    * Step 8 also takes on design constraints from the new milestones.
    * **User decisions:**
      * two-level pages (not a hash chain or a tree)
      * per-file content keys folded into the same format change
      * the vault as 7h
    * The plan commit restarts the branch from main and opens a new PR (#26).
  * **7f** planned in detail (see "7f: large files, the format and the server"); PR #26 (the plan) merged.
  * **7f** (paged versions and file keys, the Rust side): done, PR #27. As planned, with these details for 7g:
    * **zen-core `fs.rs`:**
      * `Manifest` (v1) is unchanged. Version 2 is a separate `ManifestV2 {size, chunk_size, n, key_epoch, file_key, body: Body::Inline | Body::Paged {page_size, pages: [(id, digest)]}}`, plus `AnyManifest::{V1, V2}`, `seal_any_manifest` and `open_any_manifest`. This replaces the planned `Manifest` enum, so callers of v1 didn't change.
      * `encode_page`, `decode_page`, `page_digest`, `page_capacity(max_value_bytes)` (5,624 at the defaults), `new_file_key`, `file_chunk_key`, `seal_file_chunk`/`open_file_chunk(fs_id, file_key, key_epoch, chunk, …)`, `write_paged_bytes`.
      * Labels: `FS_FILE`, `FS_PAGE`, `SIG_APP_BUNDLE` (also an accepted signature purpose, like `SIG_DB_ROOT`).
    * **wasm:**
      * `sealManifest`/`openManifest` stay v1.
      * New: `sealManifestV2(tree, node, ManifestV2)`, `openAnyManifest` → `AnyManifest` (`version` 1 or 2; v2 has `chunks`, or `pageSize` + `pages: [{id, digest}]`), `newFileKey`, `sealFileChunk`/`openFileChunk(fs, fileKey, keyEpoch, chunkId, …)`, `encodePage`, `decodePage`, `pageDigest`, `pageCapacity`, `writePagedOpBytes`.
      * Wire: `PagePut`, `PagesGet`, `PageData`, `Pages`; `Commit.pages`; `CrdtOp.write.pages`; `Version.pages`.
      * 2,135,245 bytes.
    * **Server:**
      * `tree.rs`: `Content::{Chunks, Pages}`, with the paged version marked by `u32 0xFFFFFFFF`. `put_page` (the first upload adds `cr` per chunk; a re-upload only restarts the grace period), `release_content`, `page_gc_candidate`, `collect_pages` (runs before `collect_chunks`; at most about 20,000 chunk releases per transaction), `pages_get`.
      * `commit.rs`: a page counts as one op, plus `data + 48 B per chunk`. A write carrying both chunks and pages gets 400. Pages apply after chunks and before ops.
      * Keys `pg`/`pr`/`pz`/`pc`; the feature `"fs_pages"`; the route `/v1/fs/pages/get`.
    * **Client:** `tree.ts` `versions` throws `unsupported` on a paged version until 7g; `packages/client/test/fsvectors.test.ts` (new).
    * **⚠️ Spec:**
      * formats.md §2, §4, §11.3 (v2), §11.4 (file keys), §11.5 (`write_paged` 0x04), §11.6 (pages)
      * fs.md §4, §4.1, §4.2 (new: file keys), §6
      * keyspace.md §3.6
      * api.md §2, §6 (`pages`, its costs), §12
      * labels.md
    * **Docs:** `docs/STATS.md` §1.3; TECH_DEBT TD-FS-LARGE-FILES progress (resolved in 7g).
    * **Tests:**
      * `crates/zen-core/tests/fs.rs` (6)
      * the `fs.json` v2 keys, in `tests/vectors.rs`
      * `crates/zen-server/tests/fs.rs` +4: a round trip with refusals and the chain; shared pages, GC order, re-upload and quota; purge; a reference-model GC property test over three seeds. Three of them were checked to fail when page collection doesn't release chunks.
      * vitest: 301 tests
    * **CI fixes on #27:**
      * **FoundationDB:** with a grace of 0, the GC tests assumed a single sweep sees the latest commits. On FoundationDB, commit versions can trail the clock the grace is measured with. The tests now use `sweep_until`, which sweeps to the expected state and checks after every sweep that no page outlives a chunk it lists. Two mutations (no chunk release; pages not counting their chunks) were each checked to fail.
      * **A 7d bug** (`crdtpersist` "two tabs … increment": 14, not 16, in about 1 run in 6). Two tabs share one counter actor but each has its own clock, so a tab's `seq` could be no newer than the other tab's last one, and the server ignored it as a re-send. `loadTotals` now observes the `seq` of the entries it reads (after the lock, which already forces the read). zendb.md §19.7 and DB.md were updated.
    * PR #27 merged; CI green on all 8 checks.
  * **7g** planned in detail (see "7g: large files in the client and zen-mount").

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

**Original outline:** `crdt.ts` (`patch`, `incr`, `add`, `remove`, `delete`, the merged view through `/v1/crdt/*`), change-event appends, the `$index` indexer and `$ixrows`, the offline queue with rebase on `stale_op`, `"crdt_rows"` detection. Tests: offline edits merging, counters across devices, add-wins sets, indexer lag and re-check, an indexer restart.

**Context.** Steps 5–6 (PRs #14–#19, all merged) give `@zen/db` its transactional tables and the broker. The server merges CRDT rows since step 3 (`crates/zen-server/src/crdt.rs`, api.md §13), and the client already has what the wire needs:
* `Transaction.addCrdtOps`, `CommitResult.set_dots`
* `wasm`: `DbKeys.crdt(tableId)` → `CrdtKeys {field, elem}`, `FsKeys.sealCrdtValue` / `openCrdtValue`, the `CrdtOp` / `ObjState` / `CrdtGet` / `CrdtRange` wire types
* `TreeClock.tick()` and `actor`, `Tree.rebase` (the pattern for `stale_op` / `clock_skew`)
* `TableRecord.merge` / `crdtFields` (catalog keys 8 and 9), not yet written by `createTable`

Step 7 builds the class side.

**User decisions:**
* **Split:** 7a (CRDT tables online) and 7b (indexer, offline queue, batching, property test), a PR each.
* **Queue unit, by what the call is:**
  * a CRDT-only transaction is queued as **one atomic entry**
  * a single call (`table.patch`, `incr`, …) is a **single-operation entry**
  * the queue **batches** entries into as few commits as the limits allow
* **Resolving:** CRDT writes outside a mixed transaction resolve **once queued** by default; `{wait: true}` waits for the commit.
* **Store:** memory now, behind a `QueueStore` interface that step 8 implements (disk, IndexedDB, sealed at rest).

**After compaction, re-read:**
* this section and the 6a–6c log lines
* `spec/zendb.md` §3.2, §12.7, §16, §19
* `spec/api.md` §13
* `packages/db/src/{txn,db,query,migrate,catalog,keys,broker,system}.ts`
* `packages/client/src/{kv,tree,clock}.ts` (`addCrdtOps`, `rebase`, `tick`, `actor`)
* `crates/zen-wasm/src/db.rs`
* the `crdt` section of `spec/test-vectors/zendb.json`
* `packages/db/test/{db,broker}.test.ts` (setup)

**Branch/PR.** #19 is merged: restart the branch from main, commit this plan (`docs: milestone 5 step 7 plan`), and open a new PR that 7a joins.

### 7a: CRDT tables online

#### Files
1. **`migrate.ts`:** `createTable(name, {pk, merge: 'crdt', fields?: {name: 'lww' | 'counter' | 'set'}, changes?})`.
   * It checks the server's `/v1/info` features for `"crdt_rows"`, and fails with `DbError('unsupported')` (a new code) otherwise.
   * pk fields can't be typed fields; `pad` is refused.
   * `createIndex` on a CRDT table: `unique` is refused (`bad_value`). Every other kind waits for 7b (`unsupported` until then).
2. **`crdt.ts` (new): the codec and the merged view.**
   * **Tokens and objects:** `CrdtKeys` are cached per table in `Db` and freed by `close()`.
     * object = `keys.row(table.id, pkEl)`
     * field = `crdt.field(name)`
     * elem = `crdt.elem(name, pkEl, CBOR(element))`
   * **Plaintexts:** as in §19.2, sealed with `fs.keys.sealCrdtValue(object, field, elem, CBOR)`. **⚠️ Spec addition:** an `lww` field not declared in `fields` carries its name, `{1: value, 2: name}`, since tokens can't be reversed. Declared fields keep `{1: value}`, so the vectors are unchanged.
   * **`mergeState(table, ObjState) → row | undefined`:**
     * open every value
     * the row register `alive` → the pk fields
     * `lww` fields by name, a field without a value is absent
     * counters as the sum of their entries
     * sets as the elements with at least one dot, sorted by their CBOR bytes
     * absent if there is no register or `alive` is false
     * A value that doesn't open → `corrupt`.
   * **`readCrdt(db, objects, rv?)`** batches `/v1/crdt/get` in 1,000s. **`scanCrdt(db, table, {after, limit})`** pages `/v1/crdt/range` over `keys.rows(table.id)`, continuing at `last ‖ 0x00`.
3. **`CrdtTableTx` in `txn.ts` (`tx.table(name)` dispatches on `merge`):**
   * **Writes:**
     * `insert(row)`: a `row` op (`alive: true`, `{1: pk}`), then the fields as below
     * `patch(pk, fields)`: an `lww` op per field (`undefined` unsets it); counter and set fields are refused here (`bad_value`)
     * `incr(pk, field, n)`
     * `add(pk, field, ...elems)` / `remove(pk, field, ...elems)`
     * `delete(pk)`: a `row` op, `alive: false`
   * **Reads:**
     * `get(pk)`: the merged row, read with `crdt/get` at the transaction's read version. It is not in the read set (§19.4).
     * `query()`: a scan over `scanCrdt` with predicates applied client-side, and the scan rules for `orderBy` and `limit` (`needs_index`). Index paths come in 7b.
   * **Refused** (`bad_value`): `put` and `update`, which are read-modify-write.
   * **Stamping.** Ops are stamped when the transaction finishes:
     * `row` / `lww`: `clock.tick()` as the `hlc`
     * `ctr`: `seq` = `clock.tick()`, and `value` = this installation's total
   * **Counter totals.** The `Db` keeps the base per `(object, field)`, read once from this actor's entry with `crdt/get`. The total is base + this attempt's increments, and `onCommit` stores the new base, so a re-run never double-counts.
   * **`remove`** reads the element's current dots (`crdt/get`) and sends `rem` with them: add-wins.
   * **Ops on one row** are kept in call order. An `insert` with fields puts its `row` op first.
   * **Rebase.** An `onError` hook (like `Tree.writeIn`): on `stale_op` or `clock_skew`, `clock.sync(force)`, then re-run the transaction, which restamps. At most 3 times.
   * **Limits:** the ops count in `checkLimits` (`max_commit_ops`, `max_commit_bytes`, `max_value_bytes`).
4. **Change events (§12.7):**
   * A CRDT table with `changes` appends one `zen.change` per touched row per transaction: `{op: 'put' | 'delete', table, pk}`, keyed by the pk element.
   * There is no `row` image: the merged state is known only to the server. `image: 'full'` is refused for CRDT tables.
   * They reuse `noteChange` / `emitChanges` with an `op`-only record.
5. **`Table` (outside a transaction):** `patch`, `incr`, `add`, `remove`, `delete` and `insert` are a transaction each. In 7a they always wait for the commit (the queue and `{wait}` come in 7b). `get` and `query` read.
6. **Spec (⚠️ `spec:` commit):**
   * §3.2: `merge: "crdt"` tables refuse `pad`, `unique` and `image: "full"`.
   * §12.7: CRDT change events carry no row.
   * §19.2: the `lww` name rule.
   * §19.3 (class side):
     * stamping at commit build
     * counter totals per actor from the base plus increments
     * `remove` sends the observed dots
     * `insert` vs `patch`: only `row` ops resurrect
   * §16: the example's `incr` / `add` signatures as built.
   * Error code `unsupported`.
7. **Docs:** a `docs/DB.md` section on CRDT tables (what is given up, the field types, reads, change events). The offline part is added in 7b.

#### Tests 7a (`packages/db/test/crdt.test.ts`, its own server)
* **Vectors:**
  * field and element tokens from the `crdt` section
  * opening each sealed value (row, lww, counter, set)
  * `mergeState` of a hand-built ObjState equals the expected view
* **Rows:**
  * insert → get, and patch
  * `delete` hides the row; a later `patch` doesn't resurrect it, but `insert` does
* **Concurrency (two `Db`s with separate sessions and clocks):**
  * different fields both survive; one field: the later `hlc` wins
  * counters: two installations' `incr`s sum; a conflict re-run doesn't double-count
  * sets: a concurrent `add` survives a `remove` that didn't observe it (add-wins); a `remove` of observed dots removes
* **Transactions:**
  * a transaction with a transactional row, a CRDT op and an emit is atomic: a throw leaves none of them
  * CRDT reads don't conflict: a concurrent CRDT write doesn't re-run a transaction that read the row
* **Rebase:** a skewed clock (as in `tree.test.ts`) → `stale_op` / `clock_skew` → rebased and committed.
* **Change events:** one per touched row per transaction, keyed by pk, `table.on` sees them.
* **Errors:**
  * `unsupported` against an info without `"crdt_rows"` (a wrapped `client.info`)
  * `unique` and `put` refused
* **Scan query:** paging with cursors, predicates, `needs_index`.

### 7b: the indexer and indexes on CRDT tables (detailed plan)

**Context.** 7a (PR #21, merged) gives CRDT tables online: writes, merged reads, scans and change events. Indexes on them are refused (`unsupported`). 7b adds them (zendb.md §19.6). Writers can't maintain these indexes, because the merged value exists only on the server. Instead an **indexer** consumer reads each changed row's merged state and updates the indexes from the values it last indexed (`$ixrows`).

**User decisions (step 7 replanned after 7a):**
* **Split:** the rest of step 7 becomes 7b (the indexer) and 7c (the offline queue, read modes, the property test), a PR each.
* **Queueing:** a CRDT-only `db.transaction` queues by default as one atomic entry; `{queue: false}` commits it directly.
* **Reads (7c):** three modes:
  * **default:** an expiring cache. Once an entry has expired, the read asks the server with a thin version check whether the row changed.
  * `'cache'`: offline first, the cached state regardless of age.
  * `'server'`: online only, always fresh.

**After compaction, re-read:**
* this section and the 7a log line
* `spec/zendb.md` §5.5, §8.2, §11.5, §19.4, §19.6
* `packages/db/src/{crdt,txn,query,migrate,db,system,broker}.ts` and `index/{private,fast,sealed}.ts`
* `packages/db/test/{crdt.test.ts,helpers.ts}`

**Branch/PR.** #21 is merged: restart the branch from main, commit this plan (`docs: milestone 5 step 7b plan`), and open a new PR that 7b joins.

#### Files
1. **`ixer.ts` (new): the indexer (§19.6).**
   * **`reindex(tx, t, pk, pkEl, only?: IndexDef)`:**
     1. Read the merged row (`TableTx.crdtRows`).
     2. Read its `$ixrows` row `[t.id, pkEl]`, through `tx.table('$ixrows')`, so it is in the read set.
     3. For each maintained index (or only `only`): `oldV = values[ix.id]`, and `newV = indexedValue(ix, row)` when the row exists.
        * A value of the wrong type can't be refused (the write has happened), so that row is **left out of that index** (`newV` undefined), with a warning.
        * When `oldV` and `newV` differ (`same`), `changeIndex(tx, t, ix, pk, pkEl, oldV, newV)`.
     4. Write `$ixrows`, `{table_id, pk, values: Map<index id bytes, Value[]>}`. It keeps only the maintained indexes, which drops dropped ones lazily, and is deleted when the row is gone or indexed nowhere.
   * **`startIndexers(db)`:** for every CRDT table with a maintained index, a `db.consume({group: '$index', topic: change topic, mode: 'per_key', maxInflight: 8, start: 'earliest'})`. Each event `{table, pk}` runs `reindex` in the handler's transaction; an unknown or dropped table is acknowledged and skipped.
     * It runs in the owner (`onOwner`, as consumers do), started by `Db.open` after the migrations, and stopped by `close()`.
     * `DbOptions.indexer?: boolean` (default true): tests turn it off to make lag.
     * An index created later, while this instance is open, is picked up at the next open. Migrations run at open, so that is the normal path; it is documented.
   * Events from before the index existed are handled too (`earliest`): that is harmless, since `reindex` is idempotent.
2. **`migrate.ts`:**
   * **`createIndex` on a CRDT table:**
     * It requires `changes` (`bad_value`).
     * It refuses `unique`, and so `kind: 'none'` (`bad_value`), as well as `oblivious` (`format`, as now).
     * The `unsupported` refusal of 7a goes.
   * **Backfill:** pages of `crdtRange` from `builtTo` (`pageSize`). For each live object, `reindex(tx, t, pk, pkEl, ix)` for the building index, unless its `$ixrows` row already holds a value for that index (the indexer got there first and keeps it current). A concurrent indexer commit on the same `$ixrows` row conflicts, and the page re-runs. `builtTo` is the last object, and `active` comes when `more` is false.
   * **`dropIndex`:** the same paged clears as for transactional tables. Stale `$ixrows` values are dropped lazily (above).
   * **`dropTable` (CRDT):** each page also deletes the `$ixrows` rows of its objects, and the index ranges are cleared after the rows.
3. **`query.ts`:**
   * A CRDT table goes through the same planner (pk, fast equality, private or sealed range, scan); `crdtScan` remains the scan.
   * The index paths fetch their candidate rows through one fetch, by kind of table:
     * transactional: `getEntries` + `decode`, as now
     * CRDT: `crdtRows` (batched `crdt/get`, the transaction's own operations overlaid)
   * Every predicate is re-checked on the merged row, as now (`keep`). So an entry that is stale or not yet indexed never returns a wrong row.
   * Order and cursors follow the index. A row whose indexed value changed after it was last indexed appears at its old place until reindexed (documented).
4. **`txn.ts`:** nothing new for writers. CRDT writes never call `changeIndex`, since only the indexer maintains these indexes.
5. **Spec (⚠️ `spec:` commit):**
   * **§19.6:**
     * `$ixrows` values are a map from index id to the indexed value.
     * A value of the wrong type leaves the row out of that index.
     * The group starts at `earliest`, with `maxInflight` 8.
     * The backfill rule (rows already in `$ixrows` for the index are skipped).
     * `createIndex` requires `changes`.
     * Results follow the index order of the last indexing.
   * **§11.5:** the `$ixrows` row's fields.
   * **§8.3:** dropping a CRDT table also deletes its `$ixrows` rows.
6. **Docs:** `docs/DB.md` §19 gains "Indexes on CRDT tables": the lag, re-checks, ordering, `DbOptions.indexer`, and how to wait for the indexer in tests.

#### Tests (`packages/db/test/crdtindex.test.ts`, its own server)
* **Refusals:**
  * `createIndex` without `changes` → `bad_value`
  * `unique` / `kind: 'none'` → `bad_value`
* **Indexes:** fast equality, a private range with `orderBy` both ways and paging, a sealed range, all after the indexer caught up (`until`).
* **Lag and re-check:**
  * A writer Db with `indexer: false` patches rows. An old-value query returns nothing (re-checked), and a new-value query misses the row.
  * Then another Db with the indexer starts, and the queries are right.
* **Restart:** the indexer Db closes mid-stream and more writes come. A new one resumes; the tree equals `expectCanonical` of the merged rows, and `$ixrows` matches.
* **Deletes and types:**
  * `delete` removes the entries.
  * A patch to a value of the wrong type takes the row out of that index, with a warning.
* **Backfill:**
  * Rows exist, then a migration creates a private and a sealed index with `pageSize: 2`: canonical tree and list.
  * Writes during the backfill, with the indexer on, still end canonical.
* **Drop:** `dropIndex` clears its keys, and `dropTable` clears `$ixrows` and the index ranges.
* **Verification:**
  * `npx biome check`
  * tsc: client, fuse, db test
  * `npx vitest run` three times
  * CI green on the new PR before stopping

### 7c: the offline queue, read modes, the property test (detailed plan)

**Context.** 7a and 7b (PRs #21, #22, merged; CI green) give CRDT tables online, with indexes kept by the indexer. Every CRDT read still goes to the server (`crdt/get` / `crdt/range`), and every write is a commit sent at once, so nothing works offline. 7c finishes step 7 (zendb.md §19.4, §19.7, §19.9):
* **read modes** with a per-`Db` cache of merged states, checked by a new thin endpoint
* **the offline queue** for CRDT writes
* **the 3-device property test**

**User decisions (step 7 replanning):**
* **Queueing:** a CRDT-only `db.transaction` is queued by default as one atomic entry; `{queue: false}` commits it directly. A `Table` call (`patch`, `incr`, …) is a single-op entry. The queue batches entries into as few commits as the limits allow.
* **Resolving:** writes resolve once queued; `{wait: true}` waits for the commit.
* **Store:** memory, behind a `QueueStore` interface (persisted stores come in step 8).
* **Reads:** three modes:
  * **default `auto`:** an expiring cache; an expired entry costs a thin version check (a new endpoint is fine)
  * **`cache`:** offline first, the cached state regardless of age
  * **`server`:** online only, always fresh

**After compaction, re-read:**
* this section and the 7a/7b log lines
* `spec/zendb.md` §19.3, §19.4, §19.7, §19.9; `spec/api.md` §6 (idempotency), §13.3
* `packages/db/src/{crdt,txn,db,query,ixer}.ts`
* `packages/client/src/kv.ts` (`sendCommit`, `Transaction.build`, `commitTransaction`)
* `crates/zen-proto/src/lib.rs` (`CrdtGet` ~2209, `ObjState` ~2359, `ObjStates` ~2390)
* `crates/zen-server/src/crdt.rs` (`get` ~490, `load` ~395) and `lib.rs` (routes ~282)
* `crates/zen-wasm/src/wire.rs` (CRDT lines ~103)
* `crates/zen-server/tests/crdt.rs` (`get` helper ~135)
* `packages/db/test/{crdt,crdtindex}.test.ts` (setup, `installation`, `commitsDuring`)

**Branch/PR.** #22 is merged: restart the branch from main, commit this plan (`docs: milestone 5 step 7c plan`), and open a new PR that 7c joins.

#### Server: `POST /v1/crdt/versions` (⚠️ api.md §13.3)
* **zen-proto:** `CrdtVersionsReq {fs, objects: [bytes], read_version?}` and `CrdtVersions {read_version, versions: [bytes]}`, where an **empty** entry means no such object. zen-proto has no `Vec<Option<_>>` precedent, and the names `Version`/`Versions` are taken by fs file versions. Same derives and `ts` attributes as `CrdtGet`.
* **zen-server `crdt.rs`:** `versions` handler:
  * `check_read`, at most 1,000 objects, `check_object` on each
  * one `snapshot_get` of the `cv` key per object (as `load` does), no conflict ranges
  * route `/v1/crdt/versions` in `lib.rs`
  * It needs no new feature string: it ships with `"crdt_rows"`, which this milestone introduces.
* **zen-wasm:** `encodeCrdtVersionsReq` / `decodeCrdtVersions` in `wire.rs`.
* **Test (`tests/crdt.rs`):** versions equal `get`'s `version`s, change after a write, are empty for an unknown object; the 1,000 limit and the read right. Both backends.

#### Client: `Transaction.commitId` (`packages/client/src/kv.ts`)
`build()` uses a `commitId` fixed when the `Transaction` is made, instead of a fresh one per build. Each `transaction()` attempt already makes a new `Transaction`, so nothing changes there. The queue can then send one built transaction again after a network error, with the same `commit_id` (api.md §6 idempotency, kept 24 h). Test in `kv.test.ts`: two `commitTransaction` calls on one transaction apply once.

#### `@zen/db`
1. **`crdtcache.ts`: read modes (§19.4).**
   * **The cache:** an LRU per `Db` of `object hex → {state: ObjState, checked: ms}`. `DbOptions.crdtCache` sets its size (default 1,024; 0 turns it off), and `DbOptions.crdtCacheMs` how long an entry is fresh (default 5,000).
   * **`crdtStates(db, objects, mode, rv?)`** replaces the direct `crdtGet` calls of reads (`TableTx.crdtRows`, `remove`'s dots, the backfill keeps `crdtGet`):
     * **`auto`:** fresh entries as they are. For the others, one `/v1/crdt/versions` call; equal versions are marked checked, and only changed or uncached objects are fetched with `crdt/get`. On a network error (`ZenError` `network`), cached entries are served however old, and a miss throws `DbError('offline')`, a new code.
     * **`cache`:** cached entries regardless of age; misses go to the server (`crdt/get`), and offline a miss throws `offline`.
     * **`server`:** always `crdt/get` (at the transaction's read version), filling the cache; offline it fails.
   * **Filling:** every `crdt/get` and `crdt/range` result (scans included) refreshes the cache. A commit with CRDT ops marks its objects stale, so the next `auto` read checks them.
   * **API:** `get(pk, {read})`, `query().read(mode)`, `DbOptions.crdtRead` (default `auto`). Transactions take `db.transaction(fn, {read})` as the default for their CRDT reads.
   * **Queries:**
     * `auto` and `server` run as now (index paths fetch their rows through `crdtStates`, so index fetches use the cache).
     * In `auto`, a network error falls back to the `cache` answer.
     * **`cache`:** the table's cached rows plus the rows that exist only in the queue. Predicates, `orderBy` and `limit` are applied in memory; paging by pk (path `cache`). Documented as possibly incomplete.
2. **`crdtq.ts`: the queue (§19.7).**
   * **Entry:** `{id, single: boolean, rows: [{table, tableId, pk, ops: QOp[]}], changes, at}`.
     * `QOp` is a `Pending` with its stamp: `row` / `lww` carry the `hlc` ticked **at queue time** (the edit's time, so offline edits keep their place in the order; no clock sync needed offline).
     * `ctr` keeps its delta; `seq` and the total come at send time.
     * `rem` keeps the dots seen at queue time (from the cache in any mode, so it works offline).
     * `changes` are the change notes (`{table, pk, alive}`); the events are appended at send time, in the same commit as the ops (§19.7).
   * **Queuing a transaction** (`DbTransaction.finish`):
     * **CRDT-only** means its built commit has no writes, clears, appends, consumes, chunks, `read_conflicts` or `expect_ranges`, and every `expect` is a catalog record it read. Its emits, schedules, files or KV reads make it mixed.
     * A CRDT-only transaction with `queue !== false` gives its `CrdtWrites` and change notes to `db.crdtQueue.add(entry)`. Its commit is then empty, and the read-only catalog check is skipped (the catalog is checked again at send time). So it completes offline.
     * `Table.patch` / `incr` / `add` / `remove` / `delete` / `insert` on a CRDT table are one-op transactions: entries with `single: true`.
     * **Mixed transactions** commit directly as now. Before they commit, the queue is flushed (`await flush()`), so this device's commits stay in order. Offline, that flush throws and the transaction fails, as it would anyway.
   * **Overlay:** every read overlays the queued operations of its rows (in queue order), then the transaction's own (`overlay`). A row inserted only in the queue shows in `get` and in `cache` queries.
   * **Sending** (one sender per `Db`, after `batchMs`, default 10):
     * **Batch:** entries in order, packed until the next would pass `max_commit_ops` / `max_commit_bytes` (`checkLimits`). Single-op entries are coalesced per row and field: the last `lww` wins, `ctr` deltas add up, an `add` then a `rem` of one element cancel out. Atomic entries are never split or merged into others' coalescing.
     * **Build:** a `ManualTransaction`. Each row's TableRecord is read by name and must have the entry's table id; otherwise the table was dropped or replaced, and its ops are dropped with a warning. The ops go through `CrdtWrites` with their stamps: `row` / `lww` keep their queued `hlc`; `ctr` takes the actor lock, `seq` = a tick and the total; `rem` adds the `set_dots` the queue learned from earlier queued `add`s of that element. Then the change events.
     * **Send:** `commitTransaction` on the same transaction until it succeeds or fails for sure.
       * `network` / `commit_unknown` → resend the same commit (same `commit_id`) after a backoff from 100 ms to 30 s (also woken by `online` events and by `flush()`). The actor lock stays held meanwhile, so no other counter commit can overtake it.
       * `conflict` (the catalog changed) → rebuild the batch.
       * `stale_op` / `clock_skew` → `rebase` (clock sync), then rebuild with fresh `hlc`s for its `row` / `lww` ops: the offline edit then wins over edits made since, as documented (§19.7).
       * `too_large` → split the batch; a single entry that is too large fails.
       * **Any other error** fails the batch's entries: their `wait` promises reject, `onError` reports them, and they leave the queue.
     * **On success:** `set_dots` are kept for later `rem`s; the entries are removed from the store; their objects are marked stale in the cache; the `wait` promises resolve.
   * **API:**
     * **`db.crdt`:** `pending` (entries), `online`, `flush()` (resolves when the queue is empty; offline it rejects), `onSync(fn)` (after each sent batch, and when going offline or online).
     * **`db.transaction(fn, {queue, wait})`;** `db.table(name, {wait: true})` gives a handle whose CRDT calls wait for the commit (variadic `add`/`remove` leave no room for an options argument).
   * **Store:** `QueueStore {load(): Promise<Entry[]>, put(e): Promise<void>, remove(ids): Promise<void>}`, with `memoryQueueStore()` as the default. `DbOptions.crdtQueue` takes another. An entry is in the store before its write resolves. The built commit isn't stored, so after a restart a commit whose outcome was unknown is rebuilt (new `commit_id`); step 8's persisted stores add that (noted in TECH_DEBT if not done there).
   * **Close:** `db.close()` stops the sender; entries stay in the store.
3. **`txn.ts`, `db.ts`, `query.ts`:** the hooks above (`finish` queues, `crdtRows` via `crdtStates` with the queue overlay, `remove` dots from the cache, mixed commits flush first). `DbError` gains `offline`.
4. **Spec (⚠️ `spec:` commit):**
   * **api.md §13.3:** `/v1/crdt/versions`.
   * **zendb.md §19.4:** the read modes and the cache.
   * **zendb.md §19.7:** queued entries, stamping (`row` / `lww` at queue time, counters at send time, `rem` dots and `set_dots`), batching and coalescing, the same `commit_id` on resends, rebuild rules, the flush before mixed transactions, `QueueStore`.
   * **§16:** the example's `{wait}`.
   * The new error code `offline`.
5. **Docs:** `docs/DB.md` §19 gains "Offline" (queueing, `wait`, `db.crdt`, what fails offline) and "Read modes". `docs/CLIENT.md`: `commitId`.

#### Tests
* **Server** (`tests/crdt.rs`): above.
* **`packages/db/test/crdtoffline.test.ts`** (its own server; `world({limits: {crdt_horizon_secs: 1, claim_ttl_ms: 1000}})`). Offline is simulated by wrapping `session.call` to throw `ZenError('network')` while a flag is set, as `commitsDuring` does.
  * **Read modes:**
    * `auto`: a fresh hit costs no call. An expired entry costs one `versions` call and no `get` when unchanged, and a `get` when another installation changed it. Offline, the cached row is served, and an uncached one throws `offline`.
    * `cache`: an old entry is served without a call.
    * `server`: always a `get`; offline it fails.
    * Queries: `cache` answers from cached and queued rows; `auto` falls back offline.
  * **Queue:**
    * Writes resolve while offline, are visible locally at once (`get`, `cache` queries), and arrive after going online (`flush()`); `{wait: true}` resolves only then.
    * **Two installations edit offline, then reconnect:** different fields both survive; counters sum; add-wins sets.
    * **Batching:** 50 single-op `incr`s → one or two commits (counted) with the right total. Coalescing: patches of one field → one op.
    * **Atomic entries are kept whole:** a transaction of 3 rows, with a low `max_commit_ops` that cuts the batch, still lands in one commit.
    * **A lost response** (the commit applied, then a network error is injected on the reply): the resend with the same `commit_id` applies once (counter total right).
    * **`stale_op` past the horizon:** an offline `patch` older than 1 s → rebased on send, and it wins over an online edit made meanwhile.
    * A mixed transaction flushes the queue first; `{queue: false}` commits directly; a dropped table's queued ops are dropped with a warning.
    * A remove offline of an element added offline earlier in the queue removes it (`set_dots`).
  * **Property test (§19.9):**
    * Three installations (three sessions and clocks) on one database.
    * Seeded xorshift, three seeds. Each device runs random ops (insert, delete, patch, incr, add, remove) on a few rows, in random offline/online phases, with random reconnect order.
    * Afterwards all three `flush()`; `server` reads on every device must be equal for every row.
    * Counters must equal the sum of every device's increments.
    * Sets must hold every element some device added and never removed.
* **Verification:**
  * Rust: `cargo fmt --check`, clippy (default and `--no-default-features`), `cargo test -p zen-server --test crdt`, `scripts/build-wasm.sh`
  * JS: `npx biome check`; tsc for client, fuse and db test; `npx vitest run` three times
  * CI green on the PR before stopping
* **Disk:** `CARGO_INCREMENTAL=0`; check disk before the Rust build (6.9 GB free).

### 7d: the sealed local store and the persisted queue (detailed plan)

**Context.** 7c (PR #23) gives CRDT tables an offline queue and read modes, but everything lives in memory: a restart loses queued writes, and a commit whose outcome was unknown is rebuilt with a new `commit_id`, so it may apply twice. zendb.md §9.3 says a persistent cache is "encrypted at rest" and that milestone 5 decides; TD-DB-PERSISTENT-CACHE is open. 7d builds the **sealed local store**, shared with step 8's replica, and puts the **queue** on it. 7e then persists the caches and opens offline.

**User decisions (after 7c):**
* **Scope:** persist the queue (7d), then the caches (7e): CRDT states, rows and catalog, index nodes and sealed blobs. The store is shared groundwork that step 8's replica reuses for nodes and chunks.
* **Layers, both optional:**
  * **device** (default **on**): AES-GCM under a device-local key
  * **fs** (default **off**): sealed under the fs keys, inside the device layer
* **Offline open (7e):** `unlock` and `Db.open` work offline from stored state.
* **Split:** 7d + 7e, a PR each.

**After compaction, re-read:**
* this section and the 7c log line
* `spec/zendb.md` §9.2, §9.3, §19.7; `spec/formats.md` §4
* `packages/db/src/{crdtq,crdt,db,tabs,proxy}.ts`
* `packages/client/src/{kv,clock,index}.ts`, `packages/client/package.json`
* `crates/zen-core/src/db/mod.rs` (`seal_crdt_value`), `crates/zen-wasm/src/db.rs`, `crates/zen-core/src/db_vectors.rs` (`crdt_vectors`)
* `packages/db/test/crdtoffline.test.ts` (`net`, `pair`)

**Branch/PR.** #23 is merged: restart the branch from main, commit this plan (`docs: milestone 5 step 7d plan`), and open a new PR that 7d joins.

#### Seal kind 8, local records (⚠️ formats.md §4, Rust)
* **The kind:** label `zen/v1/aad/local`, context `u32(fs) ‖ lp(store) ‖ lp(record key)`, under the KV AEAD key of `key_epoch`, like kind 7. A record moved to another store, key or fs fails authentication.
* **zen-core:** `seal_local(fs_keys, store, key, pt, rng)` and `open_local`, next to `seal_crdt_value`. A `local` section in `spec/test-vectors/zendb.json` (`db_vectors.rs`), from a fixed RNG.
* **zen-wasm:** `FsKeys.sealLocal(store, key, pt)` and `openLocal(store, key, sealed)`.
* `gen_vectors` leaves the other sections unchanged.

#### `@zen/client`: the local store (`packages/client/src/local/`)
1. **`LocalStore`**, the raw backend: `get(store, key)`, `put(store, key, value)`, `delete(store, keys)`, `list(store)` (an async iterator of `[key, value]`), `clear(store?)`, `close()`. `store` is a short name (`queue`, `crdt`, …); keys and values are bytes.
   * **`memoryLocalStore()`**
   * **`indexedDbLocalStore(name)`**: one object store per store name, created on demand (a version bump); a key is the hex of its bytes.
   * **`nodeDiskLocalStore(dir)`**: in the subpath export `@zen/client/node` (new in `package.json`), so browser bundles never import `node:fs`. One directory per store, one file per key (hex name). Writes go to a temporary file, are fsynced, then renamed, so a crash never leaves half a record.
2. **`SealedStore`** (`sealedStore(raw, {fs?, device?})`): the same API over plaintext, which seals on `put` and opens on `get` / `list`.
   * **Record:** `u8 version (1) ‖ u8 layers (bit 0 device, bit 1 fs) ‖ body`.
   * **fs layer** (`fs: UnlockedFs`, default off): `fs.keys.sealLocal(store, key, plaintext)`.
   * **device layer** (default on): AES-256-GCM through WebCrypto, a random 12-byte nonce, AAD = `zen/v1/device ‖ 0x00 ‖ lp(store) ‖ lp(key)`. It wraps the fs layer when both are on.
   * **The device key:**
     * a non-extractable `CryptoKey`
     * `deviceKey(raw)` makes one and keeps it: in IndexedDB the CryptoKey itself (structured clone); in Node a random 32-byte key in `dir/device.key`, mode 0600, imported non-extractable; in memory, fresh per process
     * or the app passes its own (`device: CryptoKey`), or `device: false` turns the layer off
   * **Failures:** a record that doesn't open (tampered, moved, another key) is treated as absent and deleted, with a warning. **No layer at all is the app's choice** (user decision): `{device: false, fs: false}` must come with `plaintext: true`, so it is never an accident. It fits a disk the OS already encrypts, data that isn't sensitive, or a trusted Node host. Docs say what it means: records sit in the clear, with no tamper check. Its records carry `layers = 0`; a store opened with layers reads them as absent (and the reverse), so a switch never mixes the two.
3. **Exports:** `LocalStore`, `SealedStore`, `sealedStore`, `memoryLocalStore`, `indexedDbLocalStore`; `nodeDiskLocalStore` from `@zen/client/node`.

#### `@zen/db`: the queue on the store
1. **`localQueueStore(sealed, ns)`** implements `QueueStore` over store `queue/<ns>`. Entries are CBOR: `Value`s, bytes and bigints round-trip (`cbor.ts`); `Pending` ops encode as integer-keyed maps.
2. **Keeping the built commit.**
   * Before the first send of a batch, the queue stores an **inflight** record: the encoded Commit (`zw.encodeCommit`), with its `commit_id` and its entry ids.
   * `done` and `fail` remove it together with the entries; a rebuild (`conflict`, restamp, split) replaces it.
   * **At load:** an inflight record is resent first, as is, with the same `commit_id`, until its outcome is known: success settles its entries, a final error fails them, network keeps waiting. Its entries are not packed again until then.
   * **Counter totals are never stored** (user decision). The server's entry for this installation is the truth, and it is read when a send needs it (`loadTotals`). A stored total restored from a backup, or copied with the store, would be sent with a newer `seq` and roll the counter back. A resent counter commit can't double-count anyway: a `seq` that isn't newer is ignored.
   * `QueueStore` gains optional `putInflight(r)`, `loadInflight()` and `removeInflight()`; the memory store implements them too.
3. **Owner-only sender.** With a store shared between tabs (IndexedDB), only the owner sends (`db.onOwner`), as consumers do.
   * Every tab writes its entries to the store and overlays the queue it has loaded.
   * A tab that queues an entry posts `queued` on the tab channel (§9.2, `proxy.ts`), and the owner reloads the store.
   * `flush()` in a non-owner waits until its own entries leave the store: a `sent` broadcast from the owner, or polling `pending` from the store.
   * Without locks (`locks: null`) every instance sends, as now.
4. **The installation's counter lock across tabs.** Tabs share one clock store, and so one installation (`actor`). Today the lock that orders its counter commits, and the totals, exist separately in each tab. Two tabs incrementing one counter directly can then both read total B and send B+1 and B+2, and the server keeps one of them.
   * `stamp` takes a Web Lock `zen/actor/<actor hex>` (through `DbOptions.locks`), held from stamping to the commit's outcome: the same span as the in-memory lock, which it wraps.
   * Taking the lock, a tab keeps its in-memory totals only if it held the lock last. A tab releasing the lock posts `actor {tab}` on the tab channel (§9.2), so each tab knows the last holder; otherwise it drops its totals and reads them from the server.
   * Without Web Locks (Node, `locks: null`): the in-memory lock, as now.
   * **Test:** two tabs (`FakeLocks` + `FakeBus`) incrementing one counter directly and concurrently: the total is the sum.
5. **Options:** `DbOptions.local?: SealedStore`. When given, the queue uses `localQueueStore(local, ns)` unless `crdtStore` overrides it; 7e's caches use the same store.
6. **Wipe:** `Db.wipe(local, ns)` clears every store of a database (sign-out).

#### Spec (⚠️ `spec:` commit)
* **formats.md §4:** kind 8, local records.
* **zendb.md §9.3** rewritten, local state:
  * the store and the record format
  * the two layers and their defaults, and the device key
  * failures are treated as absent
  * wipe
  * what is stored (7d: the queue; 7e: the caches)
* **zendb.md §19.7:** the persisted queue, the inflight record and the resend at load, the owner-only sender.
* **zendb.md §17:** the `local` vectors.

#### Docs
* `docs/CLIENT.md`: the local store, the layers, `@zen/client/node`.
* `docs/DB.md` §19: "A queue that survives restarts".
* `spec/TECH_DEBT.md`: TD-DB-PERSISTENT-CACHE stays open until 7e.

#### Tests
* **Rust:** seal and open round-trip; a moved record (another store, key or fs) fails; the vectors.
* **`packages/client/test/local.test.ts`:**
  * every backend: memory, Node disk in a temporary directory, IndexedDB through `fake-indexeddb` (a new dev dependency); put, get, list, delete, clear
  * every layer combination round-trips, `plaintext: true` included; neither layer without it is refused; records of one setting read as absent under another
  * a flipped byte, or a record copied to another key or store, reads as absent
  * the Node device key file has mode 0600, and a second open reuses it
  * a store without its device key reads nothing
  * `vectors.test.ts` gains the `local` section
* **`packages/db/test/crdtpersist.test.ts`** (its own server; `net` from crdtoffline):
  * **Survives a restart:** queue writes offline, `close()`, reopen on the same disk store: entries are back, show in reads, and are sent once online.
  * **A crash after a lost response:** the commit applied, the reply was lost, then a close and reopen. The resend has the same `commit_id` (captured) and the counter total is right (applied once).
  * **A crash before any send:** a restart sends the entries normally.
  * **Two tabs on one IndexedDB store** (`FakeLocks`, `FakeBus`): only the owner sends (commits counted per session); the other tab's writes arrive; closing the owner hands sending to the other tab.
  * **Layers:** fs layer on, device layer off, both: the queue round-trips. A record sealed for another fs is dropped with a warning.
  * **`Db.wipe`** leaves the store empty.
* **Verification:**
  * Rust: `cargo fmt --check`, clippy (default and `--no-default-features`), `cargo test -p zen-core`, `gen_vectors` (only the `local` section changes), `scripts/build-wasm.sh`
  * JS: `npx biome check`; tsc for client (test config), fuse and db test; `npx vitest run` three times
  * CI green on the PR before stopping
* **Disk:** `CARGO_INCREMENTAL=0`; check disk first.

### 7e: persisted caches and offline open (detailed plan)

**Context.** 7d (PR #24, merged) gives the sealed local store and puts the CRDT queue on it. Every cache is still in memory, so a restart starts cold, and nothing opens offline: `unlock` fetches the fs header, and `Db.open` calls `/v1/info`, reads the DbRecord, runs `migrate` and loads the catalog, all from the server. 7e puts the caches on the store and lets `unlock` and `Db.open` work offline. It resolves TD-DB-PERSISTENT-CACHE and finishes step 7.

**User decisions:**
* All four caches persist: CRDT states, rows and the catalog, private-index nodes, sealed blobs.
* `unlock` and `Db.open` work offline from stored state; migrations wait for the server.
* **Offline reads of transactional rows are opt-in** (`read: 'cache'`). The default stays strict: offline, a read-only transaction that used cached rows fails, as now.

**After compaction, re-read:**
* this section and the 7d log line
* `spec/zendb.md` §3.1, §7.7, §9.3, §19.4
* `packages/db/src/{db,txn,crdtcache,localq,query}.ts`, `index/{private,sealed}.ts`
* `packages/client/src/{fs,client,local/sealed}.ts`
* `packages/db/test/{crdtpersist,crdtoffline,txn}.test.ts` (setup, `net`)

**Branch/PR.** #24 is merged: restart the branch from main, commit this plan (`docs: milestone 5 step 7e plan`), and open a new PR that 7e joins.

#### Client: `unlock` offline (`packages/client/src/fs.ts`)
* `Fs.unlock(how?, {local?: SealedStore})` and `Fs.header({local})`:
  * Online: as now, then store the header's bytes (`header.encode()`) and its `version` in store `fs/<fs id>`, key `header`.
  * On a `network` error: decode the stored header (`zw.FsHeader.decode`, the fs-id check) and open it with `openHeader` as now. With nothing stored, the error stands.
  * The header carries its own protection (slots are AEAD-sealed). It is no less trusted than one fetched online, since signed headers come in milestone 6.
* **Layers.** The fs layer can't seal the header it unlocks, so `unlock` refuses a store with the fs layer (`TypeError`). An app that wants the fs layer for its database uses two `sealedStore`s over one backend (documented).
* `Fs.forget(local)` deletes the stored header (sign-out).
* `UnlockedFs.editHeader` refreshes the stored copy when it was unlocked with `local`.
* **Session offline:** the app resumes its own session with `client.resume(token)`, which is already offline. Unlock material is then explicit, since a resumed session holds none (documented).

#### `@zen/db`: caches on the store
1. **`localcache.ts`, a write-behind layer:** `LocalCache(sealed, store)` with `put(key, bytes)`, `delete(key)` and `load()`.
   * Writes are gathered and flushed every 100 ms and on `close()`, so filling a cache costs no store round trip per entry.
   * A write lost to a crash only means a colder cache: every entry is checked by its version or digest when used.
   * Store names, all listed in `localStores(ns)` so `Db.wipe` clears them: `queue/<ns>`, `db/<ns>`, `rows/<ns>`, `crdt/<ns>`, `nodes/<ns>`, `sealed/<ns>`.
2. **What each cache stores** (with `DbOptions.local`; `DbOptions.persist: false` keeps caches in memory while the queue stays persisted):
   * **Catalog** (`db/<ns>`): `info` (the `/v1/info` limits and features), `db` (the DbRecord), and `t<name>` (a TableRecord with its version, via `encodeTableRecord`). Written at open, and on every `catalogSet` / `catalogStale` through `Db`. Loaded at open (offline open, below).
   * **Rows** (`rows/<ns>`, key = the stored row key): `{1: version, 2: fields (CBOR), 3: parts}`. Written by `cacheRow`, deleted by `dropRow`, `rowsWritten` and LRU eviction. Loaded at open, newest first, up to `rowCache`; the excess is deleted. The checks of §7.7 are unchanged: a stale persisted row makes its transaction re-run once.
   * **CRDT states** (`crdt/<ns>`, key = object): `{1: ObjState (zw.encodeObjState) or absent, 2: checked, 3: stale}`. Written by `put`, `confirm` and `markStale`, deleted by eviction. Loaded at open up to `crdtCache`, so `cache` reads, `auto` reads (an old `checked` costs a version check), `statesIn` scans and offline reads work after a restart. The in-flight marks (`marks`, `gen`) aren't stored.
   * **Private-index nodes** (`nodes/<ns>`, key = index id ‖ node id): the node bytes. Read lazily on a memory miss, before the server, and checked against `IndexKeys.nodeId`. Written by `cacheNode`; evicted with the memory LRU (`NODE_CACHE`).
   * **Sealed blobs** (`sealed/<ns>`, key = index id): `{1: digest, 2: blob}`. Read lazily when the head's digest misses in memory, checked with `partsDigest`. Written by `cacheSealed`; one per index, replaced.
3. **Opt-in offline reads of transactional rows (`read: 'cache'`):**
   * `db.transaction(fn, {read: 'cache'})` and `table.get(pk, {read: 'cache'})` already exist for CRDT tables.
   * On transactional tables, `cache` means: online, exactly as now (cached rows checked by version). When the server can't be reached, a read-only transaction answers from cached rows unchecked, and a row that isn't cached is `offline`.
   * Writes, queries on transactional tables (their roots and index entries need the server), and the default `auto` mode stay strict offline.
   * The finish check: a `network` error in `finish`'s `snapshotGet` is accepted in `cache` mode for a read-only transaction.
4. **Offline open (`Db.open`):**
   * Online: as now, then store `info`, the DbRecord and the catalog.
   * When `/v1/info` fails with `network` and `local` holds this database:
     * Open from the stored `info`, DbRecord and catalog, if the format and integrity pass and the stored schema equals `opts.schema`. Otherwise `DbError('offline', 'the database needs the server to open (a migration is due)')`.
     * `init` and `migrate` are skipped; they run at the next online open.
     * The owner election, scheduler, indexers and consumers start as usual and back off while offline (their existing network handling).
   * `db.offline` (true when it opened from stored state) lets apps tell.
   * Clock: a cold start offline ticks from the local wall clock (no `sync`). Queued edits are re-stamped on `stale_op` / `clock_skew` at send, as now. To keep the HLC and the counter actor across restarts, the app gives the session a `ClockStore` (documented; a store on the local store would need an async `ClockStore`, out of scope).
5. **Wipe:** `Db.wipe(local, ns)` clears every store above; `Fs.forget(local)` the header.

#### Spec (⚠️ `spec:` commit)
* **zendb.md §9.3:** what is stored (the stores and records above), write-behind, every entry checked on use, `persist: false`, wipe.
* **§3.1:** opening offline (stored info, DbRecord and catalog; the schema must match; migrations wait).
* **§7.7:** persisted rows; `read: 'cache'` on transactional tables.
* **§19.4:** CRDT states persist with their `checked` times.
* **fs.md (or the unlock section of formats.md §12):** the stored header used offline.

#### Docs
* `docs/CLIENT.md` §4 (`unlock({local})`, `forget`) and §10.
* `docs/DB.md`: caches and coherence (persisted caches), opening offline, `read: 'cache'` on transactional tables.
* `spec/TECH_DEBT.md`: TD-DB-PERSISTENT-CACHE resolved.

#### Tests
* **`packages/client/test/local.test.ts`** (with a server, or a separate `unlockoffline` test in `kv.test.ts`'s world):
  * `unlock({local})` online stores the header; offline (`network` from a wrapped `session.call`) it unlocks from it and reads nothing else.
  * Nothing stored → `network`. A store with the fs layer is refused.
  * `forget` removes it.
* **`packages/db/test/dbpersist.test.ts`** (its own server; `net` as in crdtpersist):
  * **Cold start offline:** open online with `local`, read CRDT rows and transactional rows, close. Then go offline, unlock from the stored header, and `Db.open` from stored state (`db.offline`):
    * CRDT `get` in `auto` and `cache` modes, and a `cache` query, answer.
    * A transactional `get` with `read: 'cache'` answers.
    * The default read fails `offline` (strict).
    * Queued CRDT writes work and are sent once online.
  * **Schema mismatch offline** → `offline`; online it migrates.
  * **Warm start online:** a reopened `Db` serves a cached row with no row fetch (`stats.rowHits`); a private-index query fetches no nodes (count `/v1/kv/get` of node keys); a sealed query fetches no parts.
  * **A stale persisted row** (changed by another installation while closed) re-runs its transaction once and returns the new value.
  * **CRDT `auto` after a restart** costs one version check, not a `get`.
  * **Tampering:** a flipped persisted record is dropped with a warning and fetched again.
  * **`persist: false`:** caches stay cold; the queue still persists.
  * **`Db.wipe` / `Fs.forget`** leave the store empty.
* **Verification:**
  * JS: `npx biome check`; tsc for client (test config), fuse and db test; `npx vitest run` three times.
  * Rust is unchanged unless the spec needs vectors (none planned).
  * CI green on the PR before stopping.

### Rhythm
1. Restart from main, log the step 7 plan line, commit, push, open a new PR.
2. Build 7a on the user's go: the `spec:` commit, then the code commit.
3. Log the 7a line, push, update the PR, confirm CI is green, stop for compaction.
4. 7b: restart from main, commit the 7b plan (`docs: milestone 5 step 7b plan`), open a new PR. Build it on the user's go (the `spec:` commit, then the code commit), log, push, confirm CI is green, stop.
5. 7c the same way: restart from main, commit the 7c plan (`docs: milestone 5 step 7c plan`), open a new PR; build on the user's go (`spec:` commit, then code), log, push, confirm CI is green, stop.
6. 7d and 7e the same way, each from main with its plan commit (`docs: milestone 5 step 7d plan`, then 7e's).
7. 7f, 7g and 7h (added after 7e, see below) the same way: one PR each, each starting from main once the previous PR is merged. Step 7 is then done, and step 8 (`@zen/fs`, on the sealed local store and the paged layout) is next.

### 7f: large files, the format and the server (detailed plan)

**Context.** Today a file version is one value: a plaintext chunk list, which the server uses to count references for GC, followed by a sealed manifest (kind 5) that repeats the list. At 32 B per chunk within `max_value_bytes` (90,000), a version holds at most about 2,800 chunks of 64 KiB, roughly 175 MiB (TD-FS-LARGE-FILES).

Every chunk is also sealed directly under the fs-data key of its epoch, so no single file can be handed to someone without handing over the whole fs.

Step 8's replica will store and replay manifests and `write` ops, and milestones 9 and 10 need files past the cap. So the layout changes now.

**User decisions:**
* two-level pages
* per-file content keys, in the same format change
* 7f is the Rust side. The TS client keeps writing v1 until 7g.

**After compaction, re-read:**
* this section
* `spec/formats.md` §2, §4, §11; `spec/fs.md` §4, §4.1, §6; `spec/keyspace.md` §3.6; `spec/api.md` §2, §6, §12; `spec/labels.md`
* `crates/zen-core/src/{fs,keys,labels,vectors}.rs`
* `crates/zen-server/src/tree.rs`:
  * `encode_version`/`decode_version` (~201)
  * `gc_candidate`/`release_chunk` (~276)
  * `apply_write` (~600)
  * `put_chunk` (~657)
  * `apply` `Write` (~715)
  * `sweep` (~1110)
  * the purge release (~1596)
  * `collect_chunks` (~1660)
  * `file_get`/`chunks_get` (~1036)
* `crates/zen-server/src/{commit.rs (validate ~78, ~145–190; apply ~380), keys.rs (~385–407), lib.rs (features ~68, routes)}`
* `crates/zen-proto/src/lib.rs`: `Commit` (~1405), `ChunkPut` (~1993), `CrdtOp::Write` (~2058), `FileGet`/`Version`/`Versions`/`ChunksGet`/`Chunks` (~2700–2800)
* `crates/zen-wasm/src/{crypto.rs (Manifest ~52/235/365, sealChunk ~401, writeOpBytes ~1030), wire.rs (~89, ~112)}`
* `crates/zen-server/tests/fs.rs` (helpers, the property test)

**Branch/PR.** #26 (the milestones 8–11 plan) must be merged first. Then restart from main, commit `docs: milestone 5 step 7f plan` (syncing `docs/MILESTONE-5.md`), and open a new PR that 7f joins.

#### Formats (⚠️ `spec:` commit)
1. **Page** (formats.md §11.6, new):
   * The plaintext the server sees is `u32 n ‖ n × chunk_id(16)`, with `1 ≤ n` and `4 + 16n ≤ max_value_bytes`. That is up to 5,624 ids at the defaults, about 351 MiB at 64 KiB.
   * Its id is 16 random bytes chosen by the client. A page is immutable.
   * `page_digest = BLAKE3.derive_key("zen/v1/fs-page", page bytes)` (32 B), the same construction as the op chain.
2. **Manifest v2** (formats.md §11.3; the plaintext of kind 5, with the same AAD as v1):
   ```
   u8 manifest_version = 2 ‖ u64 size ‖ u32 chunk_size ‖ u64 n ‖ u32 key_epoch ‖ file_key(32) ‖ u8 form
   form 1 (inline): n × chunk_id                          (u64 n above is the count)
   form 2 (paged):  u32 page_size ‖ u32 m ‖ m × (page_id(16) ‖ page_digest(32))
   ```
   * **Paged:** `m = ceil(n / page_size)`. Every page holds `page_size` ids except the last. Chunk i is in page `i / page_size`, at slot `i % page_size`.
   * **Readers check:**
     * each page's digest against its bytes, when the page is fetched
     * every page's count
     * the inline list, or the page ids, equal the `write`'s `chunks` or `pages`
   * **Cap:** 48 B per page. At the defaults the root holds about 1,870 pages, so about 10.5 M chunks, ≈ 640 GiB at 64 KiB (more with larger chunks).
   * v1 stays valid to read. Clients write v2 from 7g on.
3. **File key** (formats.md §2, §11.4):
   * The chunk key is `KDF("zen/v1/fs-file", file_key, u32(fs))`.
   * Kind 6 keeps its label and context (`u32(fs) ‖ chunk_id`). Its header `key_epoch` is the manifest's `key_epoch`: the epoch in which the file key was made. A reader opens it with the key derived from the manifest, not the fs epoch keys.
   * A v1 manifest means its chunks are under the fs-data key, as now.
   * **Rotation (fs.md §4, a client rule):** a writer whose fs is at an epoch newer than the node's `key_epoch` picks a fresh file key, and re-seals and re-uploads every chunk. Otherwise it keeps the node's key, so `previous` reuse works. A copy to another node may carry the key and reuse the chunks.
4. **Op chain** (§11.5): the tag `write_paged = 0x04 ‖ u32(fs) ‖ tree ‖ node ‖ u32(r) ‖ r × dot ‖ u32(m) ‖ m × page_id ‖ lp(manifest)`. The inline `write` (0x03) is unchanged.
5. **Labels** (labels.md, `labels.rs`):
   * `zen/v1/fs-file` (KDF)
   * `zen/v1/fs-page` (digest)
   * `zen/v1/sig/app-bundle`, listed as reserved for milestone 11 (spec only)

#### Wire and server semantics (⚠️ `spec:` commit: fs.md §4, §4.1, §6; keyspace.md §3.6; api.md §2, §6, §12)
1. **Commit field `pages: [{fs, id, data}]`**, after `chunks`. Applied after the chunks and before the ops.
   * `data` must parse: `n ≥ 1`, and its length is exactly `4 + 16n`.
   * Every chunk in it must exist, or be uploaded in the same commit.
   * Identical bytes under an existing id restart its grace. Different bytes are refused (400).
   * **A first upload** stores the page and adds 1 to each listed chunk's `cr`, once per occurrence.
   * Every upload makes the page a GC candidate (restarting the grace period).
   * Validation:
     * each page counts 1 toward `max_commit_ops`
     * it adds `len + 16` bytes toward `max_commit_bytes`
     * it is checked with `value_ok`
     * it needs fs write
2. **`write {node, replaces, chunks | pages, manifest}`:**
   * `CrdtOp::Write` gains `pages: [bytes(16)]` (optional, serde default).
   * Both lists non-empty → 400. Both empty is an empty file, as now.
   * Every page must exist or be in the commit, else 400 `unknown page`.
   * The size check is `value_ok(manifest + 16 × (chunks | pages))`. Bytes are `48 + manifest + 16·len + 12·replaces`.
   * One write per node per commit, as now.
3. **Stored version (`tf`):**
   * inline: unchanged, `dev ‖ u32 n ‖ n × chunk ‖ manifest`
   * paged: `dev ‖ u32 0xFFFFFFFF ‖ u32 m ‖ m × page_id ‖ manifest`
   * `n` can never reach `0xFFFFFFFF` under the value cap, so existing data needs no migration.
   * Applying a paged write adds 1 to `pr` per page. A replace or a purge subtracts 1 per page and makes the page a candidate. Quota counts `manifest + 16m`, as for chunks.
4. **Keys** (keyspace.md §3.6; free prefixes, checked against `keys.rs`):
   | Key | Value |
   |---|---|
   | `pack("pg", fs, page)` | the page bytes |
   | `pack("pr", fs, page)` | `i64` LE, atomic add: the versions referencing the page |
   | `pack("pz", fs, cvs, page)` | empty: page GC candidate |
   | `pack("pc", fs, page)` | `cvs(12)` of the page's newest candidate |
   * `cr` becomes "the number of versions **and pages** referencing the chunk".
5. **GC (fs.md §6):** `collect_pages` mirrors `collect_chunks`.
   * It takes candidates older than the grace period, 500 at a time, the newest only (via `pc`).
   * If `pr ≤ 0`: delete `pg`, `pr` and `pc`, and release each listed chunk (`cr −1`, a chunk candidate). The chunks then wait one more grace period, which is simpler and only safer.
   * Quota drops by the page bytes.
   * `sweep` runs `collect_pages` before `collect_chunks`, in the same rounds per fs.
6. **Reads:**
   * `Version` gains `pages: [bytes(16)]` (optional). `/v1/fs/file/get` returns it for paged versions, with `chunks` empty.
   * New `POST /v1/fs/pages/get {fs, ids: [bytes(16)] (≤ 64)}` → `{pages: [{id, data?}]}`, under fs read, like `chunks/get`.
7. **`/v1/info` features:** `"fs_pages"`.

#### Code
* **zen-core `fs.rs`:**
  * `Page {chunks}` with `encode` / `decode` (checking the limits passed in), and `page_digest`.
  * `Manifest` becomes `enum Manifest { V1 {size, chunk_size, chunks}, V2 {size, chunk_size, n, key_epoch, file_key, body: Body} }` with `enum Body { Inline(Vec<Id>), Paged {page_size, pages: Vec<(Id, [u8;32])>} }`. `encode`/`decode` dispatch on the version byte, and `decode` checks `m = ceil(n/page_size)` and the inline count.
  * `seal_manifest`/`open_manifest` work unchanged on the enum.
  * `FileKey` (32 B, random via `Rng`), `file_chunk_key`.
  * `seal_file_chunk(fs_id, file_key, key_epoch, chunk, data, rng)` / `open_file_chunk(...)`, which checks the header epoch.
  * `write_paged_bytes`.
  * `labels.rs`: `FS_FILE`, `FS_PAGE`, added to the label list (and its uniqueness test).
* **zen-core `vectors.rs` `fs_vectors`:** new keys in `spec/test-vectors/fs.json`:
  * `page` (plaintext, digest)
  * `manifest_v2_inline` and `manifest_v2_paged` (plaintext, sealed, seeds)
  * `file_chunk` (file key from `DetRng("fs-file")`, key_epoch, sealed)
  * `write_paged` (op bytes)

  Existing keys are unchanged.
* **zen-proto:**
  * `PagePut {fs, id, data}` and `Commit.pages`
  * `CrdtOp::Write.pages`
  * `Version.pages`
  * `PagesGet {fs, ids}`, `PageData {id, data?}`, `Pages {pages}`
  * each with the ts derives, like their chunk twins
* **zen-server:**
  * `keys.rs`: `page`, `page_refs`, `page_gc`, `page_gc_ptr`.
  * `commit.rs`:
    * validate: pages, ops and bytes, the one-of rule
    * apply: `eng.put_page` after `put_chunk`
  * `tree.rs`:
    * `encode_version`/`decode_version` → `enum Content { Chunks(Vec<Id>), Pages(Vec<Id>) }`
    * `release_content`, used by `apply_write` and the purge
    * `put_page` (tracks `new_pages`, as `new_chunks`)
    * `apply_write` paged
    * `page_gc_candidate`, `collect_pages`, `sweep`
    * `file_get`: `pages`
    * `pages_get`
  * `lib.rs`: the route and the feature.
* **zen-wasm:**
  * `crypto.rs`:
    * `sealManifest` accepts the v1 shape `{size, chunkSize, chunks}`, or v2 `{version: 2, size, chunkSize, n, keyEpoch, fileKey, chunks? | pageSize + pages: [{id, digest}]}`
    * `openManifest` returns the shape with `version`
    * `newFileKey()`, `sealFileChunk`, `openFileChunk`
    * `encodePage`, `decodePage`, `pageDigest`, `writePagedOpBytes`
  * `wire.rs`: `PagePut`, `PagesGet`, `PageData`, `Pages`.
  * The TS client compiles unchanged: v1 calls keep their shape. `tree.ts` refuses a v2 manifest with a clear error until 7g.
* **Docs:**
  * `docs/API.md`: the route and the commit field
  * `docs/STATS.md` §1.3: the cap, as the format allows (the client in 7g)
  * TECH_DEBT TD-FS-LARGE-FILES: "server side done in 7f"

#### Tests
* **zen-core units:**
  * Page and Manifest v2 round trips, and refusals: a bad count, a short page, a form byte other than 1 or 2, `m` not equal to `ceil`
  * v1 still decodes
  * a file chunk opens only with its key, epoch and id. Under another file key, or with a changed header epoch, it fails.
  * the digest is domain-separated
* **`crates/zen-server/tests/fs.rs`, both backends:**
  * **Paged round trip:** upload chunks plus 3 pages over 2 commits, then a paged write. `file_get` returns `pages`, and `pages_get` returns the bytes.
  * **Refusals:**
    * an unknown page in a write
    * a page holding an unknown chunk
    * a page over `max_value_bytes`, or malformed
    * different bytes under an existing page id
    * a write with both `chunks` and `pages`
  * **Sharing:** version B reuses 2 of version A's pages plus a new last page, then replaces A. After the grace period (test `chunk_grace_secs`), only A's unshared last page and its unique chunks are gone; B reads fully.
  * **GC order:** a replace, then sweeps. The page is deleted first, then its chunks one grace period later. A page uploaded but never referenced is collected, and its chunks after it.
  * **Re-upload:** the same page id again restarts its grace and doesn't double `cr` (checked through deletion timing).
  * **Purge:** trashing and purging a paged node releases its pages.
  * **Quota:** page bytes are counted, and released after GC.
  * **Op chain:** a paged write's chain value equals `chain_next` over `write_paged_bytes` (as the inline test does).
  * **Property test:** the xorshift test gains paged writes and appends (a version reusing a prefix of pages). A reference model tracks expected live pages and chunks, checked after sweeps with grace 0.
* **`packages/client/test/vectors.test.ts`:** the new fs.json keys through wasm.
* The existing vitest suite stays green (v1 writes).

#### Verification
* **Disk:** `CARGO_INCREMENTAL=0`; check `df` first (about 6.9 GB at the last check).
* **Rust:**
  * `cargo fmt --check`
  * `cargo clippy --all-targets -- -D warnings`, default and `--no-default-features`
  * `cargo test -p zen-core -p zen-proto -p zen-server`
  * `gen_vectors`: only the new fs.json keys change (`git diff spec/test-vectors`)
  * `scripts/build-wasm.sh`
* **JS:** `npx biome check`; tsc for client, fuse and db test; `npx vitest run`.
* CI green on the PR (the fdb job covers the server tests on FoundationDB).

#### Rhythm
1. After #26 merges: restart from main, plan commit, new PR.
2. On the user's go: the `spec:` commit, then the code commit.
3. Log the 7f line, push, confirm CI is green, stop for compaction.
4. 7g is next.

### 7g: large files in the client and zen-mount (detailed plan)

**Context.** 7f (PR #27, merged) gives the server paged versions and the formats: version-2 manifests with a file key, pages, and `write_paged`. The TS client still writes version 1 and refuses paged versions (`unsupported`).

7g moves `@zen/client`'s `Tree` (and so zen-mount and `@zen/db`'s `tx.fs`) onto version 2:
* every new write is a version-2 manifest with a file key
* a file too large for the inline form is paged
* reads go through pages
* uploads stream and can resume

This resolves TD-FS-LARGE-FILES. The wire and the spec are unchanged, unless an edge case shows up (then a `spec:` commit).

**After compaction, re-read:**
* this section and the 7f log line
* `spec/formats.md` §11.3–11.6, `spec/fs.md` §4–4.2
* `packages/client/src/tree.ts`:
  * types (~50–190)
  * `versions`, `pickVersion`, `readFile`, `read`, `fetchChunks`, `chunkIndex`
  * `writeFile`, `upload`, `writeOp`, `stage`, `resolve` (~375–700)
  * `blocks`
* `packages/client/src/fs.ts` (`sealKeys`, `keysFor`, `keysAt`, `rotate`)
* `crates/zen-wasm/src/crypto.rs`: the 7f bindings (`sealManifestV2`, `openAnyManifest`, `sealFileChunk`/`openFileChunk`, page codecs)
* `packages/db/src/{files,txn}.ts` (`checkLimits`, `crdtOnly`)
* `packages/fuse/src/mount.ts` (~140–350)
* `packages/client/test/tree.test.ts` (setup)
* `packages/fuse/test/mount.test.ts`

**Branch/PR.** #27 is merged: restart the branch from main, commit this plan (`docs: milestone 5 step 7g plan`), and open a new PR that 7g joins.

#### `tree.ts`
1. **Types:**
   * `FileVersion`:
     * `manifest: AnyManifest`, the 7f shape: v1 `{version: 1, size, chunkSize, chunks}`, or v2 with `n`, `keyEpoch`, `fileKey`, and `chunks` or `pageSize` + `pages`
     * `chunks`: inline ids, empty when paged
     * `pages`: page ids, empty when inline
     * `size` and `chunkCount`, for convenience
   * `ChunkIndex` gains `fileKey?`, `keyEpoch?` (absent: version-1 chunks, which can't be reused), and `pageSize?` with `pages?: {id, digest}[]` (for page reuse).
   * `Upload` carries `manifest: AnyManifest`, `pages`, `pagesUploaded`.
   * The `Manifest` export stays the v1 shape; `AnyManifest` and `ManifestV2` are added. Documented as an API change of `FileVersion.manifest`; `size` and `chunkSize` keep their meaning.
2. **`versions(node)`:**
   * Opens with `openAnyManifest`.
   * Checks the list against the manifest: v1 and v2 inline against `chunks`; paged against `pages` (ids in order).
   * The 7f `unsupported` guard goes.
3. **Chunk ids of a range: `chunkIds(v, first, last)`.**
   * Inline: a slice.
   * Paged:
     * Fetch the pages covering chunks `first..last` through `/v1/fs/pages/get` (64 per request), with a page cache: `TreeOptions.pageCache?: ChunkCache` (the same `get`/`put` interface, so step 8 can persist it), plus a small in-memory LRU of decoded pages (256).
     * Each fetched page is checked against its manifest digest (`pageDigest`) and its expected count (the last page is shorter). A mismatch or a missing page → `ZenError(0, 'format' | 'not_found')`.
4. **Opening chunks:**
   * `openChunk(v, id, sealed)` uses `openFileChunk(fs, fileKey, keyEpoch, …)` for v2, and the fs keys (`keysFor`) for v1.
   * `fetchChunks(ids, version?)`: the version picks the opener (default v1, so existing callers are unchanged). The chunk cache keeps sealed bytes, as now.
5. **Reads:**
   * `read(node, v, offset, length)` goes through `chunkIds`.
   * **New `readStream(node, version?, {offset, length})` → `ReadableStream<Uint8Array>`:**
     * windows of up to 64 chunks per pull, fetched and opened lazily
     * pages fetched as reached
     * `cancel` stops fetching
     * memory: one window
   * `readFile` keeps its signature (the whole file in memory, documented).
   * `chunkIndex(node, v, data?)`: without `data`, it hashes through `readStream`, so it never holds the file. It records `fileKey`/`keyEpoch` and `pageSize`/`pages`.
6. **The file key**, `fileKeyFor(node, previous?)`, following fs.md §4.2:
   * the key of `previous` when given; otherwise the newest version-2 manifest's key (from the `versions` read the write does anyway for `replaces: 'current'`; one extra `file/get` otherwise)
   * kept only if its `keyEpoch` equals the fs's current epoch (`sealKeys().epoch`)
   * otherwise a fresh `newFileKey()` at the current epoch
   * Reusing chunks from `previous` needs the same key (a v1 `previous`, or a fresh key, re-seals every chunk).
7. **Writing (`stage`):**
   * **Source:** a `Uint8Array`, `Iterable`, `AsyncIterable` or `ReadableStream` (`blocks()` reads a stream through its reader).
   * **Chunk bytes** are sealed with `sealFileChunk` and flushed in commits as now. Memory is one commit's chunks, plus 16 B per chunk id and 32 B per hash (for the index).
   * **Default chunk size:** `min(64 KiB, max_value_bytes − 64)`, so a server with a small `max_value_bytes` still works. An explicit `chunkSize` is kept and checked as now.
   * **Form:**
     * The ids collect in memory until the count passes the inline limit: the largest `n` with `sealedManifestV2Inline(n) + 16n ≤ max_value_bytes`, computed from the sizes, about 2,800 at the defaults.
     * Past it the write is paged: `pageSize = pageCapacity(max_value_bytes)`. Each full page is encoded and joins the pending commit (`pages` field; cost `512 + data + 16 + 48 × ids`, as the server counts), once its chunks are in that commit or an earlier one.
     * The last partial page goes in the final flush.
     * Paged needs the `"fs_pages"` feature (`client.info()`); without it, a file past the inline limit → 413, as before.
   * **Page reuse (appends):** with `previous` paged at the same `pageSize`, page `i` reuses the previous page `i` (its id and digest, no upload) when its chunk ids equal the previous slice. They do when the chunks were reused, as in an append.
   * **Manifest:** v2, sealed with `sealManifestV2`; its sealed size + 16 × (chunks | pages) ≤ `max_value_bytes`, else 413.
8. **Resumable uploads:**
   * `upload(node, data, {onProgress?(state), resume?: state})`.
   * After each flush, `onProgress` gets an `UploadState`: plain data (bytes and numbers) that the caller can keep (step 8 keeps it on the local store). It holds:
     * `node`, `fileKey`, `keyEpoch`, `chunkSize`
     * `size` and the flushed chunks' ids and hashes
     * the full pages flushed (`{id, digest}`)
     * `form`
   * With `resume`, `data` is the rest of the source from `state.size`, which is always a multiple of `chunkSize`. Nothing already flushed is sent again.
   * It works within `chunk_grace_secs`; documented, with longer resumes in M8.
9. **`writeOp(u)`:** the op carries `chunks` or `pages`. `writeFile`'s write-cost estimate counts `pages`.
10. **`resolve`:** rewrites a kept version with its `chunks` or `pages` and its sealed manifest, as now.

#### `@zen/db`
* `txn.ts` `checkLimits` counts `c.pages` (one op each, plus `data + 48 × ids` bytes).
* `crdtOnly` treats `pages` like `chunks`.
* `files.ts` needs no change: `writeOp` carries pages.

#### zen-mount (`packages/fuse/src/mount.ts`)
* It works on `FileVersion` v2 through the tree API.
* `size` comes from `manifest.size`, unchanged.
* `chunkIndex` hashes without the buffer when the file wasn't read whole.
* Large writes are still buffered in memory until step 8 (documented in CLIENT.md §9).

#### Docs
* `docs/CLIENT.md`:
  * §8: version 2, file keys, pages, `readStream`, streaming and resumable uploads, `pageCache`
  * §9: limits, which are now about 640 GiB at the defaults; zen-mount still buffers a written file in memory
* `docs/STATS.md` §1.3: clients write paged versions.
* `spec/TECH_DEBT.md`:
  * TD-FS-LARGE-FILES resolved (the rest is M8)
  * TD-CRDT-ROWS-SERVER marked resolved (step 3; stale)

#### Tests
* **`packages/client/test/large.test.ts`** (new, its own server: `max_value_bytes` 2,048, so chunks are about 1.9 KiB, a page holds 127 ids and the inline limit is about 55 chunks):
  * **Round trip:** a file of about 400 chunks (≥ 3 pages) written from a `Uint8Array`, an async generator and a `ReadableStream`. `readFile` and `readStream` equal the source; `FileVersion.pages` has the right count.
  * **Range reads:** a read in the middle fetches one page and only its chunks (`/v1/fs/pages/get` and `/v1/fs/chunks/get` calls counted through a wrapped `session.call`). A second read of the same page hits the page cache.
  * **Append:** `writeFile` with `previous` and appended bytes. Only the last page and the new chunks are uploaded (`uploaded`, `pagesUploaded`), the file key is unchanged, and the old full pages are reused by id.
  * **Small files:** stay inline v2 (`pages` empty); a file crossing the inline limit becomes paged.
  * **Tampering:** a page swapped on the server (a wrapped `session.call` that returns another page's bytes) → `format`; a missing page → `not_found`.
  * **Version 1:** a v1 version written by hand (`sealManifest`, `sealChunk` and a raw write op) reads, through `readFile`, `read` and `readStream`. Rewriting it with `previous` gives v2 with a fresh key and every chunk re-sealed (`uploaded` = all).
  * **Keys:**
    * two files' chunks don't open with each other's keys (`openFileChunk`)
    * after `fs.rotate` (and a new unlock), the next write picks a fresh key at the new epoch and re-uploads every chunk
    * without rotation, the key is kept
  * **Resume:**
    * a source that throws after some chunks
    * the last `onProgress` state, then `upload(…, {resume})` with the rest
    * the result reads back equal, and the chunks and pages already flushed are not uploaded again (calls counted)
  * **No `"fs_pages"`** (a wrapped `client.info`): a file past the inline limit → 413.
* **`tree.test.ts`:** the existing tests pass on v2 (the `uploaded` counts in reuse tests unchanged).
* **`@zen/db`:** a `tx.fs(tree).write(upload)` of a paged upload in a transaction with a row (`txn.test.ts` or `files`), and `checkLimits` counting pages.
* **zen-mount** (`mount.test.ts`, skipped where FUSE is missing, run in CI): a file of several pages written through the kernel and read back. Its world gets a lowered `max_value_bytes`, or a separate describe with one.
* **Verification:**
  * `npx biome check`
  * tsc for client (test), fuse, db (test)
  * `npx vitest run` three times
  * Rust unchanged (`cargo test` not needed unless wasm changes)
  * CI green on the PR

#### Rhythm
1. Restart from main, commit the plan, open a new PR.
2. On the user's go: the code commit (a `spec:` commit only if needed).
3. Log the 7g line in both plan copies, push, confirm CI is green, stop for compaction.
4. 7h is next.

### 7h: a credential vault on the local store (detailed plan)
**Context.** A service worker (M9), a git helper (M10) or a restarted zen-mount must sign in and unlock with no page or prompt. Today the app holds the device identity and the session token itself. GAPS G15 asks for keys wrapped at rest under a non-extractable WebCrypto key, which the sealed store's device layer already provides.
* **`packages/client/src/local/vault.ts`:** `credentialVault(sealed)`, on store `vault`:
  * `saveDevice(device credentials)`, `saveSession(token)`
  * `signIn(client)`: resume a stored token that is still valid, else `signInDevice` with the stored device, then store the new token
  * `forget()`
  * **Refused:**
    * a store without the device layer, unless it is `plaintext: true` and the app passes `allowPlaintext`
    * a store with the fs layer, which can't seal what unlocks the fs (as for the kept header)
* **Unlocking:** the device keyslot (`unlock({device})`) then works from the vault, offline too, with the kept header of 7e.
* **⚠️ Spec:** zendb.md §9.3 gains the `vault` store and what it holds.
* **Docs:** `docs/CLIENT.md` §10; GAPS G15, partly done.
* **Tests:**
  * a round trip on memory, IndexedDB and Node disk
  * a fresh "process" signs in and unlocks from the vault: online, and offline from the kept header
  * an expired token falls back to the device
  * a tampered record reads as absent
  * the refusals
  * `forget`
* **Verification:** as for 7g.

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
* **Constraints from milestones 8–11** (added after 7e):
  * **Layout:** the replica stores pages and sealed chunks as the server does (7f/7g), never plaintext. A resumable upload's state is kept on the local store.
  * **Worker-safe core:** `@zen/fs` uses no DOM and no Node imports outside its Node store, so it runs in a service worker or a worker (M9). Reads include `createReadStream(path, {start, end})` → `ReadableStream`.
  * **Content type:** a reserved xattr key in the sealed meta (spec'd in step 8's `spec:` commit), which M9 serves as `Content-Type`.
  * **Vault:** `zen-mount` can sign in and unlock from the vault (7h), e.g. `--vault <dir>`.

## Added features: milestones 8–11 (placeholders)

Four features were added after 7e, each as a placeholder milestone at the end of the roadmap (DESIGN-3 §6), with one doc each: `docs/MILESTONE-8.md` to `MILESTONE-11.md`.

Each doc has:
* a few lines on the feature
* a short plan
* what was moved earlier, and why
* the line: **"The rest is planned when the milestone starts."**

The Q/A that proposed them is input, not the design.

Order by dependency: 8 → 9 → 10 → 11. Milestone 6 (signed roots) comes before 11.

### Milestone 8: large files (the rest)
* **Feature:** files of hundreds of GiB that are streamed, resumable and appendable.
* **Moved earlier:**
  * **7f/7g (the layout, file keys, streams).** Step 8's replica stores and replays the format, so changing it after step 8 would mean designing the write path twice.
  * **Resumable state in step 8.**
* **Plan for the rest:**
  * Upload sessions that pin chunks and pages past `chunk_grace_secs`, so a resume works after more than 24 h.
  * Repacking pages after many edits in the middle of a file (a leader job).
  * Multi-GiB performance, measured in STATS.
  * The interplay with `zen-ublk` (M6).

### Milestone 9: serving zen-fs over HTTP through a service worker (`@zen/sw`)
* **Feature:** a worker maps URLs in its scope to fs nodes and answers each fetch with a decrypted, streamed `Response`, with `Range` → 206, MIME types and index pages. The HTML stays unchanged. It works offline from the replica. An encrypted website is then viewable by the members who unlocked it.
* **Moved earlier:**
  * **7g:** `readStream`.
  * **7h:** the vault, so the worker re-unlocks after the browser kills it.
  * **Step 8:** a worker-safe `@zen/fs` with stream reads, the content-type xattr, and sealed chunks at rest. This is so the worker never caches plaintext.
* **Plan for the rest:**
  * the `packages/sw` worker: URL → node, streams, Range, MIME, directories; lifecycle with `clients.claim` and updates
  * **origin isolation**, so untrusted fs content never runs on the origin that holds keys:
    * a separate content origin, with its own config in `zen-serve`
    * `CSP: sandbox` and `nosniff` on every worker response
  * `zen-serve static push` (DESIGN-3 §4.2, replicated fs 0), and `Range` in `statics.rs`
  * the worker as the owner candidate of TD-DB-SHAREDWORKER
  * Playwright tests: offline reads, seeking, the sandbox
* **Note:** DESIGN-3 §0 made the worker "an optional cache only". M9 makes it a serving path. Whether the worker also verifies code is a question for M11.

### Milestone 10: git over zen-serve (`git-remote-zen`)
* **Feature:** `git clone zen://host/<fs>/<repo>` through a remote helper written on the client. The server stays blind.
  * Packs (with their `.idx`) are immutable files.
  * Refs live in a zen-db table.
  * A push is one transaction: the pack's file op, a compare-and-set of the refs (non-fast-forward refused on the client unless `--force`), and an emit to `("git", repo, "pushes")` for CI workers.
  * Fetch downloads only the packs it lacks.
  * Read and write rights are the fs grants. Branch protection is cooperative only.
* **Moved earlier:**
  * **7f/7g:** packs over 175 MiB, and streaming uploads.
  * **7h:** the helper signs in without a prompt.
  * Already built: files plus rows in one commit (5c), emits (6a), and the scheduler for repacking (6b).
* **Plan for the rest:**
  * `packages/git` (a Node helper binary)
  * the refs schema
  * a pack manifest for large repos
  * a gc/repack leader job
  * an optional localhost smart-HTTP bridge (`insteadOf`)
  * a dumb-HTTP public mirror through M9's static push
  * tests with real `git`: clone, push, concurrent pushes, force

### Milestone 11: an encrypted web-app platform (trust, sharing, lifecycle)
* **Feature:** what zen-serve still lacks to qualify as an encrypted-storage web-app server:
  * **Code-delivery trust:** signed app bundles, verified by a pinned bootstrap service worker (this reopens DESIGN-3 §0), plus SRI and reproducible builds, with a trust anchor the server doesn't control.
  * **Sharing:** share links that carry a file key in the URL fragment plus a scoped read capability (TD-AUTH-TOKEN-SCOPES); folder shares; sharing with outsiders.
  * **Lifecycle:**
    * member removal: revoke, `rotate`, and fresh file keys on the next write
    * account recovery
    * Web Push with encrypted payloads
    * log retention (TD-LOG-RETENTION)
    * server timers (TD-BROKER-SERVER-TIMERS)
  * **Metadata:** padding options for sizes and page counts.
  * **Assurance:** a threat model of web delivery, audit readiness, and the wasm size (TD-CLIENT-WASM-OPT).
* **Moved earlier:**
  * **7f:** per-file content keys. Sharing one file then needs no format change. The `zen/v1/sig/app-bundle` label is reserved.
  * **7h:** the vault.
  * **Milestone 6:** signed roots, the rollback half of integrity. Its signing keys are reused for bundle signatures.
  * **M9:** the worker that carries the bootstrap.
* **Plan for the rest:** the five areas above, probably split into sub-milestones when planned.

### The plan commit (after approval)
1. Restart the branch from main.
2. Sync `docs/MILESTONE-5.md` with this file.
3. Add `docs/MILESTONE-8.md` to `MILESTONE-11.md` from the sections above.
4. Add items 8–11 to DESIGN-3 §6.
5. Point TD-FS-LARGE-FILES at 7f/7g.
6. Commit `docs: milestones 8–11 placeholders, step 7f–7h plan`, push, open a new PR.
7. 7f is built on the user's go.

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
