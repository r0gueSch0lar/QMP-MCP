//! Configuration surface for the server, parsed from `QMP_MCP_*` environment
//! variables. This module is a **pure function of its input env map**: it never
//! reads the process environment directly, which keeps it trivially unit-testable
//! (mirroring `../../typescript/src/config.ts`).
//!
//! Fail-closed: any value that is present but invalid returns a [`ConfigError`]
//! naming the offending variable and the allowed values, rather than silently
//! falling back to a default. The HTTP transport additionally refuses to start
//! without auth (ADR-0005): if it is selected with no credentials configured and
//! no explicit insecure override, [`load_config`] fails here, before any server is
//! booted.
//!
//! This is a second implementation of the shared bounded context (ADR-0011): the
//! surface, defaults, and error wording track the TypeScript server so the two can
//! be cross-validated against the same inputs.

use std::collections::HashMap;
use std::path::Path;

/// An environment map: variable name to raw value. Absent keys read as unset; a
/// key present with an empty string is distinct from absent, exactly as in the
/// TypeScript server's `process.env` handling.
pub type EnvMap = HashMap<String, String>;

/// Default Event Buffer capacity — the bounded ring of recent QMP async events
/// (`QMP_MCP_EVENT_BUFFER_SIZE`). Mirrors `DEFAULT_EVENT_BUFFER_SIZE` in the TS
/// server (issue #12).
pub const DEFAULT_EVENT_BUFFER_SIZE: u32 = 256;

/// Fallback `qemu-system-*` binary: the arch that `machine_arch` degrades an unknown
/// (or x86) `machine` to, i.e. what `qemu_binary_for_machine` returns for anything not
/// mapped to ARM. The Orchestrator normally DERIVES the binary from the Instance's
/// `machine` (ADR-0013) and `QMP_MCP_QEMU_BINARY` overrides it; this is just the x86_64
/// historical default the map falls back to.
pub const DEFAULT_QEMU_BINARY: &str = "qemu-system-x86_64";

/// Which transport(s) the server exposes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportMode {
    /// Newline-delimited JSON-RPC over stdio. No network port, so auth-free.
    Stdio,
    /// Streamable HTTP transport (arrives in a later slice).
    Http,
    /// Both stdio and HTTP concurrently.
    Both,
}

impl TransportMode {
    /// Allowed values, in canonical order, for actionable error messages.
    const ALLOWED: &'static str = "stdio, http, both";

    fn parse(raw: &str) -> Option<Self> {
        match raw.to_lowercase().as_str() {
            "stdio" => Some(Self::Stdio),
            "http" => Some(Self::Http),
            "both" => Some(Self::Both),
            _ => None,
        }
    }

    /// Canonical lowercase spelling.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Stdio => "stdio",
            Self::Http => "http",
            Self::Both => "both",
        }
    }

    /// Whether this mode exposes the HTTP transport (and thus needs auth).
    pub fn exposes_http(&self) -> bool {
        matches!(self, Self::Http | Self::Both)
    }
}

impl std::fmt::Display for TransportMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Minimum severity emitted by the server's own logger.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogLevel {
    Debug,
    Info,
    Warning,
    Error,
}

impl LogLevel {
    const ALLOWED: &'static str = "debug, info, warning, error";

    fn parse(raw: &str) -> Option<Self> {
        match raw.to_lowercase().as_str() {
            "debug" => Some(Self::Debug),
            "info" => Some(Self::Info),
            "warning" => Some(Self::Warning),
            "error" => Some(Self::Error),
            _ => None,
        }
    }
}

/// Which provider guards the HTTP transport when auth is enabled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthMode {
    ApiKey,
    Jwt,
}

impl AuthMode {
    const ALLOWED: &'static str = "apikey, jwt";

    fn parse(raw: &str) -> Option<Self> {
        match raw.to_lowercase().as_str() {
            "apikey" => Some(Self::ApiKey),
            "jwt" => Some(Self::Jwt),
            _ => None,
        }
    }
}

/// An inclusive `[low, high]` host-port range. A user-mode port-forward's
/// `hostPort` must fall inside it (ADR-0009), so the agent can never bind an
/// arbitrary or privileged host port.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PortRange {
    /// Lowest host port a forward may bind (inclusive).
    pub low: u16,
    /// Highest host port a forward may bind (inclusive).
    pub high: u16,
}

/// Default host-port range for user-mode port-forwards: the IANA non-privileged
/// range 1024-65535 (ADR-0008/0009), so a forward never needs root.
pub const DEFAULT_HOSTFWD_PORT_RANGE: PortRange = PortRange {
    low: 1024,
    high: 65535,
};

