# zen-serve

An end-to-end encrypted storage server you can self-host. Think "remote LUKS" with a database and an event broker built in.

zen-serve stores and replicates **ciphertext only**. It never sees keys, file names, table or column names, keys, values or event contents. All encryption happens in the client (native, or in the browser through WebAssembly). FoundationDB handles sharding, replication and transactions. Cryptography is post-quantum hybrid from day one.

Building blocks:
* encrypted ordered **KV** with transactions
* an **event log** with exactly-once append and sequential, per-key and single-leader consumers
* **server-side CRDTs that merge on ciphertext** (including the file tree)
* plaintext **`/unencrypted`** static serving for the app bootstrap

## Repository layout

| Path | What |
|---|---|
| [`docs/`](docs/) | Design discussion and decisions (`DESIGN*.md`, `API.md`, `GAPS.md`) |
| [`spec/`](spec/) | **Normative** specification: formats, labels, suites, glossary, test vectors |
| [`crates/zen-core`](crates/zen-core) | Client-side crypto core (Rust, native + `wasm32`) |
| [`crates/zen-proto`](crates/zen-proto) | Wire types of the API (CBOR), shared by server and clients (native + `wasm32`) |
| [`crates/zen-store`](crates/zen-store) | Storage trait with FoundationDB semantics, tuple encoding, embedded `redb` backend, FoundationDB backend (feature `fdb`) |
| [`crates/zen-server`](crates/zen-server) | The `zen-serve` binary: auth, signed ACL, KV, commit, event log, consumer groups, WebSocket stream, static files, FoundationDB supervisor, backup/export |
| [`scripts/install-fdb.sh`](scripts/install-fdb.sh) | Installs the pinned FoundationDB release (sha256-checked) |

## Status

* Milestone 1 (`spec/` + `zen-core`): done.
* Milestone 2 (`zen-server` on the embedded backend, [`docs/MILESTONE-2.md`](docs/MILESTONE-2.md)): done. Server-side CRDT ops return 501 for now.
* Milestone 3 (FoundationDB backend, supervisor, backup/PITR, [`docs/MILESTONE-3.md`](docs/MILESTONE-3.md)): done.

## Running

```sh
cargo run -p zen-server -- serve --config examples/zen-serve.toml
```

On first start the server prints a one-time **claim token**. The first signed ACL (`POST /v1/acl/put`) must carry it, and that pins the first admin. Plain HTTP only for now: put it behind a TLS reverse proxy.

On FoundationDB, with zen-serve running the `fdbserver` processes itself:

```sh
sudo scripts/install-fdb.sh
cargo build -p zen-server --release --features fdb
zen-serve init -c examples/zen-serve-fdb.toml          # first node; prints a join token
zen-serve join <token> -c zen-serve-fdb.toml           # every further node
```

See [`spec/operations.md`](spec/operations.md) for clusters, backup and point-in-time restore, export/import and migration.

## Development

```sh
cargo test                                                     # includes byte-exact test vectors
cargo clippy --all-targets -- -D warnings                      # --all-features needs libfdb_c
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
