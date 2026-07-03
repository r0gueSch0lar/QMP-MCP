//! The Command Policy engine (ADR-0003): decides whether the generic `qmp_execute`
//! tool may run a given QMP command **name**.
//!
//! A second implementation of the shared bounded context, mirroring
//! `../../typescript/src/policy/command-policy.ts` — the same three layers, in strict
//! precedence:
//!
//!   1. An immutable HARD DENYLIST ([`HARD_DENYLIST`]). A command on it is ALWAYS
//!      refused and can NEVER be re-enabled by env, a policy file, or any allowlist.
//!      This is the security boundary; it is defined exactly once here as the single
//!      source of truth.
//!   2. A curated default-safe allowlist ([`DEFAULT_ALLOWLIST`]) — read/query and a
//!      few safe control commands.
//!   3. Operator overrides — `QMP_MCP_ALLOW`/`QMP_MCP_DENY` and an optional YAML
//!      policy file (`QMP_MCP_POLICY_FILE`) — that may ADD to or REMOVE from the
//!      allowlist. They can never resurrect a hard-denied command.
//!
//! The decision is a pure function ([`decide_command`]: resolved policy + command
//! name → verdict). Resolving the policy from env + file ([`resolve_command_policy`])
//! is the only part that touches the environment or the filesystem, and it fails
//! closed with an actionable error.
//!
//! CRITICAL (ADR-0003): the policy gates command NAMES, not ARGUMENTS. A curated tool
//! or allowlisted command that can take a dangerous argument must be audited/guarded
//! separately — which is why `screendump` (an arbitrary host-file write via its
//! `filename` argument) is deliberately absent from [`DEFAULT_ALLOWLIST`] and is
//! served only by the dedicated, path-controlling screendump tool.

use std::collections::HashSet;

use crate::config::EnvMap;

/// Normalise a QMP command name for policy matching: trim surrounding whitespace and
/// lower-case it. This is what stops denylist evasion — ` migrate `, `MIGRATE`, and
/// `Human-Monitor-Command` all normalise onto their canonical entry, so neither a
/// stray space nor a case flip can slip a dangerous command past [`HARD_DENYLIST`].
/// The denylist and allowlist constants are stored in this normalised form, and every
/// lookup goes through here.
pub fn normalize_command_name(raw: &str) -> String {
    raw.trim().to_lowercase()
}

/// The immutable hard denylist — the single source of truth for commands that are
/// NEVER permitted, regardless of any allow override. Each entry can exfiltrate
/// guest/host memory, read or write arbitrary host files, open host resources, or
/// (`human-monitor-command`) run arbitrary HMP and bypass every other QMP control.
/// Defined in normalised form (see [`normalize_command_name`]); mirrors the TS
/// `HARD_DENYLIST` entry-for-entry.
pub const HARD_DENYLIST: &[&str] = &[
    // Arbitrary HMP — bypasses every other control.
    "human-monitor-command",
    // Migration: exfiltrate/inject full VM state, or steer an in-flight migration
    // (incl. postcopy / recovery) to/from an arbitrary host or network target.
    "migrate",
    "migrate-incoming",
    "migrate-set-parameters",
    "migrate-set-capabilities",
    "migrate-recover",
    "migrate-continue",
    "migrate-pause",
    "migrate-start-postcopy",
    // Xen device-state save/load: serialise/deserialise full device state to/from a
    // host file descriptor.
    "xen-save-devices-state",
    "xen-load-devices-state",
    // Memory exfiltration to a host file.
    "dump-guest-memory",
    "pmemsave",
    "memsave",
    // Host-backed object/device/backend hotplug.
    "object-add",
    "blockdev-add",
    "device_add",
    "netdev_add",
    "chardev-add",
    "chardev-change",
    // Arbitrary QOM property writes — can repoint host-backed object properties.
    "qom-set",
    // Passing host file descriptors into QEMU.
    "getfd",
    "add-fd",
    // Block backup/mirror/export/create: copy or expose a guest disk to the host
    // filesystem or the network.
    "drive-backup",
    "blockdev-backup",
    "drive-mirror",
    "blockdev-mirror",
    "blockdev-create",
    "block-export-add",
    "nbd-server-start",
    "nbd-server-add",
    // Block jobs / snapshots / resize: mutate or grow a guest disk, or create a
    // snapshot image at an arbitrary host path.
    "block-commit",
    "block-stream",
    "block_resize",
    "blockdev-snapshot",
    "blockdev-snapshot-sync",
    "blockdev-snapshot-internal-sync",
];

