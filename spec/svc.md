# Relayed services

zen-serve can host internet protocols (SSH, SMTP, IMAP, any TLS service) **without seeing their plaintext**. The server is a blind TCP relay. It accepts connections and stores each connection's bytes in a short-lived, ordered queue. **Handlers** run on member clients that hold the fs keys. A handler claims a connection, reads its input, serves the protocol and writes its output.

Each **step** of a handler acks input, appends output and saves the protocol state. It commits in the same transaction as the handler's other writes (zen-db rows, events, consume steps; api.md §6). When a handler dies, another one **takes over the live connection** from the last committed step. Nothing is retained: input is deleted once acked, and output once written to the socket.

The wire contract is api.md §14, the storage layout keyspace.md §3.9, and the byte formats formats.md §13 (with seal kinds 9 and 10, formats.md §4).

## 1. Roles

* **Relay.** The zen-serve node that owns a connection's socket: the **owning node**. Only it reads and writes the socket. Any node serves the handler API, because everything passes through storage.
* **Handler.** A member client (a Node worker or a browser tab) with topic `consume` on the service id. It holds a **handler lease** and claims connections under it.
* **Connector** (`tcp-auth` only). A member client with topic `append` on the service id, connected over a WebSocket (api.md §14.9) instead of a public port.
* **Peer.** The remote end of a `tcp` connection, or the connector.

## 2. Services

A service is a `[[service]]` table in the server configuration. **Every node carries the same table**; only `listen` may differ between nodes.

| Field | Meaning | Default |
|---|---|---|
| `name` | unique name, for logs and metrics | required |
| `fs` | the fs the service belongs to | required |
| `id` | the service id, a topic id (16·n bytes, hex), conventionally the topic `svc/<name>` | required |
| `mode` | `tcp`: a public port. `tcp-auth`: connections come from members over `/v1/svc/connect` | required |
| `listen` | the address to accept on (`tcp` only; refused for `tcp-auth`) | required for `tcp` |
| `max_conns` | open connections per node; further accepts are closed at once | 1,024 |
| `per_ip_rate`, `per_ip_burst` | a token bucket of accepts per peer address, per node (`tcp` only); `0` turns it off | 10 per s, 50 |
| `window` | unacked input bytes per connection (≥ 65,536) | 262,144 |
| `out_window` | committed, unwritten output bytes per connection (≥ 65,536) | 1,048,576 |
| `accept_wait_ms` | a connection not claimed within this time is aborted | 10,000 |
| `handshake_secs` | a connection with no committed step this long after accept is aborted | 30 |
| `idle_secs` | a connection with no input and no output for this long is aborted; `0` turns it off | 300 |
| `resume` | output is released by key reveal (§5), so another handler can take over (§6) | true |
| `inbox` | an X-Wing public key (hex, formats.md §13.3): input is sealed at rest (§9) | none |

* **Rights** are topic rights on `id` (api.md §1, formats.md §3.3): `consume` to handle, `append` to connect over `tcp-auth`. Topic grants are prefix grants, so a grant on `svc/` covers every service of the fs.
* **The service key.** `K_svc = KDF("zen/v1/svc-aead", D_svc, "")`, where `D_svc` is the topic data key of `id` at the epoch (formats.md §3.3). It seals the state (kind 9) and the output-key escrows (kind 10). Giving a worker the topic keys of `svc/<name>` delegates exactly that service, as with events.
* **Settings travel with the connection.** At accept, the owning node copies `mode`, `window`, `out_window` and `resume` into the connection record, so a step behaves the same on whichever node receives it.

## 3. Connection lifecycle

1. **Admission** (`tcp`). The node checks `max_conns` and the peer's token bucket before it writes anything; over either, it closes the socket.
2. **Accept.** The node picks a **connection id** (16 random bytes) and writes, in one transaction:
   * the connection record `rc` (its node, the peer, the time, the settings, the inbox ciphertext)
   * the counters `rh`, `ra`, `rw` at zero
   * the claim `rm` with token 0
   * an entry in the service's **ready queue** `rq`, oldest first
   * its index entry `rnc`

   It then rings the queue doorbell `rqh`.
3. **Input.** The node reads the socket in batches (up to 32 KiB, or what arrived within 5 ms) and appends each as an input chunk `ri` at its byte offset, advancing `in_head` in the same transaction.
   * It stops reading while `in_head − in_acked ≥ window`. That is TCP backpressure: the peer's writes block.
   * It also stops reading while the fs is over its byte quota (keyspace.md §3.1). This is backpressure too, not an error.
   * A FIN from the peer sets the EOF flag in `rh`. A reset from the peer ends the connection (step 10).
   * If the transaction returns `commit_unknown`, the node re-reads `rh` before it re-sends a batch, so a batch is never appended twice.
   * The node reads the claim `rm` (not by snapshot) and rings the doorbell of the lease holding the connection. So a handler claiming concurrently is never left asleep.
