# zen-serve

An end-to-end encrypted storage server you can self-host. Think "remote LUKS" with a database and an event broker built in.

zen-serve stores and replicates **ciphertext only**. It never sees keys, file names, table or column names, keys, values or event contents. All encryption happens in the client (native, or in the browser through WebAssembly). FoundationDB handles sharding, replication and transactions. Cryptography is post-quantum hybrid from day one.

Building blocks:
* encrypted ordered **KV** with transactions
* an **event log** with exactly-once append and sequential, per-key and single-leader consumers
* a **filesystem whose tree and file versions the server merges as CRDTs**, on ciphertext
* plaintext **`/unencrypted`** static serving for the app bootstrap
* **zen-db and an in-app broker** in one client class, where rows, messages and files commit in one transaction, with CRDT tables the server merges, a local filesystem replica that works offline, and Loro documents ([`spec/zendb.md`](spec/zendb.md), [`docs/DB.md`](docs/DB.md))

## Repository layout

| Path | What |
|---|---|
| [`docs/`](docs/) | Design discussion and decisions (`DESIGN*.md`, `API.md`, `GAPS.md`, `EXAMPLES.md`) |
| [`spec/`](spec/) | **Normative** specification: formats, labels, suites, glossary, test vectors |
| [`crates/zen-core`](crates/zen-core) | Client-side crypto core (Rust, native + `wasm32`) |
| [`crates/zen-proto`](crates/zen-proto) | Wire types of the API (CBOR), shared by server and clients (native + `wasm32`) |
| [`crates/zen-store`](crates/zen-store) | Storage trait with FoundationDB semantics, tuple encoding, embedded `redb` backend, FoundationDB backend (feature `fdb`) |
| [`crates/zen-tls-rustcrypto`](crates/zen-tls-rustcrypto) | A rustls crypto provider on RustCrypto (pure Rust, native + `wasm32`; TLS 1.2 suites behind `tls12`), its record protection at the byte level, and the RSA key policy, for the server's native TLS and the relayed services' TLS |
| [`crates/zen-svc`](crates/zen-svc) | Protocol engines of relayed services (sans-IO, native + `wasm32`): the TLS 1.3/1.2 server whose state another handler restores at any record boundary ([`spec/svc.md`](spec/svc.md) §7.1) |
| [`crates/zen-server`](crates/zen-server) | The `zen-serve` binary: auth, signed ACL, KV, commit, event log, consumer groups, WebSocket stream, static files, FoundationDB supervisor, backup/export |
| [`crates/zen-svc-wasm`](crates/zen-svc-wasm) | WASM bindings of zen-svc's TLS engine, a second module loaded only by services that use `tls()` (`@zen/svc-wasm`, built into [`packages/svc-wasm`](packages/svc-wasm)) |
| [`crates/zen-wasm`](crates/zen-wasm) | WASM bindings of zen-core and zen-proto for the TypeScript client |
| [`packages/client`](packages/client) | `@zen/client`: the TypeScript client library, Node and browsers ([`docs/CLIENT.md`](docs/CLIENT.md)) |
| [`packages/db`](packages/db) | `@zen/db`: the client-side database (tables, indexes, queries, transactions), the in-app broker (`on`, consumers, scheduler, request/reply, sagas) and CRDT tables, with offline caches ([`docs/DB.md`](docs/DB.md)) |
| [`packages/fs`](packages/fs) | `@zen/fs`: a local replica of a filesystem tree (the node table, content on demand, offline writes), under `zen-mount` too ([`docs/CLIENT.md`](docs/CLIENT.md) §8) |
| [`packages/loro`](packages/loro) | `@zen/loro`: [Loro](https://loro.dev) documents kept on a topic, with snapshots and offline edits ([`docs/DB.md`](docs/DB.md) §20) |
| [`packages/svc`](packages/svc) | `@zen/svc`: handlers of relayed TCP services, each step in one transaction with the handler's writes, raw or behind `tls()` ([`spec/svc.md`](spec/svc.md), [`docs/CLIENT.md`](docs/CLIENT.md) §11) |
| [`apps/`](apps/) | Five example web apps (kanban, chat, booking, checkout, ledger), `hello`, and `echo` (a relayed line service), on `@zen/db`, `@zen/fs`, `@zen/loro` and `@zen/svc` ([`docs/EXAMPLES.md`](docs/EXAMPLES.md), [`apps/README.md`](apps/README.md)) |
| [`packages/fuse`](packages/fuse) | `zen-mount`: mounts a zen-serve filesystem as a local directory (FUSE) |
| [`scripts/install-fdb.sh`](scripts/install-fdb.sh) | Installs the pinned FoundationDB release (sha256-checked) |

## Status

* Milestone 1 (`spec/` + `zen-core`): done.
* Milestone 2 (`zen-server` on the embedded backend, [`docs/MILESTONE-2.md`](docs/MILESTONE-2.md)): done. Server-side CRDT ops return 501 for now.
* Milestone 3 (FoundationDB backend, supervisor, backup/PITR, [`docs/MILESTONE-3.md`](docs/MILESTONE-3.md)): done.
* Milestone 3.5 (server-merged CRDT filesystem, [`spec/fs.md`](spec/fs.md), [`docs/MILESTONE-3.5.md`](docs/MILESTONE-3.5.md)): done on the server side.
* Milestone 4 (`@zen/client` and `zen-mount`, [`docs/MILESTONE-4.md`](docs/MILESTONE-4.md), [`docs/CLIENT.md`](docs/CLIENT.md)): done.
* Milestone 4.5 (zen-db and the in-app broker, specs only, [`spec/zendb.md`](spec/zendb.md), [`docs/EXAMPLES.md`](docs/EXAMPLES.md), [`docs/MILESTONE-4.5.md`](docs/MILESTONE-4.5.md)): done.
* Milestone 5 (`@zen/db`, CRDT tables on the server, `@zen/fs` replica, `@zen/loro`, five example apps, [`docs/MILESTONE-5.md`](docs/MILESTONE-5.md), outcome at its end): done.
* Milestone 5.5 (relayed services: a blind TCP relay with handlers on member clients and mid-connection takeover, [`docs/MILESTONE-5.5.md`](docs/MILESTONE-5.5.md)): in progress. Done: the spec, the server relay, stream push, `@zen/svc` with raw handlers, the TLS engine and the `tls()` layer (steps 1–8).

## Building

```sh
cargo build -p zen-server --release                          # the default build
cargo build -p zen-server --release --no-default-features    # pure Rust
```

The **default build** needs a **C compiler** (`cc`/`gcc` or `clang`): its native TLS runs on rustls's [ring](https://github.com/briansmith/ring) provider, which builds C and assembly. That build also accepts RSA server keys. The **pure-Rust build** (`--no-default-features`) uses zen-serve's own TLS provider on RustCrypto instead and needs no C compiler; its server key must be ECDSA or Ed25519. Both offer the post-quantum hybrid key exchange and the same client-certificate sign-in. Add `--features fdb` to either for the FoundationDB backend; without it the binary is embedded-only, with no FoundationDB commands and no `libfdb_c`. `--no-default-features --features pure` builds without calling a C compiler at all. See [`spec/operations.md`](spec/operations.md) §8.4.

## Running

```sh
cargo run -p zen-server -- serve --config examples/zen-serve.toml
```

On first start the server prints a one-time **claim token**. The first signed ACL (`POST /v1/acl/put`) must carry it, and that pins the first admin.

Without `[tls]` the server speaks plain HTTP, for a TLS reverse proxy in front. With `[tls]` it terminates TLS itself: TLS 1.3 on rustls (no OpenSSL), with the post-quantum hybrid key exchange `X25519MLKEM768`, and optionally TLS client certificates for sign-in. See [`spec/operations.md`](spec/operations.md) §8 and [`spec/auth.md`](spec/auth.md) §10.

On FoundationDB, with zen-serve running the `fdbserver` processes itself:

```sh
sudo scripts/install-fdb.sh
cargo build -p zen-server --release --features fdb
zen-serve init -c examples/zen-serve-fdb.toml          # first node; prints a join token
zen-serve join <token> -c zen-serve-fdb.toml           # every further node
```

See [`spec/operations.md`](spec/operations.md) for clusters, backup and point-in-time restore, export/import, migration and TLS.

## Development

```sh
cargo test                                                     # includes byte-exact test vectors
cargo test --all --no-default-features                         # the pure-Rust build
cargo clippy --all-targets -- -D warnings                      # --all-features needs libfdb_c
cargo clippy --all-targets --no-default-features -- -D warnings
cargo check -p zen-core -p zen-proto --target wasm32-unknown-unknown
cargo run -p zen-core --example gen_vectors --features test-utils   # regenerate spec/test-vectors
```

Against FoundationDB (each test server gets its own key prefix on one cluster):

```sh
zen-serve init --no-api -c test-node.toml &               # or any cluster
ZEN_TEST_CLUSTER_FILE=/path/fdb.cluster ZEN_TEST_BACKEND=fdb \
  cargo test -p zen-store -p zen-server --features fdb
```

## License

MIT