/// The curated default-safe allowlist: read/query commands plus a few safe control
/// commands (the same control surface already exposed as first-class tools).
/// Everything here is either read-only or a non-exfiltrating control action. Defined
/// in normalised form; disjoint from [`HARD_DENYLIST`] (an invariant asserted in the
/// tests). Mirrors the TS `DEFAULT_ALLOWLIST` entry-for-entry.
///
/// NOTE: `screendump` is intentionally NOT here. It writes an arbitrary host file at
/// the path in its `arguments`, and this policy gates command NAMES, not arguments —
/// so allowing it would let `qmp_execute` write any host file. Screenshots are exposed
/// only through the dedicated screendump tool, which server-controls the path.
pub const DEFAULT_ALLOWLIST: &[&str] = &[
    // Run-state / identity (read-only).
    "query-status",
    "query-version",
    "query-name",
    "query-uuid",
    "query-kvm",
    "query-target",
    // CPUs / topology (read-only).
    "query-cpus-fast",
    "query-cpu-definitions",
    "query-hotpluggable-cpus",
    // Memory / balloon (read-only).
    "query-memory-size-summary",
    "query-memdev",
    "query-balloon",
    // Block / storage (read-only).
    "query-block",
    "query-blockstats",
    "query-block-jobs",
    "query-named-block-nodes",
    // Devices / buses / IO (read-only).
    "query-pci",
    "query-chardev",
    "query-iothreads",
    // Machine / capabilities / introspection (read-only).
    "query-machines",
    "query-commands",
    "query-events",
    "query-qmp-schema",
    // Display / input (read-only).
    "query-vnc",
    "query-spice",
    "query-mice",
    // Safe control (already exposed as curated tools).
    "stop",
    "cont",
    "system_reset",
    "system_powerdown",
];

/// True iff `name` (already normalised) is on the immutable [`HARD_DENYLIST`].
fn is_hard_denied(name: &str) -> bool {
    HARD_DENYLIST.contains(&name)
}

/// The override lists feeding [`build_policy`] (already split into entries). Mirrors
/// the TS `PolicyOverrides`: `allow` ADDs to, `deny` REMOVEs from, the allowlist.
#[derive(Debug, Default, Clone)]
pub struct PolicyOverrides {
    /// Command names to ADD to the allowlist.
    pub allow: Vec<String>,
    /// Command names to REMOVE from the allowlist.
    pub deny: Vec<String>,
}

/// A resolved Command Policy: the effective allow and deny sets, both normalised.
/// `allow` is the default allowlist plus any allow overrides; `deny` is the union of
/// the override deny lists. The hard denylist is intentionally NOT stored here — it
/// lives only in [`HARD_DENYLIST`] so a resolved policy can never weaken it. Consumed
/// by the pure [`decide_command`].
#[derive(Debug, Clone)]
pub struct ResolvedPolicy {
    /// Normalised allowlist: defaults ∪ override allow lists.
    allow: HashSet<String>,
    /// Normalised deny overrides (env + file). Removes from the allowlist.
    deny: HashSet<String>,
}

impl Default for ResolvedPolicy {
    /// The built-in default-safe policy: the allowlist with no overrides.
    fn default() -> Self {
        build_policy(&PolicyOverrides::default())
    }
}

/// Build a [`ResolvedPolicy`] from the built-in defaults plus override lists. Pure: it
/// performs no I/O and does not read the environment. Every entry is normalised on the
/// way in, so the resolved sets compare cleanly against a normalised command name.
pub fn build_policy(overrides: &PolicyOverrides) -> ResolvedPolicy {
    let mut allow: HashSet<String> = DEFAULT_ALLOWLIST.iter().map(|s| s.to_string()).collect();
    for entry in &overrides.allow {
        let name = normalize_command_name(entry);
        if !name.is_empty() {
            allow.insert(name);
        }
    }
    let mut deny: HashSet<String> = HashSet::new();
    for entry in &overrides.deny {
        let name = normalize_command_name(entry);
        if !name.is_empty() {
            deny.insert(name);
        }
    }
    ResolvedPolicy { allow, deny }
}

