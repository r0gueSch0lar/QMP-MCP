//! The server's own diagnostic logger, built on `tracing`.
//!
//! IMPORTANT: like the TypeScript server, all output goes to **stderr**. In stdio
//! transport mode stdout carries the MCP JSON-RPC stream and must never be
//! polluted by log lines. The minimum severity honours `QMP_MCP_LOG_LEVEL`
//! (resolved into [`LogLevel`] by [`crate::config`]).

use crate::config::LogLevel;
use tracing::Level;

/// Map the configured [`LogLevel`] onto a `tracing` [`Level`]. `warning` becomes
/// tracing's `WARN`.
fn tracing_level(level: LogLevel) -> Level {
    match level {
        LogLevel::Debug => Level::DEBUG,
        LogLevel::Info => Level::INFO,
        LogLevel::Warning => Level::WARN,
        LogLevel::Error => Level::ERROR,
    }
}

/// Install the global `tracing` subscriber at the configured level, writing to
/// stderr without ANSI colour codes. Call once, early in `main`, after the config
/// has been loaded.
pub fn init(level: LogLevel) {
    tracing_subscriber::fmt()
        .with_max_level(tracing_level(level))
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .init();
}