/// The validated configuration. Every field mirrors `Config` in
/// `../../typescript/src/config.ts` (same defaults and validation).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// Which transport(s) the server should expose.
    pub transport: TransportMode,
    /// Minimum severity emitted by the server's own logger.
    pub log_level: LogLevel,
    /// Address the HTTP transport binds to.
    pub http_host: String,
    /// TCP port the HTTP transport listens on.
    pub http_port: u16,
    /// Path the MCP endpoint is served from.
    pub http_endpoint: String,
    /// Browser origins permitted by the DNS-rebinding/CORS guard.
    pub allowed_origins: Vec<String>,
    /// Which provider guards the HTTP transport when auth is enabled.
    pub auth_mode: AuthMode,
    /// Valid API keys for [`AuthMode::ApiKey`], trimmed with empties dropped.
    pub api_keys: Vec<String>,
    /// Signing secret for [`AuthMode::Jwt`], or `None` when unset.
    pub jwt_secret: Option<String>,
    /// When true, the HTTP transport runs unauthenticated (local dev only).
    pub allow_insecure: bool,
    /// Absolute path of the read-write Image Store directory (ADR-0006).
    pub image_dir: String,
    /// Absolute path of the read-only ISO Store directory (ADR-0006).
    pub iso_dir: String,
    /// Explicit `qemu-system-*` binary override (`QMP_MCP_QEMU_BINARY`), or `None` when
    /// unset — in which case the binary is DERIVED per-instance from the spec's
    /// `machine` (`qemu-system-x86_64` for q35/pc, `qemu-system-aarch64` for
    /// virt/raspi*; issue #18). Set it only to force a specific emulator for every
    /// Instance (e.g. a custom build).
    pub qemu_binary_override: Option<String>,
    /// Hard cap, in GiB, on the virtual size of a created disk image.
    pub max_disk_gb: u32,
    /// Hard cap, in MiB, on a Hardware Spec's `memoryMb` (issue #9).
    pub max_memory_mb: u32,
    /// Hard cap on a Hardware Spec's `vcpus` (issue #9).
    pub max_vcpus: u32,
    /// Inclusive host-port range a user-mode forward's `hostPort` must fall in.
    pub hostfwd_port_range: PortRange,
    /// When true, host-level guest networking (`tap`/`bridge`) is permitted.
    pub allow_host_net: bool,
    /// When true, `create_instance` auto-starts the Guest by issuing QMP `cont`
    /// right after launch (`QMP_MCP_AUTO_START`, issue #8). Default false: the Guest
    /// loads paused at the `-S` startup pause and only runs on `resume_instance`.
    pub auto_start: bool,
    /// Capacity of the Event Buffer (issue #12).
    pub event_buffer_size: u32,
    /// When true, a Hardware Spec's `extraArgs` are appended to the argv (ADR-0002).
    pub allow_raw_args: bool,
    /// The password gating the noVNC Viewer (ADR-0010), or `None` when unset.
    pub viewer_password: Option<String>,
    /// Address the noVNC Viewer's HTTP server binds to.
    pub viewer_host: String,
    /// TCP port the noVNC Viewer listens on.
    pub viewer_port: u16,
}

/// Raised when an environment variable is present but holds an invalid value, or
/// when the HTTP transport is selected without the auth it requires. The message
/// always names the variable(s) and the remediation (mirrors the TS `ConfigError`).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct ConfigError(pub String);

/// Look up a variable, returning `None` for an absent key (matching `undefined`
/// in the TS server). A key present with an empty string reads as `Some("")`.
fn get<'a>(env: &'a EnvMap, name: &str) -> Option<&'a str> {
    env.get(name).map(String::as_str)
}

/// Parse an enum-valued env var. Treats undefined or the exact empty string as
/// unset and returns the fallback; otherwise validates (case-insensitively) and
/// fails closed with an actionable message on mismatch. Note: like the TS server,
/// the raw value is lower-cased but NOT trimmed, so `" http"` is rejected.
fn parse_enum<T>(
    var: &str,
    raw: Option<&str>,
    allowed: &str,
    fallback: T,
    parse: impl Fn(&str) -> Option<T>,
) -> Result<T, ConfigError> {
    match raw {
        None | Some("") => Ok(fallback),
        Some(value) => parse(value).ok_or_else(|| {
            ConfigError(format!(
                "{var} must be one of: {allowed} (got \"{value}\")."
            ))
        }),
    }
}

/// Parse a required-non-empty string, trimming surrounding whitespace. Undefined
/// or blank reads as the fallback.
fn parse_string(raw: Option<&str>, fallback: &str) -> String {
    match raw {
        None => fallback.to_string(),
        Some(value) => {
            let trimmed = value.trim();
            if trimmed.is_empty() {
                fallback.to_string()
            } else {
                trimmed.to_string()
            }
        }
    }
}

/// True when the trimmed value is a non-empty run of ASCII digits (the `^\d+$`
/// the TS server uses before numeric coercion).
fn is_all_digits(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
}

/// Parse a TCP port. Undefined or blank reads as the fallback; otherwise requires
/// a base-10 integer in 1..=65535 and fails closed on anything else.
fn parse_port(var: &str, raw: Option<&str>, fallback: u16) -> Result<u16, ConfigError> {
    let value = match raw {
        None => return Ok(fallback),
        Some(v) if v.trim().is_empty() => return Ok(fallback),
        Some(v) => v.trim(),
    };
    let err = || {
        ConfigError(format!(
            "{var} must be an integer port in 1..65535 (got \"{value}\")."
        ))
    };
    if !is_all_digits(value) {
        return Err(err());
    }
    match value.parse::<u32>() {
        Ok(port) if (1..=65535).contains(&port) => Ok(port as u16),
        _ => Err(err()),
    }
}

/// Parse a boolean flag. Accepts `true`/`false` case-insensitively (trimmed);
/// undefined or the exact empty string reads as the fallback. Fails closed on any
/// other value so a typo never reads as a silent "false".
fn parse_boolean(var: &str, raw: Option<&str>, fallback: bool) -> Result<bool, ConfigError> {
    match raw {
        None | Some("") => Ok(fallback),
        Some(value) => match value.trim().to_lowercase().as_str() {
            "true" => Ok(true),
            "false" => Ok(false),
            _ => Err(ConfigError(format!(
                "{var} must be \"true\" or \"false\" (got \"{value}\")."
            ))),
        },
    }
}

