# Operations

How to run zen-serve: a single node on the embedded backend, or a FoundationDB cluster that zen-serve supervises itself. Also covers backup, point-in-time restore, export/import and migration.

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
| `listen` (top level) | clients and browsers: the zen-serve API | `0.0.0.0:8080`, or `127.0.0.1:8080` behind a reverse proxy |
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
* Backups hold what the database holds: ciphertext and metadata. They also hold hashed sessions and the challenge key, but no bearer tokens. The credential store (auth.md §4) is in them too: API tokens only as hashes, but the public keys of password-derived keys, with their salts, are offline-guessable verifiers (auth.md §11.4). Protect backups accordingly.

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
* **Sign-in state is data and is copied** (keyspace.md §3.7): stored credentials, the login-name index, the key of the fake password parameters and the pinned origins. So password sign-in, API tokens and the origin pin keep working on the target. A target that is reached under a different origin needs `public_origins`, or its pin replaced (auth.md §5.2).
* **Consistency.**
  * On the embedded backend an export is one consistent snapshot.
  * On FoundationDB a large export spans several transactions, so it is consistent only while the servers are stopped. It warns otherwise. Use native backup for consistent copies of a live cluster.
* **Import** refuses a target that already holds data, unless `--force`. A server that was only started (never claimed or written to) holds only metadata, so it counts as empty. Import writes in transactions of at most about 4 MB.
* **Versions.** Imported keys and values keep their versionstamps byte for byte. Afterwards the target's version clock is advanced past the source's newest version, so new versionstamps sort after the imported ones. On FoundationDB this is `\xff/minRequiredCommitVersion` (what `fdbcli advanceversion` does), and it causes one quick recovery.
* **`migrate`** is export plus import without the file. Stop the embedded server first. Old sessions keep working on the new backend.

## 7. Upgrades (G22)

The FoundationDB version is pinned by `scripts/install-fdb.sh` and the `foundationdb` crate's API version (7.3). Rolling upgrades with the multi-version client, orchestrated by `zen-serve upgrade`, come later. Until then, upgrade every node together with the cluster stopped.
