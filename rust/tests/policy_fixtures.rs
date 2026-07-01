//! Shared parity fixtures (ADR-0012): assert the Rust Command Policy reproduces the
//! language-neutral golden corpus at `../testdata/policy/*.json` verdict-for-verdict —
//! the SAME corpus the TypeScript loader (`../../test/policy-parity.test.ts`) asserts.
//! Any unintentional policy drift on either side fails the fixture on whichever
//! implementation changed.
//!
//! Each fixture is `{ description?, command, arguments?, config?, expectedVerdict }`.
//! `command` is the QMP command name to decide (it may carry stray case/whitespace).
//! `arguments` is informational only: the policy gates NAMES, not arguments, so the
//! loader deliberately ignores it — fixtures carry it to document that a dangerous
//! argument never changes a name-based verdict. `config` is optional `{ allow?, deny? }`
//! overrides representing the resolved effect of QMP_MCP_ALLOW/DENY OR the YAML policy
//! file (both feed the same `build_policy`), so one field covers the env/file-override
//! cases language-neutrally; file-specific error handling stays in each implementation's
//! unit tests. `expectedVerdict` is `{ allowed, command, hardDenied?, reasonContains? }`.

use std::path::{Path, PathBuf};

use qmp_mcp::policy::{build_policy, decide_command, CommandVerdict, PolicyOverrides};
use serde::Deserialize;

#[derive(Deserialize)]
struct Fixture {
    command: String,
    #[serde(default)]
    config: Option<FixtureConfig>,
    #[serde(rename = "expectedVerdict")]
    expected: ExpectedVerdict,
}

#[derive(Deserialize, Default)]
struct FixtureConfig {
    #[serde(default)]
    allow: Vec<String>,
    #[serde(default)]
    deny: Vec<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ExpectedVerdict {
    allowed: bool,
    command: String,
    #[serde(default)]
    hard_denied: Option<bool>,
    #[serde(default)]
    reason_contains: Vec<String>,
}

fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("testdata")
        .join("policy")
}

#[test]
fn rust_policy_reproduces_the_shared_verdict_corpus() {
    let dir = fixtures_dir();
    let mut entries: Vec<PathBuf> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("cannot read fixtures dir {}: {e}", dir.display()))
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .collect();
    entries.sort();

    assert!(
        entries.len() >= 12,
        "expected a representative policy corpus, found {} fixtures in {}",
        entries.len(),
        dir.display()
    );

    for path in &entries {
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        let raw = std::fs::read_to_string(path).unwrap();
        let fixture: Fixture =
            serde_json::from_str(&raw).unwrap_or_else(|e| panic!("{name}: invalid fixture: {e}"));

        let config = fixture.config.unwrap_or_default();
        let policy = build_policy(&PolicyOverrides {
            allow: config.allow,
            deny: config.deny,
        });
        let verdict = decide_command(&policy, &fixture.command);

        match (&verdict, fixture.expected.allowed) {
            (CommandVerdict::Allowed { command }, true) => {
                assert_eq!(
                    *command, fixture.expected.command,
                    "{name}: allowed command name mismatch"
                );
                assert!(
                    fixture.expected.reason_contains.is_empty(),
                    "{name}: an allowed verdict has no reason to match"
                );
            }
            (
                CommandVerdict::Denied {
                    command,
                    reason,
                    hard_denied,
                },
                false,
            ) => {
                assert_eq!(
                    *command, fixture.expected.command,
                    "{name}: denied command name mismatch"
                );
                if let Some(expected_hard) = fixture.expected.hard_denied {
                    assert_eq!(
                        *hard_denied, expected_hard,
                        "{name}: hardDenied mismatch (reason: {reason})"
                    );
                }
                for needle in &fixture.expected.reason_contains {
                    assert!(
                        reason.contains(needle),
                        "{name}: reason is missing {needle:?}\n  reason: {reason}"
                    );
                }
            }
            (got, expected_allowed) => panic!(
                "{name}: verdict/expected mismatch (expected allowed={expected_allowed}, got {got:?})"
            ),
        }
    }
}
