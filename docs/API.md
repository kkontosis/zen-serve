# zen-serve: API outline (draft)

Based on DESIGN.md, DESIGN-2.md and DESIGN-3.md. All names are provisional. This is the design-era outline; the normative API is [spec/api.md](../spec/api.md), which is what the server implements.

## 1. zen-serve HTTP / WebSocket API

The client authenticates with a device key. Every request carries a session token. The server enforces the signed ACL on `fs_id` and on topic-id prefixes.

### Session
| Method | Path | Purpose |
|---|---|---|
| POST | `/v1/auth/challenge` | get a nonce |
| POST | `/v1/auth/session` | device signs the nonce (hybrid signature) and gets a session token |

### Admin (opaque blobs, CAS on version)
| Method | Path | Purpose |
|---|---|---|
| GET/PUT | `/v1/fs/{fs}/header` | volume header + keyslots (`If-Match: <version>`) |
| GET/PUT | `/v1/acl` | signed ACL document |
| GET | `/v1/fs` | list filesystem ids the caller can access |

### KV
| Method | Path | Body | Returns |
|---|---|---|---|
| POST | `/v1/grv` | – | `read_version` (for short transactions) |
| POST | `/v1/kv/get` | `{fs, keys[], read_version?}` | `[{key, value, version}]` |
| POST | `/v1/kv/range` | `{fs, prefix \| begin/end, limit, reverse, after?, read_version?}` | `{items[], more, next}` |

### Commit: the only write path
```
POST /v1/commit
{
  commit_id,                                  // random 128-bit; makes the commit idempotent
  read_version?,                              // short mode: native FDB conflict ranges
  read_conflicts?: [{fs, begin, end}],
  expect?:  [{fs, key, version}],             // long mode: per-key version CAS
  expect_ranges?: [{fs, begin, end, hash}],
  writes?:  [{fs, key, value | null}],
  append?:  [{topic, key_token?, envelope}],  // events published only if this commits
  consume?: [{group, topic, partition? | key_token?, from, to, token}],
  crdt_ops?: [{fs, object, op}],             // server-merged CRDT objects (DESIGN-4 §2)
}
→ 200 {commit_version, appended: [versionstamp...]}
→ 409 {conflict}            // retryable
→ 412 {cursor_moved | not_leader}   // not retryable for this event
```

