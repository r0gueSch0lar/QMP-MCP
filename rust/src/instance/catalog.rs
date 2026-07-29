//! The ISO download catalog (ADR-0018): the operator-curated list of installable OS
//! images the `download_iso` tool can fetch, each keyed by a short `id` and carrying
//! one or more mirror URLs. The catalog is DATA, not code: the built-in list ships as
//! `assets/iso-catalog.json`, embedded into the binary at compile time so bare-metal
//! builds need no runtime file (the same posture as the embedded noVNC assets); an
//! operator overrides it wholesale by pointing `QMP_MCP_ISO_CATALOG` at their own JSON.
//!
//! The security-relevant validation lives here: every entry's `filename` (the bare name
//! the image is saved as inside the ISO Store) is checked against the SAME shared store
//! name allowlist the Image/ISO stores use, so a catalog can never name a file that
//! escapes the Store. Mirrors `../../typescript/src/instance/catalog.ts`.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::store_path::{assert_valid_store_name, StoreLabels};

/// Store wording used to validate a catalog entry's `filename` as a safe ISO-Store leaf.
const CATALOG_FILE_LABELS: StoreLabels = StoreLabels {
    store: "ISO Store",
    entry: "Catalog ISO filename",
    env_var: "QMP_MCP_ISO_DIR",
};

/// The built-in catalog JSON, embedded at compile time (ADR-0018). Kept byte-identical to
/// `../../typescript/assets/iso-catalog.json` (a parity test guards the two copies).
pub const BUILTIN_CATALOG_JSON: &str = include_str!("../../assets/iso-catalog.json");

/// One installable OS image in the catalog. `filename` is the bare name it is saved as in
/// the ISO Store (validated), deliberately decoupled from the URL basename so the stored
/// name stays stable and clean. `mirrors` are tried in order by the downloader.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
pub struct CatalogEntry {
    /// Short stable key the `download_iso` tool selects this image by (e.g. `ubuntu`).
    pub id: String,
    /// Human-facing distro name (e.g. `Ubuntu`).
    pub name: String,
    /// Optional edition/variant note (e.g. `24.04.2 LTS Desktop`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub edition: Option<String>,
    /// Optional guest architecture the image targets (e.g. `x86_64`). Informational.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arch: Option<String>,
    /// The bare name the image is saved as inside the ISO Store (validated store name).
    pub filename: String,
    /// One or more download URLs, tried in order until one succeeds.
    pub mirrors: Vec<String>,
}

/// The on-disk catalog document: `{ "isos": [ ... ] }`. Extra top-level keys (`$comment`,
/// `version`) are ignored, so the file can carry human notes without breaking parsing.
#[derive(Debug, Deserialize)]
struct RawCatalog {
    #[serde(default)]
    isos: Vec<CatalogEntry>,
}

/// A validated catalog plus a note of where it came from (`built-in` or a file path), for
/// the `list_iso_catalog` / `get_download` reports.
#[derive(Debug, Clone)]
pub struct Catalog {
    /// Provenance: `built-in` for the embedded list, else the resolved file path.
    pub source: String,
    /// The validated entries, in catalog order.
    pub entries: Vec<CatalogEntry>,
}

/// Raised when a catalog file cannot be read or fails validation (malformed JSON, empty,
/// duplicate id, unsafe filename, bad mirror URL). The message is always actionable and
/// names the offending entry. Mirrors the TS `CatalogError`.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct CatalogError(pub String);

impl Catalog {
    /// Find the entry with the given `id`, if any.
    pub fn find(&self, id: &str) -> Option<&CatalogEntry> {
        self.entries.iter().find(|e| e.id == id)
    }

