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
| `per_ip_rate`, `per_ip_burst` | a token bucket of accepts per peer address, per node (for `tcp-auth`, the WebSocket client's address); `0` turns it off | 10 per s, 50 |
| `window` | unacked input bytes per connection (≥ 65,536) | 262,144 |
| `out_window` | committed, unwritten output bytes per connection (≥ 65,536) | 1,048,576 |
| `accept_wait_ms` | a connection not claimed within this time is aborted | 10,000 |
| `handshake_secs` | a connection with no committed step this long after accept is aborted | 30 |
| `idle_secs` | a connection with no input and no output for this long, and no bytes waiting, is aborted; `0` turns it off | 300 |
| `resume` | output is released by key reveal (§5), so another handler can take over (§6) | true |
| `inbox` | an X-Wing public key (hex, 1,216 bytes, formats.md §13.3): input is sealed at rest (§9). Another length is refused at start | none |

* **Rights** are topic rights on `id` (api.md §1, formats.md §3.3): `consume` to handle, `append` to connect over `tcp-auth`. Topic grants are prefix grants, so a grant on `svc/` covers every service of the fs.
* **The service key.** `K_svc = KDF("zen/v1/svc-aead", D_svc, "")`, where `D_svc` is the topic data key of `id` at the epoch (formats.md §3.3). It seals the state (kind 9) and the output-key escrows (kind 10). Giving a worker the topic keys of `svc/<name>` delegates exactly that service, as with events.
* **Timers.** Both abort with a reset (§3, step 10).
  * `handshake_secs`: "no committed step" means `ra` is still all zeros and there is no state.
  * `idle_secs`: bytes wait while input is unacked (`in_acked < in_head`) or output is unwritten (`written_to < out_head`). A waiting connection is stalled on the handler or the peer, not idle, so the timer doesn't fire.
* **Settings travel with the connection.** At accept, the owning node copies `mode`, `window`, `out_window` and `resume` into the connection record, so a step behaves the same on whichever node receives it.

## 3. Connection lifecycle

1. **Admission.** The node checks `max_conns` and the peer's token bucket before it writes anything; over either, it closes the socket.
   * For `tcp-auth`, the peer is a WebSocket on `/v1/svc/connect` (api.md §14.9). The node checks the token and topic `append` first, then admission, with the bucket of the WebSocket client's address; a refusal is 429 `quota`. Everything after admission is the same in both modes: **the connection is the WebSocket**, and the steps below read "socket" as the WebSocket. Losing the WebSocket loses the connection, as for TCP.
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
   * `tcp-auth`: each binary frame is input, and the text frame `"fin"` is the FIN. A frame larger than the room in the window is stored in parts: the rest waits in the node, and nothing more is read until it is stored. Any close from the client is a reset.
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
8. **Half-close.** A record with FIN shuts down the socket's write side once everything before it is written (`tcp-auth`: the node sends the text frame `"fin"`). The handler may still read and ack input; further records get `conn_closed`. Once the FIN record is written, the peer's EOF is stored and all input is acked, the node closes the socket and reaps (step 10).
9. **Close and abort.**
   * A record with close: once everything before it is written, the node reads and discards input for up to 1 s, so the kernel doesn't reset the last bytes, then closes the socket.
   * A record with abort: the node resets the socket (`SO_LINGER` 0) when it reaches the record.
   * `tcp-auth`: output goes in binary frames of at most 65,536 bytes. A close record sends close 1000, then waits up to 1 s for the client's close, discarding data. An abort, and every reset of step 10, sends close 4000. A connection that ends with FIN both ways sends close 1000.
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
   * `resumable` only when `resume` is on: a `resume: false` connection is never taken over (§6)
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
* **Records.** Each is stored at its offset: the first at `out_from`, each next one after the raw bytes of the one before. An empty record (FIN, close or abort alone) has no bytes, so it sits at the new `out_head` itself; it is released like any other record, and stays stored until the connection is reaped.
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

### 7.1 The TLS engine

The TLS engine (`engine: "tls"`) terminates TLS in the handler. The handshake runs through rustls; once it is done, the engine's own record layer takes over, and its state is the connection's `engine_state`.

**Versions and suites.**
* TLS 1.3 with `TLS_AES_128_GCM_SHA256`, `TLS_AES_256_GCM_SHA384` and `TLS_CHACHA20_POLY1305_SHA256`.
* TLS 1.2 with `TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256`, `TLS_ECDHE_ECDSA_WITH_AES_256_GCM_SHA384` and `TLS_ECDHE_ECDSA_WITH_CHACHA20_POLY1305_SHA256` only: ECDHE, an AEAD, and the extended master secret (RFC 7627) required. A 1.2 client without it is refused.
* Key exchange: `X25519MLKEM768` (1.3 only), `X25519` and `secp256r1`. The server's key is ECDSA (P-256, P-384) or Ed25519; RSA server keys are not supported (TD-TLS-RSA-SERVER-KEY).

**Refused or never offered.**
* No session tickets, no session-id resumption, no PSK and no early data: every connection does a full handshake.
* No renegotiation (1.2): a `HelloRequest`-style handshake record from the client gets a `no_renegotiation` warning and is otherwise ignored.
* No client certificates, and no post-handshake authentication. In 1.3, a post-handshake message other than `KeyUpdate` gets an `unexpected_message` alert.

**Resumable after the handshake.** A connection becomes resumable (§6) on the step that finishes the handshake. Until then there is no `engine_state`: rustls's handshake state can't be serialized, so a handler that dies mid-handshake leaves a connection that is aborted (TD-SVC-TLS-HANDSHAKE-RESUME). Handshake steps aren't idempotent either; a runtime that retries such a step's commit re-sends the same output, and aborts the connection when the step's outcome is unknown.

**Input and acks.**
* Input is acked only at TLS record boundaries (§6). An incomplete record stays unacked until the rest arrives; at EOF it is a truncation.
* Decrypted application data the application hasn't consumed is carried in the state. The engine stops decrypting (and acking) once it holds 16,384 bytes, so the state stays bounded: the unconsumed plaintext is at most 16,384 bytes plus one record's.
* A `close_notify` from the peer is remembered; input after it is ignored.

**Output.**
* Application data is sent in records of at most 16,384 plaintext bytes.
* **Key limits.** At the AES-GCM confidentiality limit, 2^24 records under one key, a 1.3 connection sends `KeyUpdate` (without requesting one) and continues under the new key; a 1.2 connection sends `close_notify` and refuses further writes. ChaCha20-Poly1305 has no limit here.
* **The 1.2 AES-GCM explicit nonce** (RFC 5288 leaves its construction open): a 32-bit prefix, drawn at random for each batch of records the engine sends, followed by the low 32 bits of the record's sequence number. Since the connection closes before 2^24 records, the explicit nonce is unique within a connection; a state rolled back (§12) reuses a sequence number with a fresh prefix. 1.2 ChaCha20-Poly1305 follows RFC 7905.
* **KeyUpdate.** The engine answers a peer's `KeyUpdate` that requests an update with its own, in the output of the same step (once per received request), and can send one on the application's request. A traffic secret is updated as RFC 8446 §7.2 says: `HKDF-Expand-Label(secret, "traffic upd", "", Hash.length)`.

**Errors.** A record that is too long (more than 2^14 + 256 ciphertext bytes in 1.3, 2^14 + 2048 in 1.2, or more than 2^14 plaintext bytes) gets `record_overflow`; a record that doesn't decrypt gets `bad_record_mac`; an unknown or unexpected content type gets `unexpected_message`. A fatal alert, received or sent, closes the connection: the engine sends the alert (encrypted after the handshake) and then a close record.

**The switch.** When the handshake is done, the engine takes the record keys and sequence numbers from rustls (`dangerous_into_kernel_connection`). For 1.3 it also captures the two application traffic secrets (`CLIENT_TRAFFIC_SECRET_0`, `SERVER_TRAFFIC_SECRET_0`, which `KeyUpdate` needs), re-derives the keys and IVs from them and checks they equal rustls's; a mismatch aborts the connection. Application data that arrived with the client's last handshake flight is decrypted by rustls and carried over as unconsumed plaintext.

**`engine_state`.** A CBOR map:
```
{ v: 1,
  ver: uint,                  // 0x0304 (TLS 1.3) or 0x0303 (TLS 1.2)
  suite: uint,                // the IANA cipher suite number
  tx: dir, rx: dir,           // the server's sending and receiving directions
  plain: bytes,               // decrypted application data not consumed yet (≤ 16,384 + 16,384)
  sni?: text,                 // the client's server name, if it sent one
  alpn?: bytes,               // the negotiated protocol, if any
  flags: uint }               // 1 peer_closed, 2 sent_close, 4 closed (after a fatal alert)
dir = { seq: uint,            // the next record's sequence number
        secret?: bytes,       // 1.3: the current traffic secret (32 or 48 bytes)
        key?: bytes, iv?: bytes }  // 1.2: the write key (16 or 32 bytes) and IV (12 bytes)
```
* For 1.2 AES-GCM, `iv`'s first 4 bytes are the salt and the rest is ignored; for ChaCha20-Poly1305 it is the 12-byte IV.
* A state whose `v`, `ver` or `suite` the engine doesn't know, or whose fields have the wrong lengths, is refused; the runtime then releases the connection with `close`.
* Keys are in the state, so a rotation doesn't protect a live TLS connection (§8).

### 7.2 The SSH engine

The SSH engine (`engine: "ssh"`) is an SSH-2 server (RFC 4251–4254) in the handler. It is its own implementation, without a library's hidden state, so its whole state serializes at every packet boundary, a key exchange in progress included.

**Algorithms.**
* Key exchange: `mlkem768x25519-sha256` (draft-ietf-sshm-mlkem-hybrid-kex) and `curve25519-sha256` (RFC 8731, also under its old name `curve25519-sha256@libssh.org`).
* Host key: `ssh-ed25519` (RFC 8709). One host key per service.
* Ciphers: `chacha20-poly1305@openssh.com` and `aes256-gcm@openssh.com`, both AEAD. Compression: `none`.
* MACs: `hmac-sha2-256-etm@openssh.com` and `hmac-sha2-256` are offered so that negotiation succeeds, but never used: with an AEAD cipher there is no MAC.
* User keys: `ssh-ed25519`, and `ssh-rsa` keys signing with `rsa-sha2-256` or `rsa-sha2-512` (RFC 8332), within the RSA key policy (a 2,048 to 4,096-bit modulus, an odd exponent of at least 65,537). `ssh-rsa` signatures (SHA-1) are refused, and so are other key types.

**The version exchange.** The engine sends `SSH-2.0-<software>` (default `zen`) and its first KEXINIT at once: the server speaks first. It accepts a client line `SSH-2.0-…` or `SSH-1.99-…` of at most 255 bytes with its CR LF (a bare LF is accepted too), after at most 20 other lines, which are ignored. The two version lines go into every exchange hash, so they stay in the state.

**Packets.**
* The binary packet protocol of RFC 4253 §6, with padding to 8 bytes (none and chacha20-poly1305) or 16 (aes256-gcm), at least 4 bytes of it. Padding is random.
* `chacha20-poly1305@openssh.com`: a 64-byte key; the first 32 bytes are the main key and the last 32 the header key. The nonce is the packet sequence number (64 bits, big-endian). The length is encrypted with the header key; the Poly1305 key is the first 32 bytes of the main key's block 0, and the payload is encrypted from block 1. The tag covers the encrypted length and payload.
* `aes256-gcm@openssh.com` (RFC 5647 with OpenSSH's names): a 32-byte key and a 12-byte IV, the last 8 bytes of which count packets (big-endian, incremented after each). The length is sent in the clear as additional data.
* A packet longer than 35,000 bytes (RFC 4253 §6.1's minimum), with less than 4 bytes of padding, with the wrong alignment or with an empty payload ends the connection (`PROTOCOL_ERROR`); one that doesn't authenticate ends it with `MAC_ERROR`.

**Key exchange.**
* KEXINIT negotiation as RFC 4253 §7.1. A client KEXINIT larger than 8,192 bytes is refused. A wrong guess (`first_kex_packet_follows`) makes the engine ignore the client's next packet.
* `curve25519-sha256`: the shared secret K is encoded as an mpint. `mlkem768x25519-sha256`: the client's init is its ML-KEM-768 encapsulation key (1,184 bytes) followed by its X25519 key (32); the server's reply is the ML-KEM ciphertext (1,088 bytes) followed by its X25519 key; K = SHA-256(ML-KEM shared secret ‖ X25519 shared secret), encoded as a string. Both refuse an all-zero X25519 result.
* The exchange hash and the keys are those of RFC 4253 §7.2 and §8 with SHA-256. The first exchange hash is the session id.
* **The server's ephemeral secrets never persist.** The engine computes its reply in the step that receives the client's init and forgets them; what persists is the new keys, the session id, and the client's keys waiting for its NEWKEYS.
* **Strict key exchange** (`kex-strict-s-v00@openssh.com`, against CVE-2023-48795) is on when the client's first KEXINIT offers `kex-strict-c-v00@openssh.com`. Then the client's KEXINIT must be its first packet; any message other than KEXINIT, the kex method's and NEWKEYS during the first key exchange ends the connection; and each direction's sequence number restarts at 0 after every NEWKEYS, in that direction. Without strict kex, IGNORE, DEBUG and UNIMPLEMENTED are accepted anywhere, and other unexpected messages during a key exchange get UNIMPLEMENTED.
* **EXT_INFO** (RFC 8308): right after its first NEWKEYS, if the client's first KEXINIT offers `ext-info-c`, the engine sends `server-sig-algs` = `ssh-ed25519,rsa-sha2-256,rsa-sha2-512`.
* **Rekeying.** A client KEXINIT starts a new key exchange at any time after the first. The engine starts one itself after 2^30 bytes in either direction, 2^31 packets, or an hour (the defaults) since the last one ended. From its KEXINIT to its NEWKEYS it sends only transport and key-exchange messages; other output waits in the state (below).
* **A key exchange the server started, behind unread data.** While the engine waits for the client's key-exchange messages and its input is held back by the read bound (below), it decrypts ahead through the input it was given, without consuming it, and processes the client's KEXINIT and init there. It then replies and sends NEWKEYS, remembering the two sequence numbers it handled; when in-order processing reaches them, the two packets are skipped. So output held during the key exchange never waits on input that waits on that output.

**Authentication** (RFC 4252), after the client's `ssh-userauth` service request:
* Only `publickey`. A `none` request, another method, a key the engine doesn't support, or a user name longer than 256 bytes gets USERAUTH_FAILURE listing `publickey`.
* A request without a signature asks the application whether the key may log in as the user: if so the answer is USERAUTH_PK_OK, else FAILURE.
* A signed request is verified first, over the session id as RFC 4252 §7 says; a bad signature gets FAILURE without asking. Then the application is asked: USERAUTH_SUCCESS or FAILURE.
* While the application hasn't answered, the engine reads no further input; the question is in the state, so a successor asks its own application.
* After 20 requests without success the engine ends the connection (`NO_MORE_AUTH_METHODS_AVAILABLE`). Requests after success are ignored.

**Connection** (RFC 4254), after authentication:
* Only `session` channels, at most 10 open at once; others are refused (`UNKNOWN_CHANNEL_TYPE`, or `RESOURCE_SHORTAGE` at the limit). Global requests get REQUEST_FAILURE when a reply is wanted, so no forwarding.
* Each channel takes one `exec`, `shell` or `subsystem` request; the command or subsystem name reaches the application once, with the request, and the engine keeps only which request it was. A second one, and `pty-req`, `env`, `x11-req`, `auth-agent-req@openssh.com` or any other request, gets CHANNEL_FAILURE when a reply is wanted.
* The application writes standard output, and standard error as extended data of type 1; it sends `exit-status`, EOF and CLOSE. A CLOSE from the client is answered with CLOSE, and what was queued for that channel is dropped. Extended data from the client is discarded.
* Every channel is offered the window `window` (an engine option, default 98,304 bytes) and a maximum packet of 32,768 bytes; data beyond either ends the connection. The engine sends WINDOW_ADJUST once the application has consumed half the window. The channels' windows together, plus 64 KiB, should fit the service's `window` (§2), so that a client's data in flight and its key-exchange messages fit the relay's input at once: 10 × 98,304 + 65,536 is about 1 MiB, the window SSH services should use.

**Bounds.** The state holds at most:
* **Unread channel data:** 32,768 bytes across channels. The engine reads a packet only when nothing is unread, or when what is unread plus the packet's length stays within 32,768; otherwise it stops there, and its input ack with it (as TLS does at 16,384, §7.1).
* **Queued output:** 16,384 bytes across channels, in at most 64 pieces (a piece is a run of one channel's data or standard error). A write takes what fits the peer's window now, plus what fits the queue, and says how much it took; the application keeps the rest, in its own state or by not acking its own input. The queue drains as the peer adjusts its window and after a key exchange.
* **Other held messages:** while the engine's own key exchange is in progress, the replies it can't send yet (authentication answers, channel confirmations and replies), at most 32 messages and 4,096 bytes; with that much held, it reads no further input until the key exchange is done.
* So an `engine_state` stays under 64 KiB, leaving room in `max_value_bytes` for the application's state.

**Resumable from the first step.** Nothing in the engine is lost between packets, so the first step sets `resumable`, and a successor continues at any point: during the version exchange, during a key exchange or a rekey, during authentication, or mid-channel.

**Errors.** A protocol violation sends DISCONNECT with RFC 4253 §11.1's reason code (`PROTOCOL_ERROR`, `KEY_EXCHANGE_FAILED`, `MAC_ERROR`, `SERVICE_NOT_AVAILABLE`, `PROTOCOL_VERSION_NOT_SUPPORTED`, `NO_MORE_AUTH_METHODS_AVAILABLE`) and ends the connection; the runtime closes it after sending that output. A DISCONNECT from the client, or its EOF, ends it too. An unknown message type gets UNIMPLEMENTED.

**`engine_state`.** A CBOR map; unknown fields are refused:
```
{ v: 1,
  vs: bytes,                  // the server's version line, without CR LF
  vc?: bytes,                 // the client's, once received
  lines: uint,                // lines ignored before it
  sid?: bytes,                // the session id, after the first key exchange
  strict: bool,               // strict key exchange
  tx: dir, rx: dir,           // the server's sending and receiving directions
  rx_next?: keys,             // the client's next keys, until its NEWKEYS
  kex?: kex,                  // a key exchange in progress
  kex_at: uint,               // when the last one ended (Unix seconds)
  kexes: uint,                // key exchanges completed
  auth: { state: uint,        // 0 before the service request, 1 accepted, 2 authenticated
          tries: uint, user?: text,
          ask?: { user: text, alg: text, key: bytes, signed: bool } },
  ch: [chan],
  held: [bytes],              // messages waiting for the server's NEWKEYS
  flags: uint }               // 1 the peer ended, 2 the engine ended
dir  = { keys?: keys, seq: uint, bytes: uint, packets: uint }   // bytes and packets since the keys were set
keys = { c: uint,             // 1 chacha20-poly1305, 2 aes256-gcm
         key: bytes,          // 64 or 32 bytes
         iv?: bytes }         // aes256-gcm: 12 bytes, its counter included
kex  = { ours?: bytes, theirs?: bytes,   // the KEXINIT payloads, until the reply
         alg?: uint, c2s?: uint, s2c?: uint,  // negotiated: 1 mlkem768x25519, 2 curve25519; ciphers as keys.c
         replied: bool, skip: bool,       // skip: ignore the client's next packet (a wrong guess)
         ahead?: [uint, uint] }           // sequence numbers handled ahead of order
chan = { id: uint, peer: uint, peer_window: uint, peer_max: uint,
         window: uint,        // what the client may still send
         unread: bytes,
         queue: [[uint, bytes]],          // 0 data, 1 standard error
         req?: uint,                      // 1 exec, 2 shell, 3 subsystem (the command or name is in the event only)
         exit?: uint, flags: uint }       // 1 EOF in, 2 CLOSE in, 4 EOF wanted, 8 EOF sent, 16 CLOSE wanted, 32 CLOSE sent
```
* A state whose `v` the engine doesn't know, or whose fields have the wrong types or lengths or exceed the bounds above, is refused; the runtime then releases the connection with `close`.
* Keys are in the state, so a rotation doesn't protect a live SSH session (§8); a handler can rekey it.

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
  * `in_head`, the input window, reads (`max_bytes`, api.md §14.6) and the fs byte quota all count plaintext bytes. The 17 bytes a sealed chunk adds (the sealed bit and the tag) aren't counted, as a record's framing isn't.
  * A batch re-sent after `commit_unknown` (§3, step 3) is the same bytes at the same offset under the same key, so it is the same chunk.
* The handler opens chunks with the inbox's decapsulation seed, which the admin keeps sealed in the fs. It checks that chunks are contiguous (each one's offset is the previous offset plus its length). The nonce binds the offset, so a chunk moved to another offset doesn't open.
* **A handler without the seed** can't serve the service. It releases a connection whose claim carries `inbox_ct` with `close` (§6).
* **Turning sealing off** (`unseal`, for example after STARTTLS, when the input becomes TLS ciphertext). A step sets the sticky `unseal` flag in `ra`.
  * The node notices it when it stores a batch: the batch's transaction reads `ra` (by snapshot), and from the first batch that sees the flag on, the node stores plain chunks. A batch it was already storing stays sealed.
  * Handlers accept both kinds at any time, so no coordination is needed.
  * `unseal` on a service without `inbox` sets the flag and changes nothing else.
* **The limit.** The inbox protects input **at rest**: in the keyspace, in backups, in exports. It doesn't protect it from the server process, which receives the plaintext from the socket in transit.

## 10. Liveness and wake-ups

* **Handler leases** (`/v1/svc/lease`). A lease id is 16 bytes chosen by the handler. Renewals keep the token-free lease alive; the default TTL is 5 s, and handlers renew every second.
  * A lease renewal and `/v1/svc/next` both reap the expired leases of the service: they requeue or abort each lease's connections (§6) and ring the queue doorbell.
  * `next` caps its wait at the earliest expiry among the service's leases, so a takeover waits about the TTL, not the long-poll timeout. The sweeper is the backstop.
  * A released lease is reaped at once.
* **Doorbells.** These are keys whose value is a versionstamp, rewritten to wake whoever watches them. There are three kinds:
  * **The service queue `rqh`:** rung by accepts and requeues; `next` waits on it.
  * **A handler lease `rld`:** rung by input on its connections, by claims under it, by `written_to` advancing, and by a reap; `/v1/svc/read` and stream pushes wait on it. Input and claims also append a short-lived entry to the lease's input log `rli`, so a stream push reads only the connections that changed. Ending a lease deletes its `rld` (which wakes its watchers) and its `rli`.
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
    * TLS 1.2 AES-GCM explicit nonces carry a 32-bit prefix drawn at random for each batch of records (§7.1), so a rolled-back sequence number repeats a nonce only with probability 2^-32 per batch
  * The fix is milestone 6's handler-signed checkpoint, carrying `(conn, out_head)`.
  * Honest failures never cause reuse: an uncommitted record is never revealed.
