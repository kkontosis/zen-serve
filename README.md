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

## Status

Milestone 1 (`spec/` + `zen-core`) is in place. Next: `zen-server` with the embedded backend, then FoundationDB.

## Development

```sh
cargo test                                                     # includes byte-exact test vectors
cargo clippy --all-targets --all-features -- -D warnings
cargo check -p zen-core --target wasm32-unknown-unknown
cargo run -p zen-core --example gen_vectors --features test-utils   # regenerate spec/test-vectors
```

## License

MIT