/// A Command Policy verdict for a single command name. Mirrors the TS `CommandVerdict`
/// union: an allow carries the normalised command; a denial also carries an actionable
/// reason and whether it came from the immutable hard denylist.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandVerdict {
    /// The command may run; carries the normalised name forwarded to QEMU.
    Allowed {
        /// The normalised command name (e.g. `query-status`).
        command: String,
    },
    /// The command is refused; carries an actionable reason for the agent.
    Denied {
        /// The normalised command name.
        command: String,
        /// Actionable explanation suitable for returning to the agent.
        reason: String,
        /// True iff the refusal is from the immutable hard denylist.
        hard_denied: bool,
    },
}

impl CommandVerdict {
    /// Whether the command may run.
    pub fn is_allowed(&self) -> bool {
        matches!(self, CommandVerdict::Allowed { .. })
    }
}

/// Decide whether `command` may run under `policy`. PURE — the whole point of the
/// engine — so the verdict is a deterministic function of (policy, name). Layers, in
/// precedence:
///
///   1. Hard denylist  → refused, `hard_denied: true`. Checked FIRST, so no allow
///      override can ever resurrect it.
///   2. Override deny  → refused (fail-closed: deny wins over allow).
///   3. Allowlist      → allowed.
///   4. Otherwise      → refused (default-deny; the command is simply unknown).
pub fn decide_command(policy: &ResolvedPolicy, command: &str) -> CommandVerdict {
    let name = normalize_command_name(command);

    if is_hard_denied(&name) {
        return CommandVerdict::Denied {
            reason: format!(
                "QMP command \"{name}\" is permanently denied: it is on the immutable hard denylist \
                 (it can exfiltrate guest/host memory, read or write host files, open host resources, \
                 or run arbitrary HMP). It can NEVER be enabled via QMP_MCP_ALLOW or a policy file. \
                 Use a purpose-built, audited tool if you genuinely need this capability."
            ),
            command: name,
            hard_denied: true,
        };
    }

    if policy.deny.contains(&name) {
        return CommandVerdict::Denied {
            reason: format!(
                "QMP command \"{name}\" is denied by the Command Policy (it is in QMP_MCP_DENY or the \
                 policy file deny list). Remove it from the deny configuration if it is safe to run."
            ),
            command: name,
            hard_denied: false,
        };
    }

    if policy.allow.contains(&name) {
        return CommandVerdict::Allowed { command: name };
    }

    CommandVerdict::Denied {
        reason: format!(
            "QMP command \"{name}\" is not in the Command Policy allowlist. The generic qmp_execute tool \
             only runs allowlisted commands. Add it via QMP_MCP_ALLOW or the policy file allow list if \
             it is safe — but commands on the hard denylist can never be allowed."
        ),
        command: name,
        hard_denied: false,
    }
}

/// Raised when the Command Policy refuses a command requested through `qmp_execute`.
/// Distinct from [`PolicyError`] (which is about loading the policy): this is a
/// per-call denial. `hard_denied` records whether the refusal came from the immutable
/// hard denylist, so callers can surface that it can never be enabled. Mirrors the TS
/// `CommandPolicyError`.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct CommandPolicyError {
    /// The actionable denial reason (the verdict's `reason`).
    pub message: String,
    /// True iff the command is on the immutable [`HARD_DENYLIST`].
    pub hard_denied: bool,
}

impl CommandPolicyError {
    /// Build a [`CommandPolicyError`] from a denied [`CommandVerdict`]. Returns `None`
    /// for an allowed verdict (the caller only errors on a denial).
    pub fn from_verdict(verdict: &CommandVerdict) -> Option<Self> {
        match verdict {
            CommandVerdict::Allowed { .. } => None,
            CommandVerdict::Denied {
                reason,
                hard_denied,
                ..
            } => Some(Self {
                message: reason.clone(),
                hard_denied: *hard_denied,
            }),
        }
    }
}

