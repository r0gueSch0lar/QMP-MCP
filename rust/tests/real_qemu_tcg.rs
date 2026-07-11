//! The single real-qemu TCG integration test (ADR-0003/0011, slice #21).
//!
//! It boots a real `qemu-system-x86_64` under TCG software emulation on a
//! server-managed QMP UNIX socket, negotiates the QMP Session through the
//! [`RealQemuDriver`], and round-trips a real `query-status`. Every other test runs
//! against the in-memory fake driver; this is the one that exercises a live qemu.
//!
//! It RUNTIME-SKIPS — returns early with a printed notice, never fails — when no
//! `qemu-system-x86_64` is on `PATH`. So it actually runs where qemu exists (the
//! `rust-dev` container) and is a harmless no-op anywhere without qemu.
//!
//! The argv is the production one: [`build_argv`] from a diskless spec forced to
//! TCG, which includes `-S` (vCPUs frozen at startup, mirroring the TS behavior).
//! Under `-S` qemu reports a non-running run-state (`prelaunch`), so the assertion
//! checks the *shape* of `query-status` (a `status` string + a `running` bool)
//! rather than a specific value.

use std::time::{SystemTime, UNIX_EPOCH};

use qmp_mcp::instance::hardware_spec::{build_argv, parse_hardware_spec, Accel, ArgvOptions};
use qmp_mcp::qemu::driver::{LaunchRequest, QemuDriver};
use qmp_mcp::qemu::real_driver::RealQemuDriver;
use serde_json::{json, Value};

/// The system emulator the default q35/max spec targets. Kept explicit (rather than
/// any `qemu-system-*`) so the generated x86 argv always matches the binary.
const QEMU_BINARY: &str = "qemu-system-x86_64";

/// Whether `binary` is an executable file on `PATH`.
fn on_path(binary: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|dir| dir.join(binary).is_file())
}

/// A unique-per-run nanosecond stamp for the temp socket directory.
fn nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock after epoch")
        .as_nanos()
}

#[tokio::test]
async fn real_qemu_boots_under_tcg_and_round_trips_query_status() {
    if !on_path(QEMU_BINARY) {
        eprintln!(
            "SKIP real_qemu_boots_under_tcg_and_round_trips_query_status: \
             no {QEMU_BINARY} on PATH (this test is a no-op without qemu)."
        );
        return;
    }

    // A per-run socket directory under the OS temp dir. The socket is a UNIX socket
    // the server owns — never network-exposed.
    let dir =
        std::env::temp_dir().join(format!("qmp-mcp-itest-{}-{}", std::process::id(), nanos()));
    std::fs::create_dir_all(&dir).expect("create temp socket dir");
    let socket_path = dir.join("qmp.sock").to_string_lossy().into_owned();

    // Build the production argv from a diskless spec forced to TCG.
    let spec = parse_hardware_spec(json!({ "accel": "tcg" })).expect("valid spec");
    let options = ArgvOptions {
        accel: Accel::Tcg,
        qmp_socket_path: socket_path.clone(),
        image_dir: None,
        iso_dir: None,
        host_share_dir: None,
        share_readonly: None,
        serial_buffer_bytes: 1 << 20,
        hostfwd_port_range: None,
        allow_host_net: false,
        max_memory_mb: None,
        max_vcpus: None,
        allow_raw_args: false,
    };
    let argv = build_argv(&spec, &options).expect("build argv");

    let driver = RealQemuDriver;
    let handle = driver
        .launch(LaunchRequest {
            binary: QEMU_BINARY.to_string(),
            argv,
            qmp_socket_path: socket_path.clone(),
        })
        .await
        .expect("qemu boots under TCG and the QMP session negotiates");

    // The acceptance criterion: a real QMP query-status round-trips.
    let status: Value = handle
        .execute("query-status", None)
        .await
        .expect("query-status round-trips over QMP");

    let run_state = status
        .get("status")
        .and_then(Value::as_str)
        .expect("query-status has a `status` string");
    assert!(!run_state.is_empty(), "empty run-state: {status}");
    assert!(
        status.get("running").and_then(Value::as_bool).is_some(),
        "query-status has a `running` bool: {status}"
    );

    // Tear the Instance down (SIGTERM → SIGKILL) and clean up.
    handle.close().await.expect("close tears down qemu");
    let _ = std::fs::remove_dir_all(&dir);

    eprintln!(
        "RAN real_qemu_boots_under_tcg_and_round_trips_query_status: \
         query-status returned status={run_state:?} (full: {status})"
    );
}
