# Operations

How to run zen-serve: a single node on the embedded backend, or a FoundationDB cluster that zen-serve supervises itself. Also covers backup, point-in-time restore, export/import, migration and TLS (§8).

## 1. Backends

| | `embedded` | `fdb` |
|---|---|---|
| Storage | one `redb` file, `data_dir/zen.redb` | a FoundationDB 7.3 cluster |
| Nodes | one | any number of zen-serve nodes on one cluster |
| Build | default | `cargo build -p zen-server --release --features fdb` (needs `libfdb_c`) |
| Backup | `zen-serve export` (consistent snapshot) | native continuous backup with point-in-time restore, plus `export` |

`[storage] backend` selects the backend. If it is absent, zen-serve uses `fdb` on a node set up with `zen-serve init` or `join` (which write `data_dir/fdb.cluster`), and `embedded` otherwise.

```toml
[storage]
backend = "fdb"                       # or "embedded"
cluster_file = "/etc/foundationdb/fdb.cluster"   # default: data_dir/fdb.cluster, then the platform default
key_prefix = ""                       # serve the keyspace under this prefix (§5.3)
```

Both backends store exactly the same bytes (keyspace.md), so a store moves from one to the other unchanged (§6).

## 2. Installing FoundationDB

zen-serve pins one FoundationDB release (G22): **7.3.79**. To install it:

```sh
sudo scripts/install-fdb.sh            # into /opt/foundationdb, sha256-checked
```

The script unpacks Apple's official client and server packages, and verifies each against its pinned sha256. No service is installed or started: zen-serve runs the processes itself. As root it also links `libfdb_c.so` into `/usr/lib`, so builds and binaries find it.

zen-serve looks for the binaries (`fdbserver`, `fdbcli`, `fdbbackup`, `fdbrestore`, `backup_agent`) in `[fdb] bin_dir`. If that is unset, it searches `/opt/foundationdb/…`, the Debian package locations, then `PATH`.

## 3. A cluster supervised by zen-serve

zen-serve takes over the job of `fdbmonitor` (DESIGN-2 §6):
* It starts this node's `fdbserver` processes and restarts any that exit, with backoff from 1 s up to 30 s.
* It logs their stderr.
* It stops them when zen-serve receives Ctrl-C or SIGTERM.

```toml
[fdb]
processes = 1          # fdbserver processes on this node; one per core is a good start
listen_ip = "10.0.0.5" # address the processes bind to
public_ip = "10.0.0.5" # address other nodes use (default: listen_ip)
port = 4500            # process i listens on port + i
auto_redundancy = true
# tls_cert = "/etc/zen/fdb.pem"   # all three, or none (§3.4)
# tls_key  = "/etc/zen/fdb.key"
# tls_ca   = "/etc/zen/ca.pem"
```

### 3.1 Commands

| Command | What it does |
|---|---|
| `zen-serve init -c zen.toml [--no-api]` | Create a cluster on this node: write `data_dir/fdb.cluster`, start the processes, `configure new single ssd`, print the **join token**, then serve the API. |
| `zen-serve join <token> -c zen.toml [--no-api]` | Add this node to the cluster: write the cluster file from the token, start the processes, then serve. |
| `zen-serve serve -c zen.toml` | The everyday command (e.g. the systemd unit). On a node set up by `init`/`join` it also runs the processes. |
| `zen-serve token -c zen.toml` | Print the join token again. |
| `zen-serve status -c zen.toml` | Availability, health, redundancy, machines, processes, coordinators. The exit code is 1 when the database is unavailable. Admins can also call `POST /v1/admin/status`. |

All processes on one node share a machine id, stored in `data_dir/fdb/node-id`. So FoundationDB counts each zen-serve node as one machine, and keeps replicas on different nodes.

### 3.2 Redundancy policy

With `auto_redundancy` on, the cluster status is checked every 30 s. One node acts on it: the node that owns the lowest process address.

| Nodes | Mode | Coordinators |
|---|---|---|
| 1–2 | `single` | 1 |
| 3–4 | `double` | 3 |
| ≥ 5 | `triple` | 5 |

* The mode is only ever raised (`configure double` / `configure triple`), then `coordinators auto` runs.
* It is never lowered: a node that dies must not reduce redundancy. To shrink a cluster, exclude nodes and reconfigure with `fdbcli` yourself.

### 3.3 Addresses: the API and the database are separate

Two unrelated settings:

