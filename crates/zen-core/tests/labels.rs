//! spec/labels.md and `labels::ALL` must list exactly the same labels.

use std::collections::BTreeSet;
use zen_core::labels;

#[test]
fn registry_matches_code() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../spec/labels.md");
    let text = std::fs::read_to_string(path).expect("spec/labels.md");
    // Every backticked `zen/v1/...` string in the registry.
    let in_spec: BTreeSet<&str> = text
        .split('`')
        .skip(1)
        .step_by(2)
        .filter(|s| s.starts_with("zen/v1/") || *s == "zen/v1")
        .collect();
    let in_code: BTreeSet<&str> = labels::ALL.iter().copied().collect();
    assert_eq!(
        in_code.len(),
        labels::ALL.len(),
        "duplicate label in labels::ALL"
    );
    assert_eq!(in_spec, in_code, "spec/labels.md and labels::ALL differ");
}
