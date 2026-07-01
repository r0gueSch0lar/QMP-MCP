//! Shared parity fixtures (ADR-0012): assert the Rust argv generator reproduces the
//! language-neutral golden corpus at `../testdata/argv/*.json` byte-for-byte. Each
//! fixture is `{ description?, spec, options, expectedArgv }`, where `expectedArgv`
//! uses placeholders for the non-deterministic fragments — `{{QMP_SOCKET}}` for the
//! QMP socket path and `{{IMAGE_DIR}}`/`{{ISO_DIR}}` for the realpath-resolved
//! Store directories — which this loader substitutes back before comparing (the
//! same scheme the TypeScript loader in `../../test/argv-parity.test.ts` uses).
//!
//! The same corpus is asserted by BOTH implementations, so any unintentional argv
//! drift on either side fails the fixture on whichever changed.

use std::path::{Path, PathBuf};

use qmp_mcp::config::PortRange;
use qmp_mcp::instance::hardware_spec::{build_argv, parse_hardware_spec, Accel, ArgvOptions};
use serde::Deserialize;

/// A fixed, deterministic QMP socket path stand-in. It is interpolated verbatim
/// (never touches the filesystem), so a constant suffices; the loader rewrites it
/// to `{{QMP_SOCKET}}` before comparing.
const SOCKET: &str = "/run/qmp-mcp/qmp.sock";

#[derive(Deserialize)]
struct Fixture {
    spec: serde_json::Value,
    options: FixtureOptions,
    #[serde(rename = "expectedArgv")]
    expected_argv: Vec<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct FixtureOptions {
    accel: Accel,
    #[serde(default)]
    hostfwd_port_range: Option<PortRangeJson>,
    #[serde(default)]
    allow_host_net: bool,
    #[serde(default)]
    max_memory_mb: Option<u32>,
    #[serde(default)]
    max_vcpus: Option<u32>,
    #[serde(default)]
    allow_raw_args: bool,
}

#[derive(Deserialize)]
struct PortRangeJson {
    low: u16,
    high: u16,
}

/// A best-effort self-cleaning temp directory (no external crate).
struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(tag: &str) -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path =
            std::env::temp_dir().join(format!("qmp-argv-{}-{tag}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&path).unwrap();
        Self { path }
    }

    /// The canonical (symlink-resolved) path as a string — this is what the argv
    /// generator's containment boundary produces, so it is what we substitute.
    fn real(&self) -> String {
        std::fs::canonicalize(&self.path)
            .unwrap()
            .to_string_lossy()
            .into_owned()
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// Rewrite the non-deterministic fragments back to their fixture placeholders.
fn substitute(argv: Vec<String>, real_image: &str, real_iso: &str) -> Vec<String> {
    argv.into_iter()
        .map(|s| {
            s.replace(real_image, "{{IMAGE_DIR}}")
                .replace(real_iso, "{{ISO_DIR}}")
                .replace(SOCKET, "{{QMP_SOCKET}}")
        })
        .collect()
}

fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("testdata")
        .join("argv")
}

#[test]
fn rust_generator_reproduces_the_shared_argv_corpus() {
    // Real, existing Store directories are required because the containment
    // boundary realpath-resolves them; the leaf image/iso files need not exist
    // (a missing leaf yields the same in-Store path).
    let image = TempDir::new("images");
    let iso = TempDir::new("isos");
    let real_image = image.real();
    let real_iso = iso.real();

    let dir = fixtures_dir();
    let mut entries: Vec<PathBuf> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("cannot read fixtures dir {}: {e}", dir.display()))
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .collect();
    entries.sort();

    assert!(
        entries.len() >= 12,
        "expected a representative argv corpus, found {} fixtures in {}",
        entries.len(),
        dir.display()
    );

    for path in &entries {
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        let raw = std::fs::read_to_string(path).unwrap();
        let fixture: Fixture =
            serde_json::from_str(&raw).unwrap_or_else(|e| panic!("{name}: invalid fixture: {e}"));

        let spec = parse_hardware_spec(fixture.spec)
            .unwrap_or_else(|e| panic!("{name}: spec failed to validate: {}", e.0));

        let options = ArgvOptions {
            accel: fixture.options.accel,
            qmp_socket_path: SOCKET.to_string(),
            image_dir: Some(image.path.to_string_lossy().into_owned()),
            iso_dir: Some(iso.path.to_string_lossy().into_owned()),
            hostfwd_port_range: fixture.options.hostfwd_port_range.map(|r| PortRange {
                low: r.low,
                high: r.high,
            }),
            allow_host_net: fixture.options.allow_host_net,
            max_memory_mb: fixture.options.max_memory_mb,
            max_vcpus: fixture.options.max_vcpus,
            allow_raw_args: fixture.options.allow_raw_args,
        };

        let argv = build_argv(&spec, &options)
            .unwrap_or_else(|e| panic!("{name}: build_argv failed: {}", e.0));
        let got = substitute(argv, &real_image, &real_iso);

        assert_eq!(
            got, fixture.expected_argv,
            "argv mismatch for fixture {name}"
        );
    }
}
