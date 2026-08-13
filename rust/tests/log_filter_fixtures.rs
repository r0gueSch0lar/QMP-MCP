//! Shared parity fixtures (ADR-0012): assert the Rust `QMP_MCP_LOG_FILTER` parser reproduces
//! the language-neutral golden corpus at `testdata/log-filter/*.json` — the SAME corpus the
//! TypeScript suite (`typescript/test/log-filter-parity.test.ts`) asserts. Drift here would
//! make the two variants accept different filter strings or emit different per-subsystem
//! levels.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use qmp_mcp::config::{parse_log_filter, LogLevel};
use serde::Deserialize;

#[derive(Deserialize)]
struct Fixture {
    #[allow(dead_code)]
    description: Option<String>,
    filter: String,
    expected: Option<BTreeMap<String, String>>,
    #[serde(rename = "expectError", default)]
    expect_error: bool,
    #[serde(rename = "errorContains", default)]
    error_contains: Vec<String>,
}

fn level_str(level: LogLevel) -> &'static str {
    match level {
        LogLevel::Debug => "debug",
        LogLevel::Info => "info",
        LogLevel::Warning => "warning",
        LogLevel::Error => "error",
    }
}

fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../testdata/log-filter")
}

#[test]
fn rust_parser_reproduces_the_shared_log_filter_corpus() {
    let dir = fixtures_dir();
    let mut entries: Vec<PathBuf> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("cannot read fixtures dir {}: {e}", dir.display()))
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .collect();
    entries.sort();

    assert!(
        entries.len() >= 5,
        "expected a representative log-filter corpus, found {} fixtures in {}",
        entries.len(),
        dir.display()
    );

    for path in &entries {
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        let raw = std::fs::read_to_string(path).unwrap();
        let fixture: Fixture =
            serde_json::from_str(&raw).unwrap_or_else(|e| panic!("{name}: invalid fixture: {e}"));

        let result = parse_log_filter("QMP_MCP_LOG_FILTER", Some(&fixture.filter));
        if fixture.expect_error {
            let err = match result {
                Err(err) => err,
                Ok(parsed) => panic!("{name}: expected a rejection, got a parse: {parsed:?}"),
            };
            for substring in &fixture.error_contains {
                assert!(
                    err.0.contains(substring),
                    "{name}: error message missing {substring:?}: {}",
                    err.0
                );
            }
        } else {
            let parsed: BTreeMap<String, String> = result
                .unwrap_or_else(|e| panic!("{name}: expected a parse, got: {}", e.0))
                .into_iter()
                .map(|(subsystem, level)| (subsystem, level_str(level).to_string()))
                .collect();
            assert_eq!(
                parsed,
                fixture.expected.unwrap_or_default(),
                "{name}: parsed filter diverged from the shared corpus"
            );
        }
    }
}
