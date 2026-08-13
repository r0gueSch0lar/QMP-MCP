//! Guards the operator-facing `.env.example` against drift from the Rust side, mirroring the
//! TypeScript `env-example.test.ts`: every `QMP_MCP_*` environment variable the Rust source
//! references must be documented in the shared repo-root `.env.example`, so the file stays an
//! exhaustive, copy-pasteable reference for both implementations. A new env var added to a
//! future slice fails this test until it is listed.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// Collect every `QMP_MCP_[A-Z0-9_]+` token in `text` into `vars` (no regex dependency: scan
/// for the prefix and extend while the var-name alphabet continues).
fn collect_env_vars(text: &str, vars: &mut BTreeSet<String>) {
    const PREFIX: &str = "QMP_MCP_";
    for (start, _) in text.match_indices(PREFIX) {
        let rest = &text[start + PREFIX.len()..];
        let extra = rest
            .bytes()
            .take_while(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || *b == b'_')
            .count();
        // Trim trailing underscores so a prefix-only mention (`QMP_MCP_`) doesn't count.
        let token = format!("{PREFIX}{}", &rest[..extra]);
        let token = token.trim_end_matches('_');
        if token.len() > PREFIX.len() {
            vars.insert(token.to_string());
        }
    }
}

/// Recursively collect `.rs` files under `dir`.
fn source_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            source_files(&path, out);
        } else if path.extension().is_some_and(|x| x == "rs") {
            out.push(path);
        }
    }
}

#[test]
fn env_example_documents_every_variable_the_rust_source_reads() {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut files = Vec::new();
    source_files(&manifest.join("src"), &mut files);

    let mut referenced = BTreeSet::new();
    for file in &files {
        collect_env_vars(&std::fs::read_to_string(file).unwrap(), &mut referenced);
    }
    assert!(
        !referenced.is_empty(),
        "expected the Rust source to reference QMP_MCP_* variables"
    );

    let documented = std::fs::read_to_string(manifest.join("../.env.example")).unwrap();
    let missing: Vec<&String> = referenced
        .iter()
        .filter(|name| !documented.contains(name.as_str()))
        .collect();
    assert!(
        missing.is_empty(),
        "QMP_MCP_* variables read by the Rust source but missing from .env.example: {missing:?}"
    );
}
