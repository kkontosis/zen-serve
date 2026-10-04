# Milestone 4.5: zen-db and the embedded broker (specs only)

The plan for this milestone is not written yet. This file holds its description, as decided by the user, and the documents to read before planning.

## Description

* **Specs only.** This milestone produces specifications; milestone 5 implements them.
* **zen-db and an embedded broker client, in one class.** The broker is an in-app class: part of the client library, running inside the app's own process. It offers messaging patterns built on topics and consumer groups: publish/subscribe, work queues, request/reply, delayed messages and sagas.
* **It uses the server-side broker API primitives and protocol already designed**, not new ones.
* **The same class as zen-db.** zen-db acts both for CRUD and to emit, subscribe to and consume events, together with its objects and **transactions**, as discussed at length in `spec/` and in `docs/`: database writes and broker messages are synchronized, in one transaction.
* **Some app examples and proof-of-concept targets** are part of the milestone.

## Read carefully before planning

* `docs/DESIGN.md`, `docs/DESIGN-2.md` (§1–2: primitives, the log, event sourcing, the transactional outbox, consumers), `docs/DESIGN-3.md` (§2: encrypted KV, transactions, zen-db; §3: single-leader event processing, fencing), `docs/DESIGN-4.md` (§1: event keys, consumer modes)
* `docs/API.md` (the draft client API: `db.transact`, `tx.publish`, `consumer().run`), `docs/GAPS.md`
* `spec/api.md` (§5–9: KV, commit, log, consumer groups, the stream), `spec/formats.md` (§3–5, §8), `spec/keyspace.md`, `spec/fs.md`
* `docs/MILESTONE-4.md` and `docs/CLIENT.md` (what `@zen/client` provides), `packages/client/src/{kv,log,stream}.ts`
