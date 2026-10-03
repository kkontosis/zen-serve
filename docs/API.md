# zen-serve: API outline (draft)

Based on DESIGN.md, DESIGN-2.md and DESIGN-3.md. All names are provisional.

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
  append?:  [{topic, envelope}],              // events published only if this commits
  consume?: [{group, topic, partition, from, to, lease_token}],
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
| GET | `/v1/consume/cursor` | current cursor (+ `last_commit_id`) |

### Static
`GET /unencrypted/*`, the root aliases (`/`, `/favicon.ico`, …) and the SPA fallback (DESIGN-3 §4.2).

---

## 2. Client library: `@zen/client` (TypeScript over the WASM core)

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
const doc = await zen.loro("docs/shopping-list");       // snapshot in KV, updates on a topic

// ---------- admin ----------
await zen.admin.keyslots.add({ webauthn }); await zen.admin.acl.grant(user, "/home/user", ["read"]);
```
