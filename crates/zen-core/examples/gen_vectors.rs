//! Regenerate `spec/test-vectors/*.json`.
//! Run: `cargo run -p zen-core --example gen_vectors --features test-utils`

fn main() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../spec/test-vectors");
    std::fs::create_dir_all(&dir).expect("create spec/test-vectors");
    for (name, value) in zen_core::vectors::generate().expect("generate vectors") {
        let text = serde_json::to_string_pretty(&value).expect("serialize") + "\n";
        std::fs::write(dir.join(name), text).expect("write vector file");
        println!("wrote {name}");
    }
}