**Idempotency:** `(commit_id → result)` is recorded in the same transaction. If the connection drops after the commit (FoundationDB's `commit_unknown_result`), resending the same `commit_id` returns the original result. Nothing is applied twice.

This replaces the per-device `device_seq` gap rule from DESIGN-2 §2.3, so concurrent transactions from one device don't block each other.

### Log
| Method | Path | Purpose |
|---|---|---|
| POST | `/v1/log/append` | shorthand for a commit containing only `append` |
| GET | `/v1/log/{topic}?after=&limit=` | read from a cursor |
| WS | `/v1/stream` | `subscribe {topics \| prefixes, after}`, `unsubscribe`, push frames; ephemeral `publish` / `subscribe` |

### Consume (leader groups)
| Method | Path | Purpose |
|---|---|---|
| POST | `/v1/consume/lease` | `{group, topic, partition}` → `{token, expires}` (acquire or renew) |
| DELETE | `/v1/consume/lease` | release |
| GET | `/v1/consume/cursor` | current cursor (+ `last_commit_id`); for `per_key`: low watermark |
| POST | `/v1/consume/groups` | `{group, topic, mode: broadcast\|sequential\|partitioned\|per_key\|single_key, partitions?, key_token?, max_inflight?}` |

### Static
`GET /unencrypted/*`, the root aliases (`/`, `/favicon.ico`, …) and the SPA fallback (DESIGN-3 §4.2).

### Relayed services (milestone 5.5)
Handlers on member clients serve TCP connections the server relays blind ([SVC.md](SVC.md); normative: spec/api.md §14, spec/svc.md). Rights are topic rights on the service id: `consume` to handle, `append` to connect.

| Method | Path | Purpose |
|---|---|---|
| POST | `/v1/svc/info` | the service's mode, windows, `resume` and inbox key (§14.1) |
| POST | `/v1/svc/lease` | take, renew or release a handler lease (§14.2) |
| POST | `/v1/svc/next` | claim queued connections, long-polling (§14.3) |
| POST | `/v1/svc/state` | a held connection's cursors, state and pending records again (§14.4) |
| POST | `/v1/commit` (`svc`) | a step: ack input, append output, set the state, atomically with the rest of the commit (§14.5) |
| POST | `/v1/svc/read` | input from an offset (§14.6) |
| POST | `/v1/svc/reveal` | release committed output by its key (§14.7) |
| POST | `/v1/svc/release` | give a connection back, requeued or closed (§14.8) |
| WS | `/v1/svc/connect` | a `tcp-auth` connection, as a member (§14.9) |
| WS | `/v1/stream` | `svc_sub`: input and `svc_ready` pushed for a lease; `svc_reveal` (§14.10) |

### Integrity (milestone 6)
Device checkpoints, enrolled devices, signed fs headers and fenced leases ([INTEGRITY.md](INTEGRITY.md); normative: spec/api.md §4.4, §6, §15, §16; spec/formats.md §12.1, §14). The server stores checkpoints sealed and can't read them; it checks only who may put them and that each device's `seq` extends its head.

| Method | Path | Purpose |
|---|---|---|
| POST | `/v1/checkpoint/put` | a device's next checkpoint (`seq` = head + 1; idempotent for the same bytes); counts toward the fs quota (§15.1) |
| POST | `/v1/checkpoint/heads` | every device's head `seq`, or the named devices' (§15.1) |
| POST | `/v1/checkpoint/get` | a device's checkpoints by `seq` range (§15.1) |
| WS | `/v1/stream` | `cksub`: `ckh {device, seq}` pushed whenever a head moves (§9, §15.1) |
| POST | `/v1/devices/register` | register a device cert issued by the caller's member identity; at most 64 per user (§15.2) |
| POST | `/v1/devices/get` | registered devices' certs, for verifiers (§15.2) |
| POST | `/v1/fs/header/put` | a signed header must be the admin-signed successor of the stored one (§4.4; formats.md §12.1) |
| POST | `/v1/lease` | take, renew or release a named lease, with a token that grows on each new holder (§16) |
| POST | `/v1/commit` (`fence`) | fail with `claim_lost` unless the caller still holds the lease at that token, atomically with the rest of the commit (§6, §16) |
| POST | `/v1/fs/tree/changes` | `ops` and `chain` with a caught-up answer, so replicas tie their tree state to the op chain (§12) |

`/v1/info` lists the features `checkpoints`, `signed_header`, `lease` and `devices`, and the limit `checkpoint_keep`.

### Large files (milestone 8)
Page form 2 and upload sessions ([MILESTONE-8.md](MILESTONE-8.md); normative: spec/api.md §6, §12.1; spec/formats.md §11.6; spec/keyspace.md §3.6). Pages of form 2 carry each chunk's length, for content-defined chunks. The server counts one chunk reference per entry, as for form 1. An upload session keeps a long upload's chunks and pages past `chunk_grace_secs`, so the upload can resume days later.

| Method | Path | Purpose |
|---|---|---|
| POST | `/v1/fs/upload/open` | a new session of the caller's, `ttl_secs` default `upload_ttl_secs` (7 days), capped at `upload_max_ttl_secs` (30 days); at most 64 open per user and fs (§12.1) |
| POST | `/v1/commit` (`upload`) | every chunk and page the commit puts is referenced by the session, once per id; 412 `upload_gone` if it is closed, expired or not the caller's (§6, §12.1) |
| POST | `/v1/fs/upload/renew` | the expiry set to `ttl_secs` from now (§12.1) |
| POST | `/v1/fs/upload/close` | end it; the sweeper then releases its references in bounded rounds, each starting a grace period (§12.1) |
| POST | `/v1/fs/upload/list` | the caller's open sessions, to close the ones a lost client left behind (§12.1) |

Each session reference counts as one key toward the fs quota. `/v1/info` lists the features `fs_cdc` and `upload_sessions`, and the limits `upload_ttl_secs` and `upload_max_ttl_secs`.

---

## 2. Client library: `@zen/client` (TypeScript over the WASM core)

> **Superseded by [`spec/zendb.md`](../spec/zendb.md) for zen-db and events** (milestone 4.5): the class is `Db` from `zen.db(fs, ns)`, `db.transact` is `db.transaction`, `tx.publish` / `topic.publish` are `tx.emit` / `db.emit`, `topic.subscribe` is `db.on`, and `zen.consumer(…).run` is `db.consume`. The sketch below is kept as history.

```ts
// connect & unlock
const zen = await Zen.connect("https://family.example", { device });   // device key in IndexedDB / file
await zen.unlock({ passphrase })            // or { webauthn } | { recoveryKey }

// ---------- zen-db ----------
const db = zen.db("app", { fs: "/home/user" });       // namespace inside a filesystem
const users = db.table<User>("users", {
  pk: "id",
  indexes: { email: { unique: true }, age: { kind: "private" } }, // "private" (default) | "fast"
});

await users.get(id);                                      // auto-commit, single op
await users.put(user);
await users.query().where("age", ">=", 18).orderBy("age").limit(20).all();

// explicit transaction, auto-retried on conflict (the function may run more than once)
await db.transact(async (tx) => {
  const u = await tx.table(users).get(id);
  await tx.table(users).put({ ...u, credits: u.credits - 1 });
  tx.publish("audit", { kind: "credit_used", id });        // sent only if the commit succeeds
});

// manual transaction (no automatic retry)
const tx = db.begin({ mode: "auto" });                    // "short" | "long" | "auto"
...; await tx.commit();  // or tx.abort()

// ---------- events ----------
const chat = zen.topic("chat/family");
await chat.publish(msg);                                  // outside any transaction
const sub = chat.subscribe((ev) => render(ev), { after: savedCursor });

// single-leader consumer: the transaction is bound to the event
zen.consumer({ group: "billing", topic: "orders", partitions: 4 })
   .run(async (event, tx) => {
      const o = await tx.table(orders).get(event.data.orderId);
      await tx.table(orders).put({ ...o, status: "paid" });
      tx.publish("invoices", { orderId: o.id });        // causation_id = event.id, added automatically
   });                                                    // commit = writes + publishes + cursor advance

// or by hand, with the same guarantees
const tx2 = await db.begin({ consumes: event });
...; await tx2.commit();
await event.ack();   // skip / mark processed with no writes (commit with only `consume`)

// ---------- files ----------
const home = zen.fs("/home/user");
await home.writeFile("notes.txt", bytes); await home.readdir("/");
await home.transact(async (t) => { await t.rename("a", "b"); await t.writeFile("c", x); });

// ---------- CRDT ----------
const list = await openLoro(db, "shopping-list");      // @zen/loro: snapshot in a row, updates on a topic
list.doc.getList("items").push("milk"); list.doc.commit();

// ---------- admin ----------
await zen.admin.keyslots.add({ webauthn }); await zen.admin.acl.grant(user, "/home/user", ["read"]);
```


See DESIGN-4.md for event keys, consumer modes and server-side CRDT objects.