| Setting | Who connects | Typical value |
|---|---|---|
| `listen` (top level) | clients and browsers: the zen-serve API | `0.0.0.0:443` with `[tls]` (§8), or `127.0.0.1:8080` behind a reverse proxy |
| `[fdb] listen_ip`, `public_ip`, `port` | only FoundationDB peers: other nodes' `fdbserver`s, zen-serve's own database client, `fdbcli`, backup agents | `127.0.0.1` (single node) or a private / WireGuard address |

zen-serve never proxies FoundationDB traffic. Exposing the API does not expose the database.

* **Single node.**
  * Keep `listen_ip = "127.0.0.1"` (the default): nothing outside the machine can reach FoundationDB.
  * Local processes still can: FoundationDB only speaks TCP (`IP:PORT`), with no Unix-socket transport.
  * For no database port at all, use the embedded backend.
* **Several nodes over WireGuard (or another private network).** Set `listen_ip` to the node's tunnel address on every node; `public_ip` defaults to it:

  ```toml
  listen = "0.0.0.0:8080"     # the API, public
  [fdb]
  listen_ip = "10.8.0.1"      # this node's WireGuard address
  ```

* **One address per process.**
  * Each `fdbserver` binds exactly one `IP:PORT`, and the cluster file lists those addresses.
  * So in a multi-node cluster the local zen-serve also reaches its own database through the tunnel address. That traffic stays on the machine.
  * Binding `127.0.0.1` and the tunnel address at the same time is neither possible nor needed.
* **Never** set `listen_ip` to `0.0.0.0` or a public address without TLS (§3.4).

### 3.4 Network trust (G18)

* **The join token is the cluster file**, base64url-encoded: the cluster's name and its coordinators' addresses. It is not a credential. Anyone who can reach the FoundationDB ports can read and write the whole database, which is ciphertext and metadata (the threat model: the server sees no plaintext).
* So run the cluster on a **private network** (§3.3), or turn on **TLS**. With `tls_cert`, `tls_key` and `tls_ca` set on every node:
  * the processes listen with `:tls`
  * `fdbcli`, the backup tools and zen-serve's own client use the same files
  * the CA decides who may join
* Automatic TLS between nodes comes later, with ACME.

### 3.5 Versions and clocks (G20)

* Leases, claims and idempotency records expire by **commit version**, about 1,000,000 per second, never by wall-clock time. On an idle cluster versions advance in steps of up to about 2 s, so a lease can expire up to that much late, never early.
* Sessions and challenges expire by wall-clock time (unix seconds). Keep node clocks in sync with NTP.

## 4. Several API nodes

Any number of zen-serve nodes can serve one cluster: nodes set up with `join`, or plain `serve` nodes pointed at the cluster with `[storage] cluster_file`. A request can go to any node:
* **Sessions** are stored in the keyspace, hashed. Each node caches a session for up to 10 s, so a logout or expiry reaches the other nodes within 10 s.
* **ACL changes** apply on every node as soon as each sees the new version (a watch on the ACL head).
* **Challenges** are MACs under a cluster-wide key, so any node can check one. Each is single-use across the cluster.
* **Ephemeral messages** pass through a short-lived ring in the keyspace (api.md §9.1).
* Subscriptions, long-polls and fencing work across nodes: they only use storage.

Every node runs the sweeper. Each sweep is idempotent.

An unclaimed cluster prints a claim token on every node. Use the token of the node you send `/v1/acl/put` to. Once the cluster is claimed, every node deletes its `claim-token` file (api.md §4.1).

## 5. Backup and point-in-time restore (FoundationDB)

The unit of backup and restore is the **whole keyspace**: every fs, topic, cursor, consumer group and the ACL together (G14). Restoring a part would leave cursors and logs inconsistent.

### 5.1 Continuous backup

Enable the backup agents on at least one node (more for throughput):

```toml
[backup]
agents = true
```

```sh
zen-serve backup start    -c zen.toml --dest file:///srv/zen-backups/   # or blobstore://…
zen-serve backup status   -c zen.toml
zen-serve backup describe -c zen.toml --dest file:///srv/zen-backups/backup-…   # restorable range
zen-serve backup stop     -c zen.toml
```

