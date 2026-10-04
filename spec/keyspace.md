# Server keyspace (storage layout)

The layout zen-serve uses in its ordered key-value store. Both backends use exactly these bytes: the embedded backend (`zen-store`, milestone 2) and FoundationDB (milestone 3). So a dump from one can be loaded into the other.

## 1. Encoding

* **Tuple encoding.** Keys are packed with the [FoundationDB tuple layer](https://github.com/apple/foundationdb/blob/main/design/tuple.md) encoding. Only this subset is used:

  | Type | Code | Encoding |
  |---|---|---|
  | byte string | `0x01` | bytes, every `0x00` escaped as `0x00 0xFF`, then a `0x00` terminator |
  | text string | `0x02` | UTF-8, escaped and terminated like a byte string |
  | integer | `0x0c`–`0x1c` | `0x14` for zero; `0x14 + n` then n big-endian bytes for positive values; `0x14 − n` then the ones' complement for negative values (n ≤ 8) |
  | versionstamp | `0x33` | 12 bytes: the 10-byte commit versionstamp, then a 2-byte big-endian user version |

  The encoding preserves order: tuples sort the same way as their packed bytes.
* **`pack(a, b, …)`** is the packed tuple. **`pack(…) ‖ raw`** appends raw bytes after a packed prefix. Raw suffixes are used for client-chosen keys, so range bounds map one-to-one.
* **Prefix ranges.** `range(p) = [p, strinc(p))`, where `strinc` drops trailing `0xFF` bytes and increments the last byte.

## 2. Versions, versionstamps, offsets

* **Version** (`u64`). A commit version from a clock running at about 1,000,000 versions per second. Every commit gets one, and versions strictly increase. The embedded backend computes `max(last + 1, unix_micros)`. Leases and claims expire at a version (DESIGN-3 §3.1).
* **Versionstamp** (10 bytes). `u64(version) ‖ u16(batch_order)`. The embedded backend always uses batch order 0.
* **Offset** (12 bytes). An event's position: `versionstamp ‖ u16(index)`, where `index` is the append's position in the commit's `append` list. Offsets are unique, totally ordered across an fs and strictly increasing in commit order.
  * The zero offset (12 × `0x00`) means "before the first event".
* **Value version.** Every KV value and fs header is stored as `versionstamp(10) ‖ payload`. The versionstamp is written by the backend at commit time (FDB: `SET_VERSIONSTAMPED_VALUE`). It is the `version` clients `expect` (API §5).

## 3. Layout

`fs` is the `fs_id` integer. `topic` is a topic id (16·n bytes), `key` an event key token (16 bytes), `group` a consumer-group name (1–64 bytes, client-chosen and preferably a token), `vs` a 12-byte versionstamp element and `part` a partition integer.

### 3.1 KV and fs headers

| Key | Value |
|---|---|
| `pack("kv", fs) ‖ stored_key` | `versionstamp ‖ sealed value` (formats.md §4) |
| `pack("hdr", fs)` | `versionstamp ‖ header blob` (volume header + keyslots, opaque to the server) |
| `pack("q", fs, "bytes")` | `i64` little-endian, atomic add: bytes used by KV values and events |
| `pack("q", fs, "keys")` | `i64` little-endian, atomic add: number of KV keys |

Quota counters are read with snapshot reads, so concurrent commits don't conflict on them. That makes them approximate under heavy concurrency, which is fine for quotas.

### 3.2 Event log

| Key | Value |
|---|---|
| `pack("log", fs, topic, vs)` | event entry (below) |
| `pack("lk", fs, topic, key, vs)` | empty: per-key index (DESIGN-4 §1.1) |
| `pack("lh", fs, topic)` | `versionstamp` of the last append; watched to wake topic subscribers |
| `pack("gl", fs, vs)` | `topic`: fs-wide index in offset order, used by prefix subscriptions |
| `pack("gh", fs)` | `versionstamp` of the last append in the fs; watched to wake prefix subscribers |

**Event entry:** `u8 flags ‖ [key_token(16) if flags & 1] ‖ envelope`. The envelope is the sealed event (formats.md §4, kind 2), stored verbatim.

### 3.3 Consumer groups

| Key | Value |
|---|---|
| `pack("cg", fs, group)` | group definition, CBOR (API §8.1) |
| `pack("ct", fs, topic, group)` | `u8 mode`: index of the groups on a topic, read on append |
| `pack("cc", fs, group, part)` | `offset`: committed cursor (`sequential` uses part 0; `single_key` uses part 0) |
| `pack("cl", fs, group, part)` | lease: `holder_fp(32) ‖ u64 token ‖ u64 expires_version` |
| `pack("kc", fs, group, key)` | `offset ‖ u64 last_claim_token`: last committed offset for one key (`per_key`) |
| `pack("kr", fs, group, vs, key)` | empty: the ready list, oldest pending event first (`per_key`) |
| `pack("kp", fs, group, key)` | `offset` of the key's entry in the ready list |
| `pack("km", fs, group, key)` | claim: `holder_fp(32) ‖ u64 token ‖ u64 expires_version ‖ offset` |
| `pack("ca", fs, group, sub)` | attempts: `offset ‖ u32 count`, where `sub` is the partition integer or the key token bytes |
| `pack("dlq", fs, group, vs)` | dead-letter entry: `offset(12) ‖ lp(topic) ‖ event entry` |

Mode bytes: 1 `broadcast`, 2 `sequential`, 3 `partitioned`, 4 `per_key`, 5 `single_key`.

### 3.4 Commits, ACL, server metadata

| Key | Value |
|---|---|
| `pack("cid", commit_id)` | idempotency record: `versionstamp ‖ u16 appended_count ‖ device_fp(32)` |
| `pack("cix", vs, commit_id)` | empty: expiry index for idempotency records, oldest first |
| `pack("acl", version)` | signed ACL (formats.md §9), every version kept (the membership log) |
| `pack("acl_head")` | `u64 version` of the current ACL |
| `pack("meta", name)` | backend-private metadata, e.g. `"version"` in the embedded backend |

Sessions, challenges and ephemeral subscriptions are kept in memory, not in the keyspace.