4. **Claim.** A handler holds a lease (§10) and claims queued connections with `/v1/svc/next`.
   * A claim sets `rm` to the lease, the handler's device and the token plus 1.
   * It returns the connection's cursors, its sealed state and the escrows of its committed, unrevealed output records.
   * A claim reads the step cursors `ra` normally, not by snapshot. So a late step of a previous holder conflicts with it, instead of leaving the new holder with stale state.
5. **Steps** (§4). The handler reads input (`/v1/svc/read`, or pushed over `/v1/stream`), runs the protocol, and commits a step with its other writes.
6. **Reveal** (§5). After a commit is known to have succeeded, the handler reveals the keys of its output records.
7. **Writing.** The owning node writes revealed records to the socket in order from `written_to`, and plain records as soon as they commit.
   * After each `write_all`, one transaction clears the written records and their keys and advances `written_to`. It also rings the holder's lease doorbell, which wakes a handler waiting on `window`.
   * A record that isn't revealed yet blocks the records after it.
8. **Half-close.** A record with FIN shuts down the socket's write side once everything before it is written. The handler may still read and ack input; further records get `conn_closed`.
9. **Close and abort.**
   * A record with close: once everything before it is written, the node reads and discards input for up to 1 s, so the kernel doesn't reset the last bytes, then closes the socket.
   * A record with abort: the node resets the socket (`SO_LINGER` 0) when it reaches the record.
   * Steps after either get `conn_closed`.
10. **End.** When the socket closes, for any reason, the node **reaps** the connection in one transaction:
    * deletes all of its keys
    * subtracts its bytes from the quota
    * rings its holder's lease doorbell

    Reasons include the end of the protocol, a reset or a failed write to the peer, an abort, and a timeout (`accept_wait_ms`, `handshake_secs`, `idle_secs`). A step or read on a reaped connection gets 412 `conn_closed`.