* The backup runs continuously: snapshots plus mutation logs (FoundationDB's `fdbbackup start -z`).
* Any version between the first complete snapshot and the newest log is restorable.
* Backups hold what the database holds: ciphertext and metadata. They also hold hashed sessions and the challenge key, but no bearer tokens. The credential store (auth.md §4) is in them too: API tokens only as hashes, but the public keys of password-derived keys, with their salts, are offline-guessable verifiers (auth.md §11.4), and so are OPAQUE records together with the OPAQUE server setup, which backups also hold (auth.md §8.7). Passkey public keys and the key fingerprints of registered TLS client certificates are not secrets. Protect backups accordingly.

### 5.2 Restore

```sh
zen-serve restore -c zen.toml --source file:///srv/zen-backups/backup-… \
    [--timestamp 2026/10/04.12:00:00+0000 | --version V] [--add-prefix P]
    [--orig-cluster-file /etc/zen/old-fdb.cluster]
```

* Without `--add-prefix`, the target cluster must be empty: restore into a new cluster.
* `--timestamp` picks the newest restorable version at or before that time. The time is translated into a version with the metadata of the database the backup was taken from, so when the target is a different cluster, pass that database's cluster file as `--orig-cluster-file` (default: the target's). Restoring a clone into the same cluster needs nothing extra.
* The backup agents must be running while the restore runs.

### 5.3 Clone at time T

To look at yesterday without disturbing live data:
1. Restore with `--add-prefix clone-2026-10-03/`.
2. Run a second zen-serve on the same cluster with `[storage] key_prefix = "clone-2026-10-03/"`, on another port.

The clone is a full, independent copy: the same ACL, members and sessions as at time T. Its writes never touch live data. Remove it with `fdbcli --exec 'writemode on; clearrange clone-2026-10-03/ clone-2026-10-030'` (the end key is the prefix with its last byte incremented). Clones are part of later backups until removed.

## 6. Export, import, migrate

A logical, backend-independent copy. It is ciphertext only, so the operator needs no keys.

```sh
zen-serve export  -c zen.toml --out zen.export
zen-serve import  -c zen.toml --in zen.export [--force]
zen-serve migrate -c zen-fdb.toml --from-data-dir /var/lib/zen   # embedded → configured backend
```

* **Format.** A header, then every key-value pair in key order, then a trailer with the count, a source version and a BLAKE3 digest. The digest is checked on import.
* **Server metadata is not copied** (keyspace.md §3.4): the embedded backend's version clock and the cluster's challenge key. The target keeps its own challenge key, or creates one when it first starts. A challenge lives 60 s, so nothing depends on the key surviving the copy; sessions are copied and keep working.
* **Sign-in state is data and is copied** (keyspace.md §3.7): stored credentials, the login-name index, the key of the fake password parameters, the OPAQUE server setup and the pinned origins. So password sign-in (both methods), passkeys, API tokens, registered client certificates and the origin pin keep working on the target. OPAQUE sign-ins in flight start again: their state is sealed under the challenge key, which is not copied.
* **The OPAQUE server setup must survive.** Every OPAQUE credential, and every keyslot opened by an OPAQUE export key, depends on it (auth.md §8.1). A copy or restore that loses it, or a new cluster that starts with OPAQUE users but without it, locks those users out of method 3 for good; they register again from a session of another method. Keep exports and backups that hold it as protected as the data. A target that is reached under a different origin needs `public_origins`, or its pin replaced (auth.md §5.2). Passkeys are bound to their relying-party id, the host they were registered under: on a target with another host they no longer work, and users register new ones (auth.md §7.1).
* **Consistency.**
  * On the embedded backend an export is one consistent snapshot.
  * On FoundationDB a large export spans several transactions, so it is consistent only while the servers are stopped. It warns otherwise. Use native backup for consistent copies of a live cluster.
* **Import** refuses a target that already holds data, unless `--force`. A server that was only started (never claimed or written to) holds only metadata, so it counts as empty. Import writes in transactions of at most about 4 MB.
* **Versions.** Imported keys and values keep their versionstamps byte for byte. Afterwards the target's version clock is advanced past the source's newest version, so new versionstamps sort after the imported ones. On FoundationDB this is `\xff/minRequiredCommitVersion` (what `fdbcli advanceversion` does), and it causes one quick recovery.
* **`migrate`** is export plus import without the file. Stop the embedded server first. Old sessions keep working on the new backend.

## 7. Upgrades (G22)

The FoundationDB version is pinned by `scripts/install-fdb.sh` and the `foundationdb` crate's API version (7.3). Rolling upgrades with the multi-version client, orchestrated by `zen-serve upgrade`, come later. Until then, upgrade every node together with the cluster stopped.

## 8. TLS on the API listener

zen-serve can terminate TLS itself, or stay on plain HTTP behind a reverse proxy that does. Both are supported; without `[tls]` the listener speaks plain HTTP, as before.

### 8.1 Native TLS

