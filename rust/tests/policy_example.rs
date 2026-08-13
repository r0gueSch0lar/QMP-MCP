//! Guards the shared, operator-facing `policy.example.yaml` (the Command Policy file format
//! reference): it must always parse cleanly with this implementation's loader, and yield exactly
//! the lists it documents. The TypeScript suite (`policy-example.test.ts`) asserts the SAME file
//! with the SAME expectation, so the example cannot drift out of either parser's accepted shape —
//! the ADR-0012 posture applied to the config surface.

use std::path::Path;

use qmp_mcp::policy::load_policy_file;

#[test]
fn policy_example_parses_into_exactly_the_documented_lists() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../policy.example.yaml");
    let (allow, deny) = load_policy_file(path.to_str().unwrap()).unwrap_or_else(|e| {
        panic!(
            "policy.example.yaml must parse with the Rust loader: {}",
            e.0
        )
    });

    assert_eq!(
        allow,
        vec!["query-pci", "query-fdsets", "query-tpm", "query-rocker"],
        "allow list diverged from the documented example"
    );
    assert_eq!(
        deny,
        vec!["system_powerdown"],
        "deny list diverged from the documented example"
    );
}