/// Parse a positive-integer env var (e.g. a size cap). Undefined or blank reads as
/// the fallback; otherwise requires a base-10 integer >= 1 and fails closed on
/// anything else.
fn parse_positive_int(var: &str, raw: Option<&str>, fallback: u32) -> Result<u32, ConfigError> {
    let value = match raw {
        None => return Ok(fallback),
        Some(v) if v.trim().is_empty() => return Ok(fallback),
        Some(v) => v.trim(),
    };
    let err = || {
        ConfigError(format!(
            "{var} must be a positive integer >= 1 (got \"{value}\")."
        ))
    };
    if !is_all_digits(value) {
        return Err(err());
    }
    match value.parse::<u32>() {
        Ok(n) if n >= 1 => Ok(n),
        _ => Err(err()),
    }
}

/// Parse a `LOW-HIGH` host-port range. Undefined or blank reads as the fallback;
/// otherwise requires two base-10 integers with `1 <= LOW <= HIGH <= 65535` and
/// fails closed on anything else (garbage, reversed bounds, out-of-range, missing
/// dash, surrounding spaces).
fn parse_port_range(
    var: &str,
    raw: Option<&str>,
    fallback: PortRange,
) -> Result<PortRange, ConfigError> {
    let value = match raw {
        None => return Ok(fallback),
        Some(v) if v.trim().is_empty() => return Ok(fallback),
        Some(v) => v.trim(),
    };
    let err = || {
        ConfigError(format!(
            "{var} must be a host-port range \"LOW-HIGH\" with \
             1 <= LOW <= HIGH <= 65535 (got \"{value}\")."
        ))
    };
    // Exactly one '-' with all-digit, non-empty sides (the `^(\d+)-(\d+)$` rule).
    let (lo, hi) = match value.split_once('-') {
        Some(parts) => parts,
        None => return Err(err()),
    };
    if !is_all_digits(lo) || !is_all_digits(hi) {
        return Err(err());
    }
    let (low, high) = match (lo.parse::<u32>(), hi.parse::<u32>()) {
        (Ok(low), Ok(high)) => (low, high),
        _ => return Err(err()),
    };
    if low < 1 || high > 65535 || low > high {
        return Err(err());
    }
    Ok(PortRange {
        low: low as u16,
        high: high as u16,
    })
}

/// Split a comma-separated list into trimmed, non-empty entries. Undefined (or an
/// empty string) yields an empty list.
fn parse_list(raw: Option<&str>) -> Vec<String> {
    match raw {
        None => Vec::new(),
        Some(value) => value
            .split(',')
            .map(str::trim)
            .filter(|e| !e.is_empty())
            .map(str::to_string)
            .collect(),
    }
}

/// A non-empty, trimmed env value, or `None`. Mirrors `env.X?.trim()` guarded by
/// truthiness in the TS resolvers.
fn trimmed_non_empty<'a>(env: &'a EnvMap, name: &str) -> Option<&'a str> {
    get(env, name).map(str::trim).filter(|s| !s.is_empty())
}

/// The OS temp dir, as a pure function of the env map: `TMPDIR` (trimmed, trailing
/// slash stripped) when set, else `/tmp`. Mirrors Node's `os.tmpdir()` for the
/// POSIX default used by [`resolve_image_dir`]/[`resolve_iso_dir`] as a last resort.
fn tmp_dir(env: &EnvMap) -> String {
    match trimmed_non_empty(env, "TMPDIR") {
        Some(t) => t.trim_end_matches('/').to_string(),
        None => "/tmp".to_string(),
    }
}

/// Join a base path with sub-components into a display string (mirrors `path.join`).
fn join(base: &str, parts: &[&str]) -> String {
    let mut p = Path::new(base).to_path_buf();
    for part in parts {
        p.push(part);
    }
    p.to_string_lossy().into_owned()
}

/// Resolve the read-write Image Store directory (ADR-0006/0007). An explicit
/// `QMP_MCP_IMAGE_DIR` wins; otherwise a host-agnostic default is derived from
/// `XDG_DATA_HOME`, then `HOME`, then the temp dir — so the bare-metal default
/// never assumes the Docker layout.
pub fn resolve_image_dir(env: &EnvMap) -> String {
    if let Some(explicit) = trimmed_non_empty(env, "QMP_MCP_IMAGE_DIR") {
        return explicit.to_string();
    }
    if let Some(xdg) = trimmed_non_empty(env, "XDG_DATA_HOME") {
        return join(xdg, &["qmp-mcp", "images"]);
    }
    if let Some(home) = trimmed_non_empty(env, "HOME") {
        return join(home, &[".local", "share", "qmp-mcp", "images"]);
    }
    join(&tmp_dir(env), &["qmp-mcp", "images"])
}

/// Resolve the read-only ISO Store directory (ADR-0006/0007). Mirrors
/// [`resolve_image_dir`] but is a SEPARATE directory (`isos`, not `images`).
pub fn resolve_iso_dir(env: &EnvMap) -> String {
    if let Some(explicit) = trimmed_non_empty(env, "QMP_MCP_ISO_DIR") {
        return explicit.to_string();
    }
    if let Some(xdg) = trimmed_non_empty(env, "XDG_DATA_HOME") {
        return join(xdg, &["qmp-mcp", "isos"]);
    }
    if let Some(home) = trimmed_non_empty(env, "HOME") {
        return join(home, &[".local", "share", "qmp-mcp", "isos"]);
    }
    join(&tmp_dir(env), &["qmp-mcp", "isos"])
}