/// Raised when the Command Policy cannot be resolved — a missing or unreadable
/// `QMP_MCP_POLICY_FILE`, malformed YAML, or a file whose shape is not
/// `{ allow?: string[], deny?: string[] }`. The message always names
/// `QMP_MCP_POLICY_FILE` and the remediation; the server fails closed rather than
/// starting with a half-understood policy. Mirrors the TS `PolicyError`.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct PolicyError(pub String);

/// The strict shape of a policy file. `allow`/`deny` are optional string lists;
/// unknown top-level keys are rejected (`deny_unknown_fields`) so a typo like
/// `allows:` fails loudly instead of silently doing nothing — the parity of the TS
/// zod `.strict()`.
#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct PolicyFile {
    #[serde(default)]
    allow: Vec<String>,
    #[serde(default)]
    deny: Vec<String>,
}

/// Read and parse the YAML policy file at `path`. Fails closed with a [`PolicyError`]
/// that names `QMP_MCP_POLICY_FILE` on any problem: the file cannot be read, the YAML
/// is malformed, or its shape is not `{ allow?: string[], deny?: string[] }`. Returns
/// the raw (un-normalised) allow/deny lists. Mirrors the TS `loadPolicyFile`.
pub fn load_policy_file(path: &str) -> Result<(Vec<String>, Vec<String>), PolicyError> {
    let text = std::fs::read_to_string(path).map_err(|err| {
        PolicyError(format!(
            "QMP_MCP_POLICY_FILE could not be read: {path} ({err}). Point it at a readable YAML file, \
             or unset it to use the built-in default policy."
        ))
    })?;

    let value: serde_yaml_ng::Value = serde_yaml_ng::from_str(&text).map_err(|err| {
        PolicyError(format!(
            "QMP_MCP_POLICY_FILE is not valid YAML: {path} ({err}). Fix the syntax, or unset it to \
             use the built-in default policy."
        ))
    })?;

    // An empty file (or one that is only comments) parses to Null — treat it as an
    // empty policy, exactly as the TS server does (`parsed == null ? {}`).
    let value = if value.is_null() {
        serde_yaml_ng::Value::Mapping(serde_yaml_ng::Mapping::new())
    } else {
        value
    };

    let file: PolicyFile = serde_yaml_ng::from_value(value).map_err(|err| {
        PolicyError(format!(
            "QMP_MCP_POLICY_FILE has the wrong shape: {path}. It must be a YAML mapping with optional \
             \"allow\" and \"deny\" lists of command-name strings, e.g. `allow: [query-pci]`. ({err})"
        ))
    })?;

    Ok((file.allow, file.deny))
}

/// Split a comma-separated override env var into trimmed, non-empty entries (the
/// parity of the TS `splitList`).
fn split_list(raw: Option<&str>) -> Vec<String> {
    match raw {
        None => Vec::new(),
        Some(value) => value
            .split(',')
            .map(str::trim)
            .filter(|entry| !entry.is_empty())
            .map(str::to_string)
            .collect(),
    }
}

