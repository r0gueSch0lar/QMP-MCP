//! Shared parity fixtures (ADR-0012): assert the Rust store-name allowlist
//! reproduces the language-neutral golden corpus at `../testdata/store-name/*.json`
//! verdict-for-verdict — the SAME corpus the TypeScript loader
//! (`../../test/store-name-parity.test.ts`) asserts. The name allowlist is the
//! security-critical option-injection guard (ADR-0006): a name is a single safe path
//! segment with no comma/`=`/`:`/space/leading-dash that could inject QemuOpts
//! properties downstream. A shared corpus makes any drift of that rule between the
//! two servers fail the fixture on whichever side changed.
//!
//! The corpus exercises the pure, filesystem-free name rule ONLY, via the Image
//! Store's [`assert_valid_image_name`] — the very same allowlist the ISO Store
//! shares — so the reason substrings are store-label-agnostic. Realpath containment
//! (symlink escape, missing/absent store directory) is filesystem-dependent and stays
//! in each implementation's unit tests. Image FORMAT validation needs no fixture: it
//! is a closed enum on both sides (serde / zod), rejected at the schema boundary; and
//! the NUL-byte branch is not cleanly representable as JSON, so it is pinned by a unit
//! test on each side instead.

use std::path::{Path, PathBuf};

use qmp_mcp::instance::image_store::assert_valid_image_name;
use serde::Deserialize;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Fixture {
    /// The bare name to validate.
    name: String,
    /// Whether the shared allowlist should accept it.
    expected_valid: bool,
    /// Substrings the rejection message must contain (empty for a valid name).
    #[serde(default)]
    reason_contains: Vec<String>,
}

fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("testdata")
        .join("store-name")
}

#[test]
fn rust_store_name_allowlist_reproduces_the_shared_corpus() {
    let dir = fixtures_dir();
    let mut entries: Vec<PathBuf> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("cannot read fixtures dir {}: {e}", dir.display()))
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .collect();
    entries.sort();

    assert!(
        entries.len() >= 12,
        "expected a representative store-name corpus, found {} fixtures in {}",
        entries.len(),
        dir.display()
    );

    for path in &entries {
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        let raw = std::fs::read_to_string(path).unwrap();
        let fixture: Fixture =
            serde_json::from_str(&raw).unwrap_or_else(|e| panic!("{name}: invalid fixture: {e}"));

        let verdict = assert_valid_image_name(&fixture.name);
        assert_eq!(
            verdict.is_ok(),
            fixture.expected_valid,
            "{name}: name {:?} expected valid={}, got {:?}",
            fixture.name,
            fixture.expected_valid,
            verdict
        );

        match verdict {
            Ok(()) => assert!(
                fixture.reason_contains.is_empty(),
                "{name}: a valid name must not carry reasonContains"
            ),
            Err(err) => {
                for needle in &fixture.reason_contains {
                    assert!(
                        err.0.contains(needle),
                        "{name}: reason {:?} is missing expected substring {:?}",
                        err.0,
                        needle
                    );
                }
            }
        }
    }
}