```toml
listen = "0.0.0.0:443"            # binding a port below 1024 needs CAP_NET_BIND_SERVICE
public_origins = ["https://zen.example.org"]

[tls]
cert = "/etc/zen/api.pem"         # PEM chain, the server's certificate first
key = "/etc/zen/api.key"          # PEM private key
# client_ca = "/etc/zen/clients-ca.pem"   # client certificates for sign-in (§8.2)
```

* **TLS 1.3 only**, ALPN `http/1.1`. WebSocket (`/v1/stream`) runs over the same connection type. Current browsers and HTTP libraries all speak TLS 1.3.
* **rustls, no OpenSSL**, with one of two crypto providers, chosen when zen-serve is built (§8.4): **ring** (the default build) or **RustCrypto** (the pure-Rust build). The start-up log names the build's provider.
* **Key exchange**, preferred first: the post-quantum hybrid **`X25519MLKEM768`**, then `X25519`, `secp256r1`, and in the default build `secp384r1`. Browsers that support the hybrid get it; others fall back. Both builds offer the hybrid, which is zen-serve's own on RustCrypto (ML-KEM-768 and X25519) in either. **Ciphers**: `TLS_AES_128_GCM_SHA256`, `TLS_AES_256_GCM_SHA384`, `TLS_CHACHA20_POLY1305_SHA256`, preferred in that order.
* **The server key**: ECDSA P-256 or P-384 (PKCS#8 or SEC1 PEM), Ed25519 (PKCS#8), or, in the default build only, **RSA** (PKCS#1 or PKCS#8 PEM) with a 2048- to 4096-bit modulus and an odd public exponent from 65537 to 2³² − 1, signing with RSA-PSS as TLS 1.3 requires. ring's RSA signing is constant-time. An RSA key outside those limits stops the start with a message saying why.
  * **The pure-Rust build refuses RSA server keys**: the start stops with a message saying it was built without the `ring` feature. Its only RSA implementation, the `rsa` crate, has private-key operations that are not constant-time, and the server would be open to the Marvin timing attack (RUSTSEC-2023-0071, `TD-TLS-RSA-SERVER-KEY`). Use the default build, or ask your CA for an ECDSA certificate; the chain above it may be RSA-signed, since only clients verify it (a Let's Encrypt ECDSA certificate works).
  * RSA **client** certificates and client CAs work in both builds (§8.2).
* **Start-up checks.** A file that can't be read, a key that doesn't match the certificate, or a `client_ca` without certificates stops the start with a message naming the setting.
* **Rotation.** The files are read at start-up: restart zen-serve after renewing the certificate. Automatic certificates (ACME) and reloading without a restart are deferred (`TD-TLS-ACME`).
* **Limits.** A handshake must finish within 10 s; at most 1024 run at once, and further connections wait in the kernel's accept queue.
* Every node of a cluster has its own `[tls]`.

### 8.2 Client certificates

With `client_ca` set and `[auth] mtls` on, the listener asks clients for a certificate from that CA, without requiring one, for sign-in method 5 (auth.md §10.1). A client that presents a certificate the CA didn't issue, or an expired one, fails the handshake. The client CA, its certificates and the clients' keys may be ECDSA (P-256, P-384), Ed25519, or RSA of 2048 to 4096 bits (auth.md §10.1); a smaller RSA key, or a larger one, fails the handshake. Both builds verify the same keys and signatures (§8.4).

A small CA with OpenSSL:

```sh
openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes -days 3650 \
    -subj "/CN=zen clients" -keyout clients-ca.key -out clients-ca.pem
openssl req -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes -subj "/CN=alice" \
    -keyout alice.key -out alice.csr
openssl x509 -req -in alice.csr -CA clients-ca.pem -CAkey clients-ca.key -days 365 \
    -extfile <(printf 'extendedKeyUsage=clientAuth') -out alice.pem
openssl pkcs12 -export -in alice.pem -inkey alice.key -out alice.p12   # to import into a browser
```

The certificate signs in only after it is registered to a member (auth.md §10.3, api.md §3.13): the member registers it from a connection that presents it, or an admin uploads `alice.pem`.

### 8.3 Behind a reverse proxy

A proxy that only terminates TLS needs nothing special: zen-serve listens on HTTP, on an address only the proxy reaches, with `public_origins` set to the public `https://` origin.

A proxy can also verify client certificates and forward them, for method 5's **trusted-proxy mode** (auth.md §10.2). With nginx:

```nginx
map $http_upgrade $connection_upgrade { default upgrade; "" close; }
map $ssl_client_verify $zen_client_cert { SUCCESS $ssl_client_escaped_cert; default ""; }

server {
    listen 443 ssl;
    server_name zen.example.org;
    ssl_certificate         /etc/nginx/zen.pem;
    ssl_certificate_key     /etc/nginx/zen.key;
    ssl_client_certificate  /etc/nginx/clients-ca.pem;
    ssl_verify_client       optional;      # clients without a certificate still get in

    location / {
        proxy_pass http://10.0.0.10:8080;
        proxy_http_version 1.1;
        proxy_set_header Host $host;
        proxy_set_header Upgrade $http_upgrade;              # /v1/stream
        proxy_set_header Connection $connection_upgrade;
        # Always set, on every request: an empty value removes the header,
        # including one the client sent itself.
        proxy_set_header X-Client-Cert $zen_client_cert;
    }
}
```

```toml
# zen-serve, on 10.0.0.10
listen = "10.0.0.10:8080"
public_origins = ["https://zen.example.org"]

[auth]
mtls_trusted_proxies = ["10.0.0.5"]    # the nginx host
# mtls_proxy_header = "x-client-cert"  # the default
```

> **Warning: zen-serve trusts the proxy's verification completely.** It doesn't check the forwarded certificate's chain or dates. Anything that can send a request from a trusted address, with any value in the header, can sign in as any member whose certificate it knows, and certificates are not secret.
> * List **only the proxies** in `mtls_trusted_proxies`, never a client network, and never `0.0.0.0/0`.
> * The proxy must **set or clear the header on every request** it forwards (`proxy_set_header`, as above), never pass a client's value through.
> * No other client or service may reach zen-serve from a trusted address. With the proxy on the same host and `127.0.0.1` trusted, every local process is trusted too.
> * A request that carries the header from any other address is refused with 401 and logged (auth.md §10.2), which shows a proxy whose address is missing from the list.

Other proxies: Caddy (`header_up X-Client-Cert {http.request.tls.client.certificate_der_base64}`), HAProxy (`http-request set-header X-Client-Cert %[ssl_c_der,base64]` when `ssl_c_verify` is 0) and Traefik (`passTLSClientCert` with `pem: true`, header `X-Forwarded-Tls-Client-Cert`, set as `mtls_proxy_header`) all send formats zen-serve reads.

zen-serve itself may also listen with `[tls]` behind the proxy: the proxy's own certificate, if it presents one, is not taken for a user's.

### 8.4 Builds and crypto providers

zen-server has a Cargo feature **`ring`**, on by default, that chooses the TLS crypto provider:

| | Default build (`ring`) | Pure-Rust build (`--no-default-features`) |
|---|---|---|
| Provider | rustls's provider on [ring](https://github.com/briansmith/ring) (`tls::ring` in zen-server) | zen-serve's own on RustCrypto (`tls::rustcrypto`) |
| Record protection, HKDF, signature verification, signing, randomness | ring | RustCrypto |
| `X25519MLKEM768` | zen-serve's, on RustCrypto (ring has no ML-KEM) | the same |
| `X25519`, `secp256r1` | ring | RustCrypto |
| `secp384r1` key exchange | ring | not offered |
| RSA server keys | yes (§8.1) | refused (`TD-TLS-RSA-SERVER-KEY`) |
| Verified signatures (client certificates, CAs) | ECDSA P-256/P-384, Ed25519, RSA PKCS#1 v1.5 and PSS, under the same RSA policy (auth.md §10.1) | the same |
| Builds C and assembly | yes, ring's: a C compiler is needed | no |
| Independent review | ring and its rustls provider are widely deployed; the hybrid key exchange is zen-serve's (`TD-TLS-PROVIDER-AUDIT`) | not yet (`TD-TLS-PROVIDER-AUDIT`) |

```sh
cargo build -p zen-server --release                                   # default: ring (needs a C compiler)
cargo build -p zen-server --release --no-default-features             # pure Rust
cargo build -p zen-server --release --no-default-features --features fdb
```

* ring brings C, assembly and `unsafe` code into the build. Its RSA verification alone would take 2048- to 8192-bit keys and public exponents from 3, so zen-serve applies the policy of auth.md §10.1 in front of it.
* The pure-Rust build needs no C compiler. With one installed, `blake3` still assembles its SIMD code; adding `--features blake3/pure` turns that off too, so that nothing calls a C compiler (FoundationDB's `libfdb_c` is a C library either way).
* Passkey RS256 verification (auth.md §7) uses the `rsa` crate in both builds.
* The two builds speak the same TLS and accept the same client certificates; only RSA server keys and the extra `secp384r1` group differ.
