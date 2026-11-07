//! Guards the `--json-usage` catalog against unversioned changes.
//!
//! The test stores a SHA-256 digest of the catalog's canonical JSON, not the catalog itself,
//! so the repository carries no large generated file.
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::process::{Command, Stdio};

/// Expected schema version of the CLI catalog.
const EXPECTED_SCHEMA_VERSION: &str = "0.1.1";

/// Digest of the canonical catalog for the current `schema_version`.
const CATALOG_SHA256: &str = "86313dd4bac35e08a933ab86d7be5d5cd2073a5bd2bca42dd43e4e5066896fb7";

/// Rebuilds every object with its keys sorted, so the digest does not depend on whether
/// `serde_json`'s `preserve_order` feature is enabled by the crates built alongside this one.
fn canonical(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let sorted: BTreeMap<&String, Value> = map.iter().map(|(key, value)| (key, canonical(value))).collect();
            Value::Object(sorted.into_iter().map(|(key, value)| (key.clone(), value)).collect::<Map<_, _>>())
        }
        Value::Array(items) => Value::Array(items.iter().map(canonical).collect()),
        other => other.clone(),
    }
}

/// The top-level `version` is the crate version (`CARGO_PKG_VERSION`), not catalog content,
/// so it is removed before hashing.
fn catalog_digest(mut catalog: Value) -> String {
    catalog.as_object_mut().expect("catalog is a JSON object").remove("version");
    let text = serde_json::to_string(&canonical(&catalog)).expect("catalog serializes");
    Sha256::digest(text.as_bytes()).iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Compares the current `--json-usage` canonical digest against the recorded SHA-256 golden constant.
#[test]
fn usage_catalog_matches_recorded_digest() {
    let output =
        Command::new(env!("CARGO_BIN_EXE_terrio")).arg("--json-usage").stdin(Stdio::null()).output().expect("run terrio --json-usage");
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let live: Value = serde_json::from_slice(&output.stdout).expect("catalog is JSON");

    assert_eq!(
        live["schema_version"], EXPECTED_SCHEMA_VERSION,
        "the --json-usage catalog version changed without updating EXPECTED_SCHEMA_VERSION in usage_golden.rs"
    );

    let digest = catalog_digest(live);
    assert_eq!(
        digest, CATALOG_SHA256,
        "the --json-usage catalog changed. Bump schema_version (patch, e.g. 0.1.1 -> 0.1.2) in \
         usage.rs, the schema const and title in schemas/cli-usage-v0.1.schema.json, the version \
         expectations in tests/usage_golden.rs, tests/json_usage.rs, tests/ui.rs, and \
         python/tests/test_cli.py, then set CATALOG_SHA256 in \
         crates/terrio-cli/tests/usage_golden.rs to the new digest in the same commit"
    );
}