    /// A comma-separated list of the available ids, for an actionable "unknown id" message.
    pub fn ids_hint(&self) -> String {
        self.entries
            .iter()
            .map(|e| e.id.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// True when `id` is a single safe token: a leading alphanumeric then alphanumerics, dot,
/// underscore, or hyphen (the same shape as a store name — no whitespace/injection chars).
fn is_valid_id(id: &str) -> bool {
    let mut chars = id.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_alphanumeric())
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// Parse and validate a catalog from its JSON text, tagging it with `source` for reports.
/// Fails closed (with an actionable, entry-naming message) on malformed JSON, an empty list,
/// a duplicate/invalid id, a `filename` that is not a safe ISO-Store leaf, or a mirror that is
/// not an `http(s)` URL — so a bad catalog is rejected at load, never at download time.
pub fn parse_catalog(json: &str, source: &str) -> Result<Catalog, CatalogError> {
    let raw: RawCatalog = serde_json::from_str(json)
        .map_err(|e| CatalogError(format!("ISO catalog ({source}) is not valid JSON: {e}")))?;
    if raw.isos.is_empty() {
        return Err(CatalogError(format!(
            "ISO catalog ({source}) contains no entries (expected a non-empty \"isos\" array)."
        )));
    }
    let mut seen: Vec<&str> = Vec::with_capacity(raw.isos.len());
    for entry in &raw.isos {
        if !is_valid_id(&entry.id) {
            return Err(CatalogError(format!(
                "ISO catalog ({source}) has an invalid id {:?} — ids must be a single token of \
                 letters, digits, dot, underscore, or hyphen.",
                entry.id
            )));
        }
        if seen.contains(&entry.id.as_str()) {
            return Err(CatalogError(format!(
                "ISO catalog ({source}) has a duplicate id \"{}\".",
                entry.id
            )));
        }
        seen.push(&entry.id);
        if entry.name.trim().is_empty() {
            return Err(CatalogError(format!(
                "ISO catalog entry \"{}\" ({source}) has an empty name.",
                entry.id
            )));
        }
        // The filename must be a safe single-segment ISO-Store leaf (no traversal/injection),
        // so a downloaded image can never be written outside the Store.
        assert_valid_store_name(&entry.filename, &CATALOG_FILE_LABELS).map_err(|e| {
            CatalogError(format!(
                "ISO catalog entry \"{}\" ({source}) has an invalid filename: {}",
                entry.id, e.0
            ))
        })?;
        if entry.mirrors.is_empty() {
            return Err(CatalogError(format!(
                "ISO catalog entry \"{}\" ({source}) has no mirrors (expected at least one URL).",
                entry.id
            )));
        }
        for url in &entry.mirrors {
            let ok = (url.starts_with("http://") || url.starts_with("https://"))
                && !url.chars().any(|c| c.is_whitespace() || c.is_control());
            if !ok {
                return Err(CatalogError(format!(
                    "ISO catalog entry \"{}\" ({source}) has an invalid mirror URL {:?} — mirrors \
                     must be http(s) URLs with no whitespace.",
                    entry.id, url
                )));
            }
        }
    }
    Ok(Catalog {
        source: source.to_string(),
        entries: raw.isos,
    })
}

/// Load the catalog from `path` (`QMP_MCP_ISO_CATALOG`) or fall back to the embedded built-in
/// list when it is `None`. A set-but-unreadable path fails closed with an actionable message
/// naming the variable, rather than silently falling back — consistent with the fail-closed
/// config posture. Mirrors the TS `loadCatalog`.
pub fn load_catalog(path: Option<&str>) -> Result<Catalog, CatalogError> {
    match path {
        None => parse_catalog(BUILTIN_CATALOG_JSON, "built-in"),
        Some(p) => {
            let json = std::fs::read_to_string(p).map_err(|e| {
                CatalogError(format!(
                    "cannot read ISO catalog \"{p}\" (QMP_MCP_ISO_CATALOG): {e}. Point it at a \
                     readable JSON file, or leave it unset to use the built-in list."
                ))
            })?;
            parse_catalog(&json, p)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_catalog_parses_and_covers_the_expected_distros() {
        let catalog = load_catalog(None).unwrap();
        assert_eq!(catalog.source, "built-in");
        // The full requested distro set is present, keyed by stable id.
        for id in [
            "ubuntu",
            "linuxmint",
            "zorinos",
            "popos",
            "elementaryos",
            "fedora",
            "opensuse",
            "archlinux",
            "manjaro",
            "endeavouros",
            "gentoo",
            "nixos",
            "voidlinux",
            "slackware",
            "debian",
            "rhel",
            "cachyos",
            "rockylinux",
            "alpine",
            "lubuntu",
            "xubuntu",
            "bodhi",
            "tails",
            "qubesos",
        ] {
            assert!(
                catalog.find(id).is_some(),
                "built-in catalog missing id {id}"
            );
        }
        assert_eq!(catalog.entries.len(), 24);
        // Every entry has at least one mirror and a valid store-name filename (parse_catalog
        // enforces this, so a successful load already proves it — assert the shape anyway).
        for e in &catalog.entries {
            assert!(!e.mirrors.is_empty());
            assert!(assert_valid_store_name(&e.filename, &CATALOG_FILE_LABELS).is_ok());
        }
    }

    #[test]
    fn rejects_malformed_empty_duplicate_and_unsafe_entries() {
        assert!(parse_catalog("{ not json", "t")
            .unwrap_err()
            .0
            .contains("not valid JSON"));
        assert!(parse_catalog(r#"{"isos":[]}"#, "t")
            .unwrap_err()
            .0
            .contains("no entries"));
        // Duplicate id.
        let dup = r#"{"isos":[
            {"id":"a","name":"A","filename":"a.iso","mirrors":["https://x/a.iso"]},
            {"id":"a","name":"A2","filename":"a2.iso","mirrors":["https://x/a2.iso"]}
        ]}"#;
        assert!(parse_catalog(dup, "t")
            .unwrap_err()
            .0
            .contains("duplicate id"));
        // A traversing filename is rejected by the shared store-name allowlist.
        let bad_file = r#"{"isos":[
            {"id":"a","name":"A","filename":"../escape.iso","mirrors":["https://x/a.iso"]}
        ]}"#;
        assert!(parse_catalog(bad_file, "t")
            .unwrap_err()
            .0
            .contains("invalid filename"));
        // A non-http mirror is rejected.
        let bad_url = r#"{"isos":[
            {"id":"a","name":"A","filename":"a.iso","mirrors":["file:///etc/passwd"]}
        ]}"#;
        assert!(parse_catalog(bad_url, "t")
            .unwrap_err()
            .0
            .contains("invalid mirror URL"));
    }

    #[test]
    fn find_and_ids_hint() {
        let catalog = load_catalog(None).unwrap();
        assert_eq!(catalog.find("archlinux").unwrap().name, "Arch Linux");
        assert!(catalog.find("nope").is_none());
        assert!(catalog.ids_hint().contains("ubuntu"));
    }
}