/// Resolve and validate an EXPLICIT `qemu-system-*` binary override
/// (`QMP_MCP_QEMU_BINARY`), or `None` when unset or blank/whitespace-only. An explicit
/// value is trimmed and must be a **bare command name** (resolved via `PATH`) or an
/// **absolute path**, over the safe charset `[A-Za-z0-9._/+-]`.
///
/// `None` means "no override" — the Orchestrator then DERIVES the binary from the
/// per-instance `machine` (`qemu-system-x86_64` for q35/pc, `qemu-system-aarch64` for
/// virt/raspi*; see [`crate::instance::hardware_spec::qemu_binary_for_machine`], issue
/// #18), so switching guest architectures no longer needs an env flip + restart. An
/// explicit value overrides that derivation for every Instance (e.g. a custom build).
///
/// The value is exec'd as argv[0] with **no shell**, but we still fail closed on
/// whitespace, shell metacharacters, and control characters — and on a relative path
/// (`./qemu`, `build/qemu`) — so a foot-gun never reaches `exec`.
pub fn qemu_binary_override(env: &EnvMap) -> Result<Option<String>, ConfigError> {
    let value = match trimmed_non_empty(env, "QMP_MCP_QEMU_BINARY") {
        None => return Ok(None),
        Some(v) => v,
    };
    // Safe charset: ASCII letters/digits plus the path and version punctuation that
    // legitimate binary names and absolute paths use (`^[A-Za-z0-9._/+-]+$`).
    let charset_ok = value
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'/' | b'+' | b'-'));
    // A '/' is only allowed when the value is an absolute path; a bare command name
    // carries no slash. This rejects relative paths, which resolve against an
    // ambient CWD rather than a stable location.
    let shape_ok = !value.contains('/') || value.starts_with('/');
    if !charset_ok || !shape_ok {
        return Err(ConfigError(format!(
            "QMP_MCP_QEMU_BINARY must be a bare command name or an absolute path over \
             [A-Za-z0-9._/+-] (no whitespace, shell metacharacters, or control \
             characters) (got \"{value}\")."
        )));
    }
    Ok(Some(value.to_string()))
}

/// An original (untrimmed) env value that is present and not whitespace-only, or
/// `None`. Mirrors the JWT-secret / Viewer-password resolvers, which keep the
/// original string but treat a blank value as unset (fail-closed).
fn present_non_blank(env: &EnvMap, name: &str) -> Option<String> {
    match get(env, name) {
        Some(v) if !v.trim().is_empty() => Some(v.to_string()),
        _ => None,
    }
}

