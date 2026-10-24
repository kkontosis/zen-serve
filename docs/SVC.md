# Relayed services

A guide for whoever runs a relayed service or writes its handler. The normative description is [spec/svc.md](../spec/svc.md), with the requests in [spec/api.md §14](../spec/api.md); the handler API is in [CLIENT.md §11](CLIENT.md#11-relayed-services-zensvc). Measured costs are in [STATS.md §5.5](STATS.md#55-relayed-service-costs-measured).

## 1. What a relayed service is

zen-serve never sees plaintext, so it can't run SSH, SMTP or IMAP itself. Instead it relays them:
* **The server** accepts TCP connections on a port, and keeps each connection's bytes in a durable, ordered queue. It never reads them.
* **Handlers** are member clients that hold the fs keys: Node workers, or a browser tab. A handler claims connections, reads their input, and writes their output.
* **A step** is one call of the handler. It reads input, writes output, and changes the connection's state. It commits in one transaction with the handler's own zen-db rows and events. So a reply goes out only if what it reports committed.
* **Takeover.** When a handler dies, another one continues the same live connection, TLS and SSH sessions included. The peer sees a pause, not an error.
* **Nothing is kept.** Input is deleted once acked, and output once written to the socket.

A service has one of two modes:
* **`tcp`**: a public port. Anyone can connect, and the handler authenticates them itself (an SSH key, an IMAP password).
* **`tcp-auth`**: no port of its own. Members connect through zen-serve's own HTTPS listener, as a WebSocket with their zen token, and need topic `append` on the service. `zen-connect` turns that into a local port, or into an ssh `ProxyCommand` ([CLIENT.md §11](CLIENT.md#members-as-clients-tcp-auth-and-zen-connect)).

## 2. Configuring a service

A service is a `[[service]]` table in `zen-serve.toml`. The example configs ([examples/zen-serve.toml](../examples/zen-serve.toml)) carry a commented one.

```toml
[[service]]
name = "imaps"
fs = 1
id = "…"                  # the topic id of svc/imaps, hex
mode = "tcp"
listen = "0.0.0.0:993"
idle_secs = 1860          # IMAP clients re-IDLE within 29 minutes
```

| Setting | Default | Notes |
|---|---|---|
| `name`, `fs`, `id`, `mode` | required | `id` is the service's topic id: `bytes.hex(fs.service('svc', name).id)` from an unlocked fs |
| `listen` | required for `tcp` | refused for `tcp-auth` |
| `max_conns` | 1,024 | open connections per node; more are closed at once |
| `per_ip_rate`, `per_ip_burst` | 10/s, 50 | accepts per peer address and node; `0` turns it off |
| `window` | 256 KiB | input stored and not yet acked, per connection. **SSH needs 1 MiB** (ten channel windows and 64 KiB, svc.md §7.2) |
| `out_window` | 1 MiB | output committed and not yet written, per connection |
| `accept_wait_ms` | 10,000 | a connection no handler claims by then is closed |
| `handshake_secs` | 30 | a connection with no committed step by then is closed |
| `idle_secs` | 300 | no bytes either way for this long closes it; `0` never. **IMAP needs at least 1,860** (RFC 3501 §5.4's 30 minutes) |
| `resume` | true | handlers can take over live connections (§5) |
| `inbox` | none | an X-Wing public key: input is sealed at rest (§4) |

* **Every node carries the same table.** Only `listen` may differ. A handler may be connected to any node, and a step may land on another node than the one holding the socket.
* **Ports.** Open `listen` to the internet: the peers connect there, not to zen-serve's HTTPS port. A port below 1024 needs `CAP_NET_BIND_SERVICE` or a forward from the firewall.
* **Rights** are topic rights on `id`: `consume` to handle, `append` to connect over `tcp-auth`. A grant on the prefix `svc/` covers every service of the fs.
* **Changing the table** needs a restart of each node. Connections on a node end with it: a TCP socket can't move between machines.

## 3. Writing a handler

`serve()` from `@zen/svc` runs a handler for a service until it is stopped. The [echo app](../apps/echo) is the smallest complete example; the [mail app](../apps/mail) is a real protocol.

```ts
import { LineReader, serve } from '@zen/svc';

const h = serve(db, {
  service: ['svc', 'echo'],
  handler: async (c, tx) => {
    let n = c.state ? Number(new TextDecoder().decode(c.state)) : 0;
    const r = new LineReader(c.input);
    for (let l = r.nextText(); l !== null; l = r.nextText()) {
      c.write(`${++n}: ${l}\n`);
      await tx.table('lines').insert({ n, line: l });   // commits with the reply
    }
    c.ack(r.consumed);                                   // whole lines only
    c.state = new TextEncoder().encode(String(n));
    if (c.eof) c.end();
  },
});
```

* **A step reads what isn't acked yet.** Ack only whole protocol units (a line, a command). What isn't acked comes back in the next step, on this handler or the next one.
* **State is small.** `c.state` is sealed with each step and must fit a stored value with the layer's own state (90,000 bytes in all). Keep the protocol's position there, not its data: IMAP keeps which message and offset it is sending, and rebuilds the bytes from its rows.
* **Output is bounded per step.** A step writes at most `c.room` bytes. A handler with more to send keeps its place in the state and calls `c.again()`, and the next step follows at once.
* **Layers** turn the connection's bytes into what the handler sees:
  * `raw()`, the default: bytes.
  * `tls({cert, key})`: TLS 1.3 and 1.2, terminated in the handler. The handler sees plaintext through the same `RawStep`.
  * `starttls(…)`: plain first, then TLS after `c.startTls()`, as SMTP's STARTTLS.
  * `ssh({hostKey})`: an SSH server. The handler sees authentication questions and channels (`SshStep`).
* **Waking.** A handler runs when input arrives. To speak without input (IMAP IDLE telling a client of new mail), a step registers keys with `c.wakeOn(key)`, and the app calls `h.wake(key)` when something changes. An idle connection costs no steps.
* **Where it runs.** In a Node worker (`signInWorker`, as the apps' `workers/`) or in a browser tab: of a browser's tabs, the one that owns the database serves. Run two or more handlers for a service: they share its connections, and each is the other's successor.
* **Errors.** A handler that throws closes its connection and calls `onError`. A conflict re-runs the handler, so it must not have effects outside `tx`.

## 4. Keys and secrets

Every secret a handler needs lives in the fs, sealed like any row, and is read by the handler before `serve()`:
* **TLS certificates and keys** (`tls()`, `starttls()`): PEM, ECDSA P-256/P-384 or Ed25519. The apps keep them in a `certs` table.
* **SSH host keys** (`ssh()`): OpenSSH's Ed25519 PEM. Users' public keys are rows the handler checks (`sshPublicKey` gives the fingerprint).
* **The inbox seed** of a sealed inbox: 32 random bytes. The service's `inbox` setting is its public key (`zw.inboxPublic(seed)`).
* **Protocol passwords**, such as IMAP's app passwords: the mail app stores their hashes in a table.

The admin brings certificates and renews them; there is no ACME client ([TD-SVC-ACME](../spec/TECH_DEBT.md#td-svc-acme)).

**A sealed inbox** keeps plaintext input (SMTP before STARTTLS) sealed at rest: the server encrypts each chunk to the service's key as it stores it, and only handlers with the seed open it. It protects what is stored, not what passes through the server process. A step calls `c.unseal()` once the input is ciphertext anyway (after STARTTLS).

## 5. Takeover

`serve()` holds a handler lease, renewed every second (`leaseTtlMs`, 5 s by default). When a handler dies, its lease lapses, and its connections are claimed by another handler of the service. That one continues from the last committed step:
* output committed but not yet written is released again
* input not acked is pushed again
* the connection's state comes with the claim

What survives a takeover, per layer:

| Layer | Survives | Lost |
|---|---|---|
| `raw()` | everything, from the first step | — |
| `tls()` | everything after the handshake | a handshake in progress: the connection is closed ([TD-SVC-TLS-HANDSHAKE-RESUME](../spec/TECH_DEBT.md#td-svc-tls-handshake-resume)) |
| `starttls()` | the plain phase, and the TLS phase after its handshake | the handshake |
| `ssh()` | everything, key exchanges and rekeys included | — |

* **The pause** is about the lease TTL plus a claim: with a 1 s TTL it measured about one second (STATS §5.5). A shorter TTL takes over faster, and renews more often.
* **`resume: false`** skips the release by key reveal: output goes out a little sooner, but a connection whose handler dies is closed.
* **A node that dies** loses its sockets: their peers see a reset, as with any server.

## 6. What the server sees

The server relays bytes it can't read when the protocol is encrypted end to end (TLS, SSH, STARTTLS after the switch). Even then it sees:
* who connects: peer addresses, or for `tcp-auth` the member's user and device
* when, how much, and in what rhythm: connection times, sizes and timing
* which handler device holds which connection

For a plaintext protocol (SMTP before STARTTLS, a raw service) the server also sees the bytes in transit, and stores them until they are acked, unless the service has a sealed inbox. Output is stored sealed until its step has committed, then with its key until it is written. A malicious server can replay an old snapshot of a connection ([TD-SVC-STATE-ROLLBACK](../spec/TECH_DEBT.md#td-svc-state-rollback)); svc.md §12 has the details.

## 7. Costs

[STATS.md §5.5](STATS.md#55-relayed-service-costs-measured) has measured step latency per layer, throughput, the takeover pause and the cost of an idle connection. In short: every step is a commit, so a request and its reply cost a few round trips to the server, and throughput is bounded by how much input one step takes.

## 8. Limits

* TCP only: no UDP or QUIC ([TD-SVC-UDP](../spec/TECH_DEBT.md#td-svc-udp)).
* No ACME ([TD-SVC-ACME](../spec/TECH_DEBT.md#td-svc-acme)).
* A TLS handshake doesn't survive a takeover ([TD-SVC-TLS-HANDSHAKE-RESUME](../spec/TECH_DEBT.md#td-svc-tls-handshake-resume)).
* SSH: public keys only, no PTY, no forwarding ([TD-SVC-SSH-SCOPE](../spec/TECH_DEBT.md#td-svc-ssh-scope)).
* `tcp-auth` connectors can't slow the server's sends ([TD-SVC-CONNECT-BACKPRESSURE](../spec/TECH_DEBT.md#td-svc-connect-backpressure)).
* Changing a sealed inbox's key needs the old seed until its connections end ([TD-SVC-INBOX-ROTATION](../spec/TECH_DEBT.md#td-svc-inbox-rotation)).
* A malicious server can roll a connection back ([TD-SVC-STATE-ROLLBACK](../spec/TECH_DEBT.md#td-svc-state-rollback)).
* The mail app is inbound only, with an IMAP subset ([TD-MAIL-SCOPE](../spec/TECH_DEBT.md#td-mail-scope)).