11. **Abort requests from elsewhere.** A node that is not the owner (a reveal with a bad key, a requeue of a connection that can't resume, §6) writes the abort request `rz` and rings the owning node's doorbell, and the owner resets and reaps. A step with an abort request pending gets `conn_closed`.
12. **Node death.** A socket dies with its node; that's inherent to TCP. Each node holds a node lease `rk` under a node id chosen at each start, and renews it every second; it expires after 10 s. The sweeper of any node reaps the connections (`rnc`) of an expired node lease, then deletes the lease.

## 4. The step

A step is one entry of a commit's `svc` list (api.md §14.5): `{fs, svc, conn, token, in_from, in_to, out_from, out?, state?, resumable?, unseal?}`. It applies atomically with the rest of the commit, after the consume steps and before the appends. The server checks, in order:

1. **Request shape** (400). Each record parses (formats.md §13.1), and:
   * a record carries no flag after a FIN, close or abort earlier in the list
   * an empty record carries FIN, close or abort
   * a record's raw length is at most `max_value_bytes − 130`
   * the records match the service: sealed when `resume` is on, plain when it's off
   * `state` is at most `max_value_bytes`
   * `in_from ≤ in_to`
   * there is at most one step per connection in the commit
2. **The claim** (412 `claim_lost`). `rm` names this `token`, the caller's device as holder, and a non-zero lease. `rm` is read normally, so a concurrent claim or reap conflicts with the step. A connection that no longer exists, or has an abort request pending, gets 412 `conn_closed`.
3. **The cursors** (412 `cursor_moved`). `in_from` equals `in_acked`, and `out_from` equals `out_head`.
4. **The input** (412 `cursor_moved`). `in_to ≤ in_head`, by a snapshot read of `rh`. `in_head` only grows, so a stale read only refuses early.
5. **Closing** (412 `conn_closed`). After a FIN, the step may not carry records. After close or abort, no step is accepted.
6. **The output window** (429 `window`). `out_head + (raw bytes of out) − written_to ≤ out_window`, by a snapshot read of `rw`. `written_to` only grows. The handler waits on its lease doorbell, which rings when `written_to` advances.
7. **The state rule** (400). Once a connection has a state, a step that moves either cursor must carry a new state. The state's authenticated data binds the cursors (formats.md §4, kind 9), so an old state no longer opens.

Then it applies:
* **Input.** Chunks wholly below `in_to` are cleared. A chunk that spans `in_to` stays, and its start becomes `in_cleared`. The server finds it with a snapshot reverse read of one key below `in_to`; chunks are immutable, so that read can't be stale.
* **Cursors.** `ra` gets the new `in_acked = in_to`, `in_cleared`, `out_head` (advanced by the records' raw lengths), the state length, and the flags. FIN, close and abort come from the records; `resumable` and `unseal` are sticky once set.
* **Records.** Each is stored at its offset: the first at `out_from`, each next one after the raw bytes of the one before.
* **State.** `rs` is replaced when `state` is present.
* **Quota.** The fs byte counter changes by the output's raw bytes, minus the input cleared, plus the change in state length.
* **Wake-ups.** The owning node's doorbell `rn` rings, with a work entry `rnq` for the connection, because new records or a moved `in_acked` may unblock it.

A step with no records and no input to ack is valid (it saves state, or sets `resumable` or `unseal`).

## 5. Release by key reveal

When `resume` is on, an output record is sealed under a key `k_r` of its own: 32 random bytes per record **per commit attempt** (formats.md §13.1). The record also carries `key_hash = BLAKE3.derive_key("zen/v1/svc-reveal", k_r)`, and an escrow of `k_r` sealed under `K_svc` (kind 10). The server stores the record without being able to read it.

* **Reveal after a known commit.** Only after the commit is known to have succeeded does the handler send `k_r` (`/v1/svc/reveal`, or `svc_reveal` on the stream). After a `commit_unknown`, it first resends the commit with the same `commit_id` (api.md §6).
* **The server's check.** `key_hash` must match, then the record's AEAD must open. The server stores the key next to the record (`rv`), and the owning node decrypts at write time. A wrong key is refused. From the claim holder, a key that fails the AEAD means the record can never be written, so the connection is aborted.
* **Why.** Only committed bytes ever reach the wire. A step whose commit fails is rebuilt, and may reuse the protocol's sequence numbers: TLS and SSH records of the failed attempt were never readable by anyone, so the reuse is harmless.
* **A `commit_id` fixes its records.** Resending a `commit_id` returns the stored result without applying the body again (api.md §6), so the records and keys of a `commit_id` must not change.
  * A rebuilt step uses a new `commit_id`.
  * A handler that gives up on an unknown outcome never reveals that attempt's keys. It recovers with `/v1/svc/state`. If the commit did apply, its records are listed as pending, and their keys come from their escrows.
* **Re-reveal.** A new holder, or a holder re-syncing, opens the escrows of pending records and reveals them. Revealing a committed record is always safe, so reveals need topic `consume` but no claim token.
* **`resume: false`.** Records are plain. The owning node writes them as soon as they commit, fenced only by the claim token. There's no reveal, and a connection whose claim lapses after its first step is aborted (§6).

## 6. Takeover

When a lease is released or expires (§10), each of its connections is either requeued or aborted. It is requeued when it is **fresh** (`in_acked = 0` and `out_head = 0`: no step has done anything yet) or **resumable** (a step set `resumable`). Every other connection is aborted.

A new holder:
1. claims the connection, getting the cursors, the state and the pending escrows
2. checks that the state opens with the cursors it was given (formats.md §4, kind 9)
3. reveals the pending records (§5)
4. reads input from `in_acked` and continues

* **Ack only at boundaries.** A handler acks input only at the end of a complete protocol unit: a TLS record, an SSH packet, or what the application has fully consumed. So the state holds sequence numbers and bounded application data, never half a record.
* **When a connection becomes resumable is the engine's call.** TLS sets `resumable` after the handshake: a handshake in progress can't be serialized, so a handler that dies mid-handshake leaves a connection that is aborted. The SSH engine persists its key exchange and sets it from the first step.
* **Rollback.** A handler remembers, per connection, the highest `(in_acked, out_head)` it has seen in this process. If a claim returns lower values, it aborts the connection (§12).

## 7. Engine state

The plaintext of a kind-9 state is a CBOR map `{v: 1, engine: text, engine_state: bytes, app: bytes}`:
* `engine` is `"raw"`, `"tls"` or `"ssh"`.
* `engine_state` is the engine's own versioned encoding, defined with the engine.
* `app` is the application's state.

The whole state is at most `max_value_bytes`. An engine that must queue output (for example while an SSH channel's window is closed) bounds the queue by holding back its input acks.

## 8. Epochs and delegation

* A handler seals the state and the escrows under the newest epoch it holds; `key_epoch` is in the sealed header (formats.md §4).
* A handler that holds the fs keys walks the epoch chain (formats.md §2) to open older states.
* **A delegated worker** that holds only the service's keys can't walk the chain, so its delegation carries `K_svc` (or the topic keys `D_svc`) for the current epoch and the one before. A successor that can't open a state's epoch aborts the connection (`/v1/svc/release` with `close`).
* **A rotation doesn't protect live sessions.** A TLS connection's traffic secrets are in its state, and TLS 1.3's key update is derived from them. Handlers SHOULD close TLS connections whose state is sealed under an epoch older than the current one, and SHOULD rekey SSH connections in that case (a fresh key exchange, which needs no old secret).

## 9. Sealed inbox

Without `inbox`, input is stored as it arrives. For TLS and SSH that is ciphertext; for a plaintext protocol (SMTP before STARTTLS), it is plaintext at rest.

With `inbox`:
* The owning node encapsulates once per connection to the service's X-Wing key. The ciphertext goes in the connection record.
* The node seals every input chunk under the derived key (formats.md §13.3). Chunks carry a sealed bit, and offsets count plaintext bytes.
* The handler opens chunks with the inbox's decapsulation seed, which the admin keeps sealed in the fs. It checks that chunks are contiguous (each one's offset is the previous offset plus its length).
* **Turning sealing off** (`unseal`, for example after STARTTLS, when the input becomes TLS ciphertext). A step sets the sticky `unseal` flag. Chunks the node seals before it notices are still sealed. Handlers accept both kinds at any time, so no coordination is needed.
* **The limit.** The inbox protects input **at rest**: in the keyspace, in backups, in exports. It doesn't protect it from the server process, which receives the plaintext from the socket in transit.

## 10. Liveness and wake-ups

* **Handler leases** (`/v1/svc/lease`). A lease id is 16 bytes chosen by the handler. Renewals keep the token-free lease alive; the default TTL is 5 s, and handlers renew every second.
  * A lease renewal and `/v1/svc/next` both reap the expired leases of the service: they requeue or abort each lease's connections (§6) and ring the queue doorbell.
  * `next` caps its wait at the earliest expiry among the service's leases, so a takeover waits about the TTL, not the long-poll timeout. The sweeper is the backstop.
  * A released lease is reaped at once.
* **Doorbells.** These are keys whose value is a versionstamp, rewritten to wake whoever watches them. There are three kinds:
  * **The service queue `rqh`:** rung by accepts and requeues; `next` waits on it.
  * **A handler lease `rld`:** rung by input on its connections, by `written_to` advancing, and by a reap; `/v1/svc/read` and stream pushes wait on it. Input also appends a short-lived entry to the lease's input log `rli`, so a stream push reads only the connections that changed.
  * **A node `rn`:** rung by steps, reveals and abort requests, each with a work entry `rnq`; the owning node's writer waits on it and clears exactly the entries it read.
* **No watch per connection.** Storage watches are limited (about 10,000 per FoundationDB client), so the number of watches grows with nodes, services and handler leases, never with connections.

## 11. Failures

| What fails | What happens |
|---|---|
| no handler | the connection is aborted after `accept_wait_ms` |
| a handler, before its first step | the connection is fresh and is requeued |
| a handler, mid-step (no commit) | the step never happened; the successor redoes it from the same input |
| a handler, after a commit and before its reveal | the successor reveals the pending records from their escrows |
| a handler, on a connection that isn't resumable (TLS mid-handshake, `resume: false` after a step) | the connection is aborted |
| the owning node | its sockets are lost; its connections are reaped once its node lease expires |
| storage, briefly | the node stops reading the sockets and retries; handlers retry their commits (`commit_unknown`: same `commit_id`) |
| a slow peer | input: the window fills and the peer's writes block. Output: `out_window` fills and steps get `window` |
| a slow handler | the window fills and the peer's writes block; `idle_secs` doesn't fire while bytes wait |
| a reveal lost | the record stays pending; any later claim or `/v1/svc/state` lists it |
| a reveal repeated | a no-op (api.md §14.7, result 1) |
| a bad record (the AEAD fails at reveal) | the connection is aborted |

## 12. What the server sees

* **Always:**
  * peer addresses (`tcp`), or the connector's user and device (`tcp-auth`)
  * connection times, sizes and timing
  * which handler device holds which connection
  * the input bytes, in transit
* **At rest:**
  * the input until it is acked. That's ciphertext for TLS and SSH; for plaintext protocols it is plaintext, unless the inbox seals it (§9).
  * output records, sealed until revealed. The key is then stored next to the record until it is written.
  * the state, sealed (kind 9), and its length
* **Never:** a handler's keys, the state's contents, or output before its commit is known.
* **Rollback (TD-SVC-STATE-ROLLBACK).** A malicious server can serve an older but consistent snapshot of a connection: `ra`, `rs`, pending records and the handler's database, as with any stored value (zendb.md §14).
  * The engine then reuses record sequence numbers. If its new output differs (timestamps, randomness), the server sees two TLS or SSH records under one nonce.
  * **Mitigations:**
    * a handler aborts on a cursor regression it can see (§6)
    * TLS 1.2 AES-GCM uses random 64-bit explicit nonces (RFC 5288), which a rollback can't repeat
  * The fix is milestone 6's handler-signed checkpoint, carrying `(conn, out_head)`.
  * Honest failures never cause reuse: an uncommitted record is never revealed.
