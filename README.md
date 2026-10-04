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
| [`crates/zen-store`](crates/zen-store) | Storage trait with FoundationDB semantics, tuple encoding, embedded `redb` backend |
| [`crates/zen-server`](crates/zen-server) | The `zen-serve` binary: auth, signed ACL, KV, commit, event log, consumer groups, WebSocket stream, static files |

## Status

* Milestone 1 (`spec/` + `zen-core`): done.
* Milestone 2 (`zen-server` on the embedded backend, [`docs/MILESTONE-2.md`](docs/MILESTONE-2.md)): done. Server-side CRDT ops return 501 for now.
* Next: the FoundationDB backend and supervisor.

## Running

```sh
cargo run -p zen-server -- serve --config examples/zen-serve.toml
```

On first start the server prints a one-time **claim token**. The first signed ACL (`POST /v1/acl/put`) must carry it, and that pins the first admin. Plain HTTP only for now: put it behind a TLS reverse proxy.

## Development

```sh
cargo test                                                     # includes byte-exact test vectors
cargo clippy --all-targets --all-features -- -D warnings
cargo check -p zen-core -p zen-proto --target wasm32-unknown-unknown
cargo run -p zen-core --example gen_vectors --features test-utils   # regenerate spec/test-vectors
```

## License

MIT
