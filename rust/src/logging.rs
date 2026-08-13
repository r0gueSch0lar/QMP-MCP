//! The server's own diagnostic logger, built on `tracing`.
//!
//! IMPORTANT: like the TypeScript server, all output goes to **stderr**. In stdio
//! transport mode stdout carries the MCP JSON-RPC stream and must never be
//! polluted by log lines. The minimum severity honours `QMP_MCP_LOG_LEVEL`,
//! per-subsystem overrides honour `QMP_MCP_LOG_FILTER` (both resolved by
//! [`crate::config`]), and when `QMP_MCP_LOG_FILE` is set every emitted line is
//! additionally appended to that file — stderr always stays on.

use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::Path;
use std::sync::{Arc, Mutex};

use tracing::{Level, Metadata};
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::Layer;

use crate::config::LogLevel;

/// Map a tracing target (a Rust module path) onto the shared subsystem vocabulary
/// (`config::LOG_SUBSYSTEMS`) that `QMP_MCP_LOG_FILTER` keys on. The mapping mirrors the
/// TypeScript variant's per-file logger scoping: recording and the serial bridge live inside
/// the orchestrator module here, so — like the TS side, which scopes them explicitly — they
/// log under `orchestrator`. Targets outside the crate (dependency logs) return `None` and
/// follow the global `QMP_MCP_LOG_LEVEL` only.
pub fn subsystem_for_target(target: &str) -> Option<&'static str> {
    // The bin crate root (`main.rs`) logs under the crate name itself.
    if target == "qmp_mcp" || target.starts_with("qmp_mcp::server") {
        return Some("server");
    }
    if target.starts_with("qmp_mcp::instance::download")
        || target.starts_with("qmp_mcp::instance::catalog")
    {
        return Some("download");
    }
    if target.starts_with("qmp_mcp::instance") {
        return Some("orchestrator");
    }
    if target.starts_with("qmp_mcp::qemu") {
        return Some("qmp");
    }
    if target.starts_with("qmp_mcp::viewer") {
        return Some("viewer");
    }
    if target.starts_with("qmp_mcp::policy") {
        return Some("policy");
    }
    None
}

/// Severity ordering shared with the TS logger's `SEVERITY` map (`TRACE` folds into debug).
fn severity(level: &Level) -> u8 {
    match *level {
        Level::TRACE | Level::DEBUG => 0,
        Level::INFO => 1,
        Level::WARN => 2,
        Level::ERROR => 3,
    }
}

fn min_severity(level: LogLevel) -> u8 {
    match level {
        LogLevel::Debug => 0,
        LogLevel::Info => 1,
        LogLevel::Warning => 2,
        LogLevel::Error => 3,
    }
}

/// Open `QMP_MCP_LOG_FILE` for appending, fail-closed: the file's directory must already
/// exist (the server never creates log directories), and the path must not be a directory.
/// The error strings mirror the TypeScript `openLogFile`.
fn open_log_file(path: &str) -> Result<File, String> {
    let parent = match Path::new(path).parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    };
    match std::fs::metadata(parent) {
        Err(_) => {
            return Err(format!(
                "QMP_MCP_LOG_FILE: directory \"{}\" does not exist (create it first; the server does not create log directories).",
                parent.display()
            ));
        }
        Ok(meta) if !meta.is_dir() => {
            return Err(format!(
                "QMP_MCP_LOG_FILE: \"{}\" is not a directory.",
                parent.display()
            ));
        }
        Ok(_) => {}
    }
    if std::fs::metadata(path).is_ok_and(|m| m.is_dir()) {
        return Err(format!(
            "QMP_MCP_LOG_FILE: \"{path}\" is a directory, not a file."
        ));
    }
    OpenOptions::new()
        .append(true)
        .create(true)
        .open(path)
        .map_err(|err| format!("QMP_MCP_LOG_FILE: cannot open \"{path}\": {err}"))
}

/// A `MakeWriter` that always writes to stderr and, when configured, also appends the same
/// bytes to the log file. File-write failures are swallowed: a full disk must degrade the
/// file sink, never the server (stderr keeps working either way).
#[derive(Clone)]
struct TeeMakeWriter {
    file: Option<Arc<Mutex<File>>>,
}

struct TeeWriter {
    file: Option<Arc<Mutex<File>>>,
}

