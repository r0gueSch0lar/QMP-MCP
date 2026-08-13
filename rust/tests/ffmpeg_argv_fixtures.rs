//! Shared parity fixtures (ADR-0012 / ADR-0017): assert the Rust ffmpeg-argv builder reproduces
//! the language-neutral golden corpus at `../testdata/ffmpeg-argv/*.json` byte-for-byte — the SAME
//! corpus the TypeScript loader (`../../typescript/test/ffmpeg-argv-parity.test.ts`) asserts. Any
//! unintentional drift in the recording argv on either side fails the fixture on whichever changed.
//!
//! Each fixture is `{ description?, codec, crf, maxFps, pixfmt, out, expectedArgv }`. `maxFps` is
//! part of the input tuple but must NOT appear in the argv (wallclock timestamps + VFR make it a
//! capture-loop pacing cap only) — the corpus pins exactly that.

use std::path::{Path, PathBuf};

use qmp_mcp::instance::orchestrator::build_ffmpeg_args;
use serde::Deserialize;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Fixture {
    codec: String,
    crf: u32,
    max_fps: u32,
    pixfmt: String,
    out: String,
    expected_argv: Vec<String>,
}

fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("testdata")
        .join("ffmpeg-argv")
}

#[test]
fn rust_builder_reproduces_the_shared_ffmpeg_argv_corpus() {
    let dir = fixtures_dir();
    let mut entries: Vec<PathBuf> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("cannot read fixtures dir {}: {e}", dir.display()))
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .collect();
    entries.sort();

    assert!(
        entries.len() >= 5,
        "expected a representative ffmpeg-argv corpus, found {} fixtures in {}",
        entries.len(),
        dir.display()
    );

    for path in &entries {
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        let raw = std::fs::read_to_string(path).unwrap();
        let fixture: Fixture =
            serde_json::from_str(&raw).unwrap_or_else(|e| panic!("{name}: invalid fixture: {e}"));

        let argv = build_ffmpeg_args(
            &fixture.codec,
            fixture.crf,
            fixture.max_fps,
            &fixture.pixfmt,
            &fixture.out,
        );
        assert_eq!(
            argv, fixture.expected_argv,
            "argv mismatch for fixture {name}"
        );
        // maxFps must never leak into the argv as an input frame rate.
        assert!(
            !argv.iter().any(|a| a == "-framerate"),
            "fixture {name} unexpectedly emitted -framerate"
        );
    }
}
