//! Shared parity fixtures (ADR-0012): assert the Rust `ffmpeg -encoders` parser reproduces the
//! language-neutral golden corpus at `testdata/ffmpeg-detection/*.json` — the SAME corpus the
//! TypeScript suite (`typescript/test/ffmpeg-detection-parity.test.ts`) asserts. The recording
//! capability gate (ADR-0017) keys off this parse, so drift here would make the two variants
//! disagree about whether a codec is available in the same ffmpeg build.
//!
//! Each fixture is `{ description?, output, expectedEncoders }`, where `output` is captured
//! `ffmpeg -encoders` text and `expectedEncoders` is the sorted, de-duplicated name set.

use std::path::{Path, PathBuf};

use qmp_mcp::instance::orchestrator::parse_ffmpeg_encoders;
use serde::Deserialize;

#[derive(Deserialize)]
struct Fixture {
    #[allow(dead_code)]
    description: Option<String>,
    output: String,
    #[serde(rename = "expectedEncoders")]
    expected_encoders: Vec<String>,
}

fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../testdata/ffmpeg-detection")
}

#[test]
fn rust_parser_reproduces_the_shared_ffmpeg_detection_corpus() {
    let dir = fixtures_dir();
    let mut entries: Vec<PathBuf> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("cannot read fixtures dir {}: {e}", dir.display()))
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .collect();
    entries.sort();

    assert!(
        entries.len() >= 5,
        "expected a representative ffmpeg-detection corpus, found {} fixtures in {}",
        entries.len(),
        dir.display()
    );

    for path in &entries {
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        let raw = std::fs::read_to_string(path).unwrap();
        let fixture: Fixture =
            serde_json::from_str(&raw).unwrap_or_else(|e| panic!("{name}: invalid fixture: {e}"));

        let mut got: Vec<String> = parse_ffmpeg_encoders(&fixture.output).into_iter().collect();
        got.sort();
        assert_eq!(
            got, fixture.expected_encoders,
            "{name}: parsed encoder set diverged from the shared corpus"
        );
    }
}