/// Resolve the effective Command Policy from the environment: the built-in defaults,
/// overlaid with `QMP_MCP_ALLOW`/`QMP_MCP_DENY` and, when `QMP_MCP_POLICY_FILE` is set,
/// the YAML file's allow/deny lists. The only impure entry point (reads env + the
/// filesystem). Returns a [`PolicyError`] — fail-closed — if the policy file is
/// missing, unreadable, or malformed. Mirrors the TS `resolveCommandPolicy`.
///
/// Hard-denied commands named in an allow override are kept (they stay denied at
/// decision time) but logged as a warning, so an operator who tried to enable one
/// learns it was ignored rather than silently mis-trusting their config.
pub fn resolve_command_policy(env: &EnvMap) -> Result<ResolvedPolicy, PolicyError> {
    let mut allow = split_list(env.get("QMP_MCP_ALLOW").map(String::as_str));
    let mut deny = split_list(env.get("QMP_MCP_DENY").map(String::as_str));

    let file_path = env
        .get("QMP_MCP_POLICY_FILE")
        .map(|v| v.trim())
        .filter(|v| !v.is_empty());
    if let Some(path) = file_path {
        let (file_allow, file_deny) = load_policy_file(path)?;
        allow.extend(file_allow);
        deny.extend(file_deny);
    }

    for entry in &allow {
        if is_hard_denied(&normalize_command_name(entry)) {
            tracing::warn!(
                "Command Policy: \"{}\" is on the immutable hard denylist and cannot be allowed; \
                 ignoring its allow override. It remains denied.",
                entry.trim()
            );
        }
    }

    Ok(build_policy(&PolicyOverrides { allow, deny }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn defaults() -> ResolvedPolicy {
        ResolvedPolicy::default()
    }

    // --- invariants (mirror the TS "Command Policy invariants" suite) ---

    #[test]
    fn hard_denylist_and_default_allowlist_are_disjoint() {
        for cmd in DEFAULT_ALLOWLIST {
            assert!(!is_hard_denied(cmd), "{cmd} is in both lists");
        }
    }

    #[test]
    fn every_hard_denylist_entry_is_already_normalised() {
        for cmd in HARD_DENYLIST {
            assert_eq!(*cmd, normalize_command_name(cmd));
        }
    }

    #[test]
    fn every_default_allowlist_entry_is_already_normalised() {
        for cmd in DEFAULT_ALLOWLIST {
            assert_eq!(*cmd, normalize_command_name(cmd));
        }
    }

    #[test]
    fn includes_every_required_command_in_the_hard_set() {
        for cmd in [
            "human-monitor-command",
            "migrate",
            "migrate-incoming",
            "migrate-set-parameters",
            "dump-guest-memory",
            "pmemsave",
            "memsave",
            "object-add",
            "blockdev-add",
            "device_add",
            "netdev_add",
            "chardev-add",
            "chardev-change",
            "getfd",
            "add-fd",
        ] {
            assert!(is_hard_denied(cmd), "{cmd} must be hard-denied");
        }
    }

    #[test]
    fn also_hard_denies_the_widened_host_file_backstop_set() {
        for cmd in [
            "xen-save-devices-state",
            "xen-load-devices-state",
            "qom-set",
            "block-commit",
            "block-stream",
            "block_resize",
            "blockdev-snapshot",
            "blockdev-snapshot-sync",
            "blockdev-snapshot-internal-sync",
            "migrate-recover",
            "migrate-continue",
            "migrate-pause",
            "migrate-start-postcopy",
        ] {
            assert!(is_hard_denied(cmd), "{cmd} must be hard-denied");
        }
    }

    // --- hard denylist (immutable) ---

    #[test]
    fn refuses_every_hard_denied_command_under_the_default_policy() {
        for cmd in HARD_DENYLIST {
            match decide_command(&defaults(), cmd) {
                CommandVerdict::Denied { hard_denied, .. } => assert!(hard_denied, "{cmd}"),
                other => panic!("{cmd} should be denied, got {other:?}"),
            }
        }
    }

    #[test]
    fn still_refuses_every_hard_denied_command_when_an_allow_override_tries_to_enable_it() {
        for cmd in HARD_DENYLIST {
            let policy = build_policy(&PolicyOverrides {
                allow: vec![cmd.to_string()],
                deny: vec![],
            });
            match decide_command(&policy, cmd) {
                CommandVerdict::Denied {
                    hard_denied,
                    reason,
                    ..
                } => {
                    assert!(hard_denied, "{cmd}");
                    assert!(reason.contains("hard denylist"), "{cmd}: {reason}");
                }
                other => panic!("{cmd} should stay denied, got {other:?}"),
            }
        }
    }

    #[test]
    fn still_refuses_a_hard_denied_command_when_qmp_mcp_allow_tries_to_enable_it() {
        let env: EnvMap = [(
            "QMP_MCP_ALLOW".to_string(),
            "human-monitor-command, migrate".to_string(),
        )]
        .into_iter()
        .collect();
        let policy = resolve_command_policy(&env).unwrap();
        for cmd in ["human-monitor-command", "migrate"] {
            match decide_command(&policy, cmd) {
                CommandVerdict::Denied { hard_denied, .. } => assert!(hard_denied),
                other => panic!("{cmd} should stay denied, got {other:?}"),
            }
        }
    }

    // --- default allowlist & default-deny ---

    #[test]
    fn allows_a_default_allowlisted_command() {
        assert_eq!(
            decide_command(&defaults(), "query-status"),
            CommandVerdict::Allowed {
                command: "query-status".to_string()
            }
        );
    }

    #[test]
    fn denies_an_unknown_command_by_default_not_as_a_hard_denial() {
        match decide_command(&defaults(), "totally-made-up-command") {
            CommandVerdict::Denied {
                hard_denied,
                reason,
                ..
            } => {
                assert!(!hard_denied);
                assert!(reason.contains("not in the Command Policy allowlist"));
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    // --- screendump is NOT generically executable (the name-vs-argument gate) ---

    #[test]
    fn screendump_is_absent_from_the_default_allowlist() {
        assert!(!DEFAULT_ALLOWLIST.contains(&"screendump"));
    }

    #[test]
    fn denies_screendump_under_the_default_policy_default_deny_not_hard() {
        match decide_command(&defaults(), "screendump") {
            CommandVerdict::Denied {
                hard_denied,
                reason,
                ..
            } => {
                assert!(!hard_denied);
                assert!(reason.contains("not in the Command Policy allowlist"));
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    // --- case / whitespace evasion is blocked ---

    #[test]
    fn treats_case_and_whitespace_variants_as_the_hard_denied_canonical_command() {
        for evasion in [
            " migrate ",
            "MIGRATE",
            "Migrate",
            "  Human-Monitor-Command  ",
            "HUMAN-MONITOR-COMMAND",
            "Device_Add",
        ] {
            match decide_command(&defaults(), evasion) {
                CommandVerdict::Denied { hard_denied, .. } => assert!(hard_denied, "{evasion}"),
                other => panic!("{evasion} should be denied, got {other:?}"),
            }
        }
    }

    #[test]
    fn normalises_an_allowlisted_command_with_stray_case_space_and_forwards_canonical_name() {
        assert_eq!(
            decide_command(&defaults(), "  Query-Status  "),
            CommandVerdict::Allowed {
                command: "query-status".to_string()
            }
        );
    }

    // --- env overrides ---

    #[test]
    fn qmp_mcp_allow_adds_a_safe_command_that_was_previously_default_denied() {
        assert!(!decide_command(&defaults(), "query-rocker").is_allowed());
        let env: EnvMap = [("QMP_MCP_ALLOW".to_string(), "query-rocker".to_string())]
            .into_iter()
            .collect();
        let policy = resolve_command_policy(&env).unwrap();
        assert_eq!(
            decide_command(&policy, "query-rocker"),
            CommandVerdict::Allowed {
                command: "query-rocker".to_string()
            }
        );
    }

    #[test]
    fn qmp_mcp_deny_removes_a_command_from_the_allowlist() {
        assert!(decide_command(&defaults(), "system_reset").is_allowed());
        let env: EnvMap = [("QMP_MCP_DENY".to_string(), "system_reset".to_string())]
            .into_iter()
            .collect();
        let policy = resolve_command_policy(&env).unwrap();
        match decide_command(&policy, "system_reset") {
            CommandVerdict::Denied {
                hard_denied,
                reason,
                ..
            } => {
                assert!(!hard_denied);
                assert!(reason.contains("denied by the Command Policy"));
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn deny_wins_over_allow_when_a_command_is_in_both() {
        let env: EnvMap = [
            ("QMP_MCP_ALLOW".to_string(), "query-rocker".to_string()),
            ("QMP_MCP_DENY".to_string(), "query-rocker".to_string()),
        ]
        .into_iter()
        .collect();
        let policy = resolve_command_policy(&env).unwrap();
        assert!(!decide_command(&policy, "query-rocker").is_allowed());
    }

    // --- YAML policy file overrides ---

    /// A best-effort self-cleaning temp file (no external crate).
    struct TempFile {
        path: std::path::PathBuf,
    }

    impl TempFile {
        fn write(tag: &str, body: &str) -> Self {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "qmp-policy-{}-{tag}-{nanos}.yaml",
                std::process::id()
            ));
            std::fs::write(&path, body).unwrap();
            Self { path }
        }

        fn path(&self) -> String {
            self.path.to_string_lossy().into_owned()
        }
    }

    impl Drop for TempFile {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.path);
        }
    }

    fn env_with(name: &str, value: &str) -> EnvMap {
        [(name.to_string(), value.to_string())]
            .into_iter()
            .collect()
    }

    #[test]
    fn honours_the_file_allow_and_deny_lists() {
        let file = TempFile::write("ok", "allow:\n  - query-rocker\ndeny:\n  - system_reset\n");
        let policy =
            resolve_command_policy(&env_with("QMP_MCP_POLICY_FILE", &file.path())).unwrap();
        assert!(decide_command(&policy, "query-rocker").is_allowed());
        assert!(!decide_command(&policy, "system_reset").is_allowed());
    }

    #[test]
    fn keeps_a_hard_denied_command_denied_even_when_the_file_allows_it() {
        let file = TempFile::write("hard", "allow:\n  - migrate\n  - human-monitor-command\n");
        let policy =
            resolve_command_policy(&env_with("QMP_MCP_POLICY_FILE", &file.path())).unwrap();
        for cmd in ["migrate", "human-monitor-command"] {
            match decide_command(&policy, cmd) {
                CommandVerdict::Denied { hard_denied, .. } => assert!(hard_denied, "{cmd}"),
                other => panic!("{cmd} should stay denied, got {other:?}"),
            }
        }
    }

    #[test]
    fn merges_file_and_env_overrides_deny_still_wins() {
        let file = TempFile::write("merge", "allow:\n  - query-rocker\n");
        let env: EnvMap = [
            ("QMP_MCP_POLICY_FILE".to_string(), file.path()),
            ("QMP_MCP_DENY".to_string(), "query-rocker".to_string()),
        ]
        .into_iter()
        .collect();
        let policy = resolve_command_policy(&env).unwrap();
        assert!(!decide_command(&policy, "query-rocker").is_allowed());
    }

    #[test]
    fn treats_an_empty_file_as_a_defaults_only_policy() {
        let file = TempFile::write("empty", "");
        let policy =
            resolve_command_policy(&env_with("QMP_MCP_POLICY_FILE", &file.path())).unwrap();
        assert!(decide_command(&policy, "query-status").is_allowed());
        assert!(!decide_command(&policy, "query-rocker").is_allowed());
    }

    #[test]
    fn fails_closed_on_a_missing_policy_file_naming_the_variable() {
        let missing = std::env::temp_dir()
            .join("qmp-policy-does-not-exist-xyz.yaml")
            .to_string_lossy()
            .into_owned();
        let err = resolve_command_policy(&env_with("QMP_MCP_POLICY_FILE", &missing)).unwrap_err();
        assert!(
            err.0.contains("QMP_MCP_POLICY_FILE could not be read"),
            "{}",
            err.0
        );
    }

    #[test]
    fn fails_closed_on_malformed_yaml_naming_the_variable() {
        let file = TempFile::write("bad", "allow: \"unterminated\n");
        let err =
            resolve_command_policy(&env_with("QMP_MCP_POLICY_FILE", &file.path())).unwrap_err();
        assert!(
            err.0.contains("QMP_MCP_POLICY_FILE is not valid YAML"),
            "{}",
            err.0
        );
    }

    #[test]
    fn fails_closed_on_a_wrong_shaped_file_allow_is_not_a_list() {
        let file = TempFile::write("shape", "allow: query-status\n");
        let err =
            resolve_command_policy(&env_with("QMP_MCP_POLICY_FILE", &file.path())).unwrap_err();
        assert!(
            err.0.contains("QMP_MCP_POLICY_FILE has the wrong shape"),
            "{}",
            err.0
        );
    }

    #[test]
    fn fails_closed_on_an_unknown_top_level_key_typo_guard() {
        let file = TempFile::write("typo", "allows:\n  - query-pci\n");
        let err =
            resolve_command_policy(&env_with("QMP_MCP_POLICY_FILE", &file.path())).unwrap_err();
        assert!(
            err.0.contains("QMP_MCP_POLICY_FILE has the wrong shape"),
            "{}",
            err.0
        );
    }

    #[test]
    fn command_policy_error_carries_the_hard_denied_flag() {
        let verdict = decide_command(&defaults(), "migrate");
        let err = CommandPolicyError::from_verdict(&verdict).unwrap();
        assert!(err.hard_denied);
        assert!(err.to_string().contains("hard denylist"));
    }
}