/// Build a validated [`Config`] from an environment map. Returns a [`ConfigError`]
/// on any invalid value, and — per ADR-0005 — when the HTTP transport is selected
/// without configured auth and without an explicit insecure override.
pub fn load_config(env: &EnvMap) -> Result<Config, ConfigError> {
    let transport = parse_enum(
        "QMP_MCP_TRANSPORT",
        get(env, "QMP_MCP_TRANSPORT"),
        TransportMode::ALLOWED,
        TransportMode::Stdio,
        TransportMode::parse,
    )?;
    let log_level = parse_enum(
        "QMP_MCP_LOG_LEVEL",
        get(env, "QMP_MCP_LOG_LEVEL"),
        LogLevel::ALLOWED,
        LogLevel::Info,
        LogLevel::parse,
    )?;
    let http_host = parse_string(get(env, "QMP_MCP_HTTP_HOST"), "127.0.0.1");
    let http_port = parse_port("QMP_MCP_HTTP_PORT", get(env, "QMP_MCP_HTTP_PORT"), 8080)?;
    let http_endpoint = parse_string(get(env, "QMP_MCP_HTTP_ENDPOINT"), "/mcp");
    let auth_mode = parse_enum(
        "QMP_MCP_AUTH",
        get(env, "QMP_MCP_AUTH"),
        AuthMode::ALLOWED,
        AuthMode::ApiKey,
        AuthMode::parse,
    )?;
    let api_keys = parse_list(get(env, "QMP_MCP_API_KEYS"));
    let jwt_secret = present_non_blank(env, "QMP_MCP_JWT_SECRET");
    let allow_insecure = parse_boolean(
        "QMP_MCP_ALLOW_INSECURE",
        get(env, "QMP_MCP_ALLOW_INSECURE"),
        false,
    )?;

    // Allowed browser origins for the DNS-rebinding/CORS guard. Default to the
    // loopback origins for the configured port; an explicit list overrides it.
    let origin_override = parse_list(get(env, "QMP_MCP_HTTP_ALLOWED_ORIGINS"));
    let allowed_origins = if origin_override.is_empty() {
        vec![
            format!("http://localhost:{http_port}"),
            format!("http://127.0.0.1:{http_port}"),
        ]
    } else {
        origin_override
    };

    // ADR-0005 fail-closed: the HTTP transport can build and control VMs, so it
    // refuses to start unauthenticated unless the operator opts in explicitly.
    if transport.exposes_http() && !allow_insecure {
        if auth_mode == AuthMode::ApiKey && api_keys.is_empty() {
            return Err(ConfigError(
                "HTTP transport requires authentication but none is configured \
                 (QMP_MCP_AUTH=apikey, QMP_MCP_API_KEYS is empty). \
                 Set QMP_MCP_API_KEYS to a comma-separated list of keys, \
                 or switch to JWT with QMP_MCP_AUTH=jwt and QMP_MCP_JWT_SECRET, \
                 or set QMP_MCP_ALLOW_INSECURE=true to run unauthenticated (local dev only)."
                    .to_string(),
            ));
        }
        if auth_mode == AuthMode::Jwt && jwt_secret.is_none() {
            return Err(ConfigError(
                "HTTP transport with QMP_MCP_AUTH=jwt requires a signing secret but \
                 QMP_MCP_JWT_SECRET is not set. \
                 Set QMP_MCP_JWT_SECRET, \
                 or set QMP_MCP_ALLOW_INSECURE=true to run unauthenticated (local dev only)."
                    .to_string(),
            ));
        }
    }

    Ok(Config {
        transport,
        log_level,
        http_host,
        http_port,
        http_endpoint,
        allowed_origins,
        auth_mode,
        api_keys,
        jwt_secret,
        allow_insecure,
        image_dir: resolve_image_dir(env),
        iso_dir: resolve_iso_dir(env),
        qemu_binary_override: qemu_binary_override(env)?,
        max_disk_gb: parse_positive_int(
            "QMP_MCP_MAX_DISK_GB",
            get(env, "QMP_MCP_MAX_DISK_GB"),
            64,
        )?,
        max_memory_mb: parse_positive_int(
            "QMP_MCP_MAX_MEMORY_MB",
            get(env, "QMP_MCP_MAX_MEMORY_MB"),
            4096,
        )?,
        max_vcpus: parse_positive_int("QMP_MCP_MAX_VCPUS", get(env, "QMP_MCP_MAX_VCPUS"), 2)?,
        hostfwd_port_range: parse_port_range(
            "QMP_MCP_HOSTFWD_PORT_RANGE",
            get(env, "QMP_MCP_HOSTFWD_PORT_RANGE"),
            DEFAULT_HOSTFWD_PORT_RANGE,
        )?,
        allow_host_net: parse_boolean(
            "QMP_MCP_ALLOW_HOST_NET",
            get(env, "QMP_MCP_ALLOW_HOST_NET"),
            false,
        )?,
        auto_start: parse_boolean("QMP_MCP_AUTO_START", get(env, "QMP_MCP_AUTO_START"), false)?,
        event_buffer_size: parse_positive_int(
            "QMP_MCP_EVENT_BUFFER_SIZE",
            get(env, "QMP_MCP_EVENT_BUFFER_SIZE"),
            DEFAULT_EVENT_BUFFER_SIZE,
        )?,
        allow_raw_args: parse_boolean(
            "QMP_MCP_ALLOW_RAW_ARGS",
            get(env, "QMP_MCP_ALLOW_RAW_ARGS"),
            false,
        )?,
        viewer_password: present_non_blank(env, "QMP_MCP_VIEWER_PASSWORD"),
        viewer_host: parse_string(get(env, "QMP_MCP_VIEWER_HOST"), "127.0.0.1"),
        viewer_port: parse_port("QMP_MCP_VIEWER_PORT", get(env, "QMP_MCP_VIEWER_PORT"), 6080)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build an [`EnvMap`] from `(name, value)` pairs.
    fn env(pairs: &[(&str, &str)]) -> EnvMap {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    /// The host-agnostic default Image/ISO Store dirs for an empty environment
    /// (temp dir is `/tmp` when `TMPDIR` is unset).
    const DEFAULT_IMAGE_DIR: &str = "/tmp/qmp-mcp/images";
    const DEFAULT_ISO_DIR: &str = "/tmp/qmp-mcp/isos";

    /// The full default config for an empty environment (stdio, no auth).
    fn defaults() -> Config {
        Config {
            transport: TransportMode::Stdio,
            log_level: LogLevel::Info,
            http_host: "127.0.0.1".into(),
            http_port: 8080,
            http_endpoint: "/mcp".into(),
            allowed_origins: vec![
                "http://localhost:8080".into(),
                "http://127.0.0.1:8080".into(),
            ],
            auth_mode: AuthMode::ApiKey,
            api_keys: vec![],
            jwt_secret: None,
            allow_insecure: false,
            image_dir: DEFAULT_IMAGE_DIR.into(),
            iso_dir: DEFAULT_ISO_DIR.into(),
            qemu_binary_override: None,
            max_disk_gb: 64,
            max_memory_mb: 4096,
            max_vcpus: 2,
            hostfwd_port_range: PortRange {
                low: 1024,
                high: 65535,
            },
            allow_host_net: false,
            auto_start: false,
            event_buffer_size: 256,
            allow_raw_args: false,
            viewer_password: None,
            viewer_host: "127.0.0.1".into(),
            viewer_port: 6080,
        }
    }

    #[test]
    fn defaults_to_stdio_no_auth_when_env_empty() {
        assert_eq!(load_config(&env(&[])).unwrap(), defaults());
    }

    #[test]
    fn reads_valid_values_and_normalises_case() {
        let cfg = load_config(&env(&[
            ("QMP_MCP_TRANSPORT", "HTTP"),
            ("QMP_MCP_LOG_LEVEL", "Debug"),
            ("QMP_MCP_AUTH", "ApiKey"),
            ("QMP_MCP_API_KEYS", "k1"),
        ]))
        .unwrap();
        assert_eq!(cfg.transport, TransportMode::Http);
        assert_eq!(cfg.log_level, LogLevel::Debug);
        assert_eq!(cfg.auth_mode, AuthMode::ApiKey);
    }

    #[test]
    fn treats_empty_string_as_unset() {
        assert_eq!(
            load_config(&env(&[("QMP_MCP_TRANSPORT", "")])).unwrap(),
            defaults()
        );
    }

    #[test]
    fn rejects_invalid_transport_naming_variable_and_allowed_values() {
        let err = load_config(&env(&[("QMP_MCP_TRANSPORT", "ftp")])).unwrap_err();
        assert!(err
            .0
            .contains("QMP_MCP_TRANSPORT must be one of: stdio, http, both"));
    }

    #[test]
    fn rejects_invalid_log_level_naming_variable() {
        let err = load_config(&env(&[("QMP_MCP_LOG_LEVEL", "verbose")])).unwrap_err();
        assert!(err.0.contains("QMP_MCP_LOG_LEVEL"));
        assert!(err.0.contains("debug, info, warning, error"));
    }

    #[test]
    fn http_host_port_endpoint_defaults_and_overrides() {
        let cfg = load_config(&env(&[
            ("QMP_MCP_TRANSPORT", "http"),
            ("QMP_MCP_API_KEYS", "k"),
        ]))
        .unwrap();
        assert_eq!(cfg.http_host, "127.0.0.1");
        assert_eq!(cfg.http_port, 8080);
        assert_eq!(cfg.http_endpoint, "/mcp");

        let cfg = load_config(&env(&[
            ("QMP_MCP_TRANSPORT", "http"),
            ("QMP_MCP_API_KEYS", "k"),
            ("QMP_MCP_HTTP_HOST", " 0.0.0.0 "),
            ("QMP_MCP_HTTP_PORT", "9000"),
            ("QMP_MCP_HTTP_ENDPOINT", " /rpc "),
        ]))
        .unwrap();
        assert_eq!(cfg.http_host, "0.0.0.0");
        assert_eq!(cfg.http_port, 9000);
        assert_eq!(cfg.http_endpoint, "/rpc");
    }

    #[test]
    fn derives_default_allowed_origins_from_port() {
        let cfg = load_config(&env(&[
            ("QMP_MCP_TRANSPORT", "http"),
            ("QMP_MCP_API_KEYS", "k"),
            ("QMP_MCP_HTTP_PORT", "9000"),
        ]))
        .unwrap();
        assert_eq!(
            cfg.allowed_origins,
            vec![
                "http://localhost:9000".to_string(),
                "http://127.0.0.1:9000".to_string()
            ]
        );
    }

    #[test]
    fn explicit_allowed_origins_override_default() {
        let cfg = load_config(&env(&[
            ("QMP_MCP_TRANSPORT", "http"),
            ("QMP_MCP_API_KEYS", "k"),
            (
                "QMP_MCP_HTTP_ALLOWED_ORIGINS",
                "https://app.example.com, https://admin.example.com ",
            ),
        ]))
        .unwrap();
        assert_eq!(
            cfg.allowed_origins,
            vec![
                "https://app.example.com".to_string(),
                "https://admin.example.com".to_string()
            ]
        );
    }

    #[test]
    fn fails_closed_on_invalid_ports() {
        for port in ["abc", "8080x", "0", "70000", "-1", "80.5"] {
            let err = load_config(&env(&[
                ("QMP_MCP_TRANSPORT", "http"),
                ("QMP_MCP_API_KEYS", "k"),
                ("QMP_MCP_HTTP_PORT", port),
            ]))
            .unwrap_err();
            assert!(
                err.0
                    .contains("QMP_MCP_HTTP_PORT must be an integer port in 1..65535"),
                "port {port:?} gave {:?}",
                err.0
            );
        }
    }

    #[test]
    fn apikey_fail_closed_names_keys_and_insecure() {
        let err = load_config(&env(&[("QMP_MCP_TRANSPORT", "http")])).unwrap_err();
        assert!(err.0.contains("QMP_MCP_API_KEYS"));
        assert!(err.0.contains("QMP_MCP_ALLOW_INSECURE"));
    }

    #[test]
    fn both_transport_fails_closed_with_no_keys() {
        assert!(load_config(&env(&[("QMP_MCP_TRANSPORT", "both")])).is_err());
    }

    #[test]
    fn keys_of_only_commas_and_whitespace_are_empty_and_fail_closed() {
        assert!(load_config(&env(&[
            ("QMP_MCP_TRANSPORT", "http"),
            ("QMP_MCP_API_KEYS", " , ,, "),
        ]))
        .is_err());
    }

    #[test]
    fn accepts_http_with_keys_trimmed() {
        let cfg = load_config(&env(&[
            ("QMP_MCP_TRANSPORT", "http"),
            ("QMP_MCP_API_KEYS", "k1, k2 ,, k3 "),
        ]))
        .unwrap();
        assert_eq!(cfg.api_keys, vec!["k1", "k2", "k3"]);
        assert_eq!(cfg.auth_mode, AuthMode::ApiKey);
    }

    #[test]
    fn permits_insecure_http_with_no_keys() {
        let cfg = load_config(&env(&[
            ("QMP_MCP_TRANSPORT", "http"),
            ("QMP_MCP_ALLOW_INSECURE", "true"),
        ]))
        .unwrap();
        assert!(cfg.allow_insecure);
        assert!(cfg.api_keys.is_empty());
    }

    #[test]
    fn jwt_fail_closed_names_secret() {
        let err = load_config(&env(&[
            ("QMP_MCP_TRANSPORT", "http"),
            ("QMP_MCP_AUTH", "jwt"),
        ]))
        .unwrap_err();
        assert!(err.0.contains("QMP_MCP_JWT_SECRET"));
    }

    #[test]
    fn jwt_whitespace_only_secret_is_unset_and_fails_closed() {
        let err = load_config(&env(&[
            ("QMP_MCP_TRANSPORT", "http"),
            ("QMP_MCP_AUTH", "jwt"),
            ("QMP_MCP_JWT_SECRET", "   "),
        ]))
        .unwrap_err();
        assert!(err.0.contains("QMP_MCP_JWT_SECRET"));
    }

    #[test]
    fn accepts_http_jwt_with_secret() {
        let cfg = load_config(&env(&[
            ("QMP_MCP_TRANSPORT", "http"),
            ("QMP_MCP_AUTH", "jwt"),
            ("QMP_MCP_JWT_SECRET", "s3cr3t"),
        ]))
        .unwrap();
        assert_eq!(cfg.auth_mode, AuthMode::Jwt);
        assert_eq!(cfg.jwt_secret.as_deref(), Some("s3cr3t"));
    }

    #[test]
    fn rejects_non_boolean_allow_insecure() {
        let err = load_config(&env(&[
            ("QMP_MCP_TRANSPORT", "http"),
            ("QMP_MCP_ALLOW_INSECURE", "yes"),
        ]))
        .unwrap_err();
        assert!(err
            .0
            .contains("QMP_MCP_ALLOW_INSECURE must be \"true\" or \"false\""));
    }

    #[test]
    fn image_store_defaults_and_overrides() {
        let cfg = load_config(&env(&[])).unwrap();
        assert_eq!(cfg.image_dir, DEFAULT_IMAGE_DIR);
        assert_eq!(cfg.max_disk_gb, 64);

        assert_eq!(
            load_config(&env(&[("QMP_MCP_IMAGE_DIR", " /srv/images ")]))
                .unwrap()
                .image_dir,
            "/srv/images"
        );
        assert_eq!(
            load_config(&env(&[("XDG_DATA_HOME", "/x/data")]))
                .unwrap()
                .image_dir,
            "/x/data/qmp-mcp/images"
        );
        assert_eq!(
            load_config(&env(&[("HOME", "/home/u")])).unwrap().image_dir,
            "/home/u/.local/share/qmp-mcp/images"
        );
    }

    #[test]
    fn max_disk_gb_reads_and_fails_closed() {
        assert_eq!(
            load_config(&env(&[("QMP_MCP_MAX_DISK_GB", "128")]))
                .unwrap()
                .max_disk_gb,
            128
        );
        assert!(load_config(&env(&[("QMP_MCP_MAX_DISK_GB", "big")]))
            .unwrap_err()
            .0
            .contains("QMP_MCP_MAX_DISK_GB must be a positive integer"));
        assert!(load_config(&env(&[("QMP_MCP_MAX_DISK_GB", "0")])).is_err());
    }

    #[test]
    fn resource_caps_defaults_and_fail_closed() {
        let cfg = load_config(&env(&[])).unwrap();
        assert_eq!(cfg.max_memory_mb, 4096);
        assert_eq!(cfg.max_vcpus, 2);
        assert_eq!(cfg.event_buffer_size, 256);

        assert_eq!(
            load_config(&env(&[("QMP_MCP_MAX_MEMORY_MB", "32768")]))
                .unwrap()
                .max_memory_mb,
            32768
        );
        assert!(load_config(&env(&[("QMP_MCP_MAX_MEMORY_MB", "lots")]))
            .unwrap_err()
            .0
            .contains("QMP_MCP_MAX_MEMORY_MB must be a positive integer"));
        assert!(load_config(&env(&[("QMP_MCP_MAX_MEMORY_MB", "0")])).is_err());

        assert_eq!(
            load_config(&env(&[("QMP_MCP_MAX_VCPUS", "16")]))
                .unwrap()
                .max_vcpus,
            16
        );
        assert!(load_config(&env(&[("QMP_MCP_MAX_VCPUS", "many")]))
            .unwrap_err()
            .0
            .contains("QMP_MCP_MAX_VCPUS must be a positive integer"));

        assert_eq!(
            load_config(&env(&[("QMP_MCP_EVENT_BUFFER_SIZE", "1024")]))
                .unwrap()
                .event_buffer_size,
            1024
        );
        assert!(load_config(&env(&[("QMP_MCP_EVENT_BUFFER_SIZE", "big")]))
            .unwrap_err()
            .0
            .contains("QMP_MCP_EVENT_BUFFER_SIZE must be a positive integer"));
    }

    #[test]
    fn allow_raw_args_defaults_closed_and_fails_closed() {
        assert!(!load_config(&env(&[])).unwrap().allow_raw_args);
        assert!(
            load_config(&env(&[("QMP_MCP_ALLOW_RAW_ARGS", "true")]))
                .unwrap()
                .allow_raw_args
        );
        assert!(
            !load_config(&env(&[("QMP_MCP_ALLOW_RAW_ARGS", "false")]))
                .unwrap()
                .allow_raw_args
        );
        assert!(load_config(&env(&[("QMP_MCP_ALLOW_RAW_ARGS", "yes")]))
            .unwrap_err()
            .0
            .contains("QMP_MCP_ALLOW_RAW_ARGS must be \"true\" or \"false\""));
    }

    #[test]
    fn iso_store_defaults_separate_and_overrides() {
        let cfg = load_config(&env(&[])).unwrap();
        assert_eq!(cfg.iso_dir, DEFAULT_ISO_DIR);
        assert_ne!(cfg.iso_dir, cfg.image_dir);

        assert_eq!(
            load_config(&env(&[("QMP_MCP_ISO_DIR", " /srv/isos ")]))
                .unwrap()
                .iso_dir,
            "/srv/isos"
        );
        assert_eq!(
            load_config(&env(&[("XDG_DATA_HOME", "/x/data")]))
                .unwrap()
                .iso_dir,
            "/x/data/qmp-mcp/isos"
        );
        assert_eq!(
            load_config(&env(&[("HOME", "/home/u")])).unwrap().iso_dir,
            "/home/u/.local/share/qmp-mcp/isos"
        );
    }

    #[test]
    fn qemu_binary_override_is_none_when_unset_or_blank() {
        // No override -> the binary is derived from the machine at launch (issue #18).
        assert_eq!(load_config(&env(&[])).unwrap().qemu_binary_override, None);
        assert_eq!(
            load_config(&env(&[("QMP_MCP_QEMU_BINARY", "")]))
                .unwrap()
                .qemu_binary_override,
            None
        );
        assert_eq!(
            load_config(&env(&[("QMP_MCP_QEMU_BINARY", "   ")]))
                .unwrap()
                .qemu_binary_override,
            None
        );
    }

    #[test]
    fn qemu_binary_explicit_override_is_honored() {
        // An explicit value overrides machine derivation for every Instance; a bare
        // name and an absolute path are both accepted and trimmed.
        assert_eq!(
            load_config(&env(&[("QMP_MCP_QEMU_BINARY", "qemu-system-aarch64")]))
                .unwrap()
                .qemu_binary_override,
            Some("qemu-system-aarch64".to_string())
        );
        assert_eq!(
            load_config(&env(&[(
                "QMP_MCP_QEMU_BINARY",
                " /usr/bin/qemu-system-riscv64 "
            )]))
            .unwrap()
            .qemu_binary_override,
            Some("/usr/bin/qemu-system-riscv64".to_string())
        );
    }

    #[test]
    fn qemu_binary_fails_closed_on_unsafe_values() {
        // Shell metacharacters / whitespace / relative paths are rejected, naming
        // the variable and the allowed form.
        for value in [
            "qemu; rm -rf",
            "qemu-system-x86_64 --enable-kvm",
            "qemu\tsystem",
            "$(rm -rf /)",
            "qemu|nc",
            "../bin/qemu-system-aarch64",
            "./qemu",
            "build/qemu-system-aarch64",
        ] {
            let err = load_config(&env(&[("QMP_MCP_QEMU_BINARY", value)])).unwrap_err();
            assert!(
                err.0.contains("QMP_MCP_QEMU_BINARY"),
                "value {value:?} gave {:?}",
                err.0
            );
        }
    }

    #[test]
    fn guest_networking_defaults_and_range_parsing() {
        let cfg = load_config(&env(&[])).unwrap();
        assert_eq!(
            cfg.hostfwd_port_range,
            PortRange {
                low: 1024,
                high: 65535
            }
        );
        assert!(!cfg.allow_host_net);

        assert_eq!(
            load_config(&env(&[("QMP_MCP_HOSTFWD_PORT_RANGE", "2000-3000")]))
                .unwrap()
                .hostfwd_port_range,
            PortRange {
                low: 2000,
                high: 3000
            }
        );
        // a single-port range (low == high) is allowed
        assert_eq!(
            load_config(&env(&[("QMP_MCP_HOSTFWD_PORT_RANGE", "8080-8080")]))
                .unwrap()
                .hostfwd_port_range,
            PortRange {
                low: 8080,
                high: 8080
            }
        );
    }

    #[test]
    fn hostfwd_range_fails_closed_naming_variable() {
        for range in [
            "abc",
            "1024",
            "1024-",
            "-65535",
            "0-65535",
            "1024-70000",
            "3000-2000",
            "1024-65535-2",
            " 1024 - 2048 ",
        ] {
            let err = load_config(&env(&[("QMP_MCP_HOSTFWD_PORT_RANGE", range)])).unwrap_err();
            assert!(
                err.0.contains("QMP_MCP_HOSTFWD_PORT_RANGE"),
                "range {range:?} gave {:?}",
                err.0
            );
        }
    }

    #[test]
    fn allow_host_net_boolean_fails_closed() {
        assert!(
            load_config(&env(&[("QMP_MCP_ALLOW_HOST_NET", "true")]))
                .unwrap()
                .allow_host_net
        );
        assert!(
            !load_config(&env(&[("QMP_MCP_ALLOW_HOST_NET", "False")]))
                .unwrap()
                .allow_host_net
        );
        assert!(load_config(&env(&[("QMP_MCP_ALLOW_HOST_NET", "yes")]))
            .unwrap_err()
            .0
            .contains("QMP_MCP_ALLOW_HOST_NET must be \"true\" or \"false\""));
    }

    #[test]
    fn auto_start_defaults_false_and_reads_true() {
        // issue #8: create_instance leaves the Guest paused unless QMP_MCP_AUTO_START.
        assert!(!load_config(&env(&[])).unwrap().auto_start);
        assert!(
            load_config(&env(&[("QMP_MCP_AUTO_START", "true")]))
                .unwrap()
                .auto_start
        );
    }

    #[test]
    fn viewer_defaults_and_reads() {
        let cfg = load_config(&env(&[])).unwrap();
        assert_eq!(cfg.viewer_host, "127.0.0.1");
        assert_eq!(cfg.viewer_port, 6080);
        assert_eq!(cfg.viewer_password, None);

        let cfg = load_config(&env(&[
            ("QMP_MCP_VIEWER_PASSWORD", "view-secret"),
            ("QMP_MCP_VIEWER_HOST", "0.0.0.0"),
            ("QMP_MCP_VIEWER_PORT", "7000"),
        ]))
        .unwrap();
        assert_eq!(cfg.viewer_password.as_deref(), Some("view-secret"));
        assert_eq!(cfg.viewer_host, "0.0.0.0");
        assert_eq!(cfg.viewer_port, 7000);
    }

    #[test]
    fn viewer_whitespace_password_is_unset() {
        assert_eq!(
            load_config(&env(&[("QMP_MCP_VIEWER_PASSWORD", "   ")]))
                .unwrap()
                .viewer_password,
            None
        );
    }

    #[test]
    fn viewer_port_fails_closed() {
        assert!(load_config(&env(&[("QMP_MCP_VIEWER_PORT", "abc")]))
            .unwrap_err()
            .0
            .contains("QMP_MCP_VIEWER_PORT must be an integer port in 1..65535"));
    }

    #[test]
    fn stdio_does_not_require_http_auth() {
        assert!(load_config(&env(&[("QMP_MCP_TRANSPORT", "stdio")])).is_ok());
        // an auth mode with no HTTP transport is harmless
        assert_eq!(
            load_config(&env(&[("QMP_MCP_AUTH", "jwt")]))
                .unwrap()
                .auth_mode,
            AuthMode::Jwt
        );
    }
}