impl Write for TeeWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        io::stderr().write_all(buf)?;
        if let Some(file) = &self.file {
            if let Ok(mut file) = file.lock() {
                let _ = file.write_all(buf);
            }
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        io::stderr().flush()?;
        if let Some(file) = &self.file {
            if let Ok(mut file) = file.lock() {
                let _ = file.flush();
            }
        }
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for TeeMakeWriter {
    type Writer = TeeWriter;

    fn make_writer(&'a self) -> Self::Writer {
        TeeWriter {
            file: self.file.clone(),
        }
    }
}

/// Install the global `tracing` subscriber: stderr (never stdout), no ANSI colour codes, the
/// configured global level, per-subsystem `QMP_MCP_LOG_FILTER` overrides, and the optional
/// `QMP_MCP_LOG_FILE` tee. Call once, early in `main`, after the config has been loaded;
/// fails closed (with a message naming the variable) on an unusable log file.
pub fn init(
    level: LogLevel,
    filters: &BTreeMap<String, LogLevel>,
    log_file: Option<&str>,
) -> Result<(), String> {
    let file = match log_file {
        None => None,
        Some(path) => Some(Arc::new(Mutex::new(open_log_file(path)?))),
    };
    let filters = filters.clone();
    let filter = tracing_subscriber::filter::filter_fn(move |meta: &Metadata<'_>| {
        let min = subsystem_for_target(meta.target())
            .and_then(|subsystem| filters.get(subsystem))
            .copied()
            .unwrap_or(level);
        severity(meta.level()) >= min_severity(min)
    });
    let fmt_layer = tracing_subscriber::fmt::layer()
        .with_writer(TeeMakeWriter { file })
        .with_ansi(false)
        .with_filter(filter);
    tracing_subscriber::registry().with(fmt_layer).init();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn targets_map_onto_the_shared_subsystem_vocabulary() {
        assert_eq!(subsystem_for_target("qmp_mcp"), Some("server"));
        assert_eq!(
            subsystem_for_target("qmp_mcp::instance::orchestrator"),
            Some("orchestrator")
        );
        assert_eq!(
            subsystem_for_target("qmp_mcp::instance::download"),
            Some("download")
        );
        assert_eq!(
            subsystem_for_target("qmp_mcp::instance::catalog"),
            Some("download")
        );
        assert_eq!(
            subsystem_for_target("qmp_mcp::qemu::real_driver"),
            Some("qmp")
        );
        assert_eq!(subsystem_for_target("qmp_mcp::viewer"), Some("viewer"));
        assert_eq!(subsystem_for_target("qmp_mcp::policy"), Some("policy"));
        // Dependency targets follow the global level only.
        assert_eq!(subsystem_for_target("hyper::proto"), None);
        assert_eq!(subsystem_for_target("rmcp"), None);
    }

    #[test]
    fn severities_mirror_the_ts_ordering() {
        assert!(severity(&Level::ERROR) > severity(&Level::WARN));
        assert!(severity(&Level::WARN) > severity(&Level::INFO));
        assert!(severity(&Level::INFO) > severity(&Level::DEBUG));
        assert_eq!(severity(&Level::TRACE), severity(&Level::DEBUG));
        assert_eq!(min_severity(LogLevel::Warning), severity(&Level::WARN));
    }

    #[test]
    fn open_log_file_fails_closed_on_a_missing_directory() {
        let missing = std::env::temp_dir().join(format!(
            "qmp-log-missing-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let path = missing.join("server.log");
        let err = open_log_file(path.to_str().unwrap()).unwrap_err();
        assert!(
            err.contains("QMP_MCP_LOG_FILE"),
            "must name the variable: {err}"
        );
        assert!(err.contains("does not exist"), "must say why: {err}");
    }

    #[test]
    fn open_log_file_appends_into_an_existing_directory() {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "qmp-log-ok-{}-{}-{seq}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("server.log");
        {
            let mut file = open_log_file(path.to_str().unwrap()).unwrap();
            file.write_all(b"one\n").unwrap();
        }
        {
            let mut file = open_log_file(path.to_str().unwrap()).unwrap();
            file.write_all(b"two\n").unwrap();
        }
        let content = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            content, "one\ntwo\n",
            "the file sink must append, not truncate"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
