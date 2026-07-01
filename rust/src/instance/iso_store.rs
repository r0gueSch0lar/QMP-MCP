//! The ISO Store (ADR-0006): a separate, *read-only* directory that holds
//! installation/boot ISO media, kept distinct from the read-write Image Store so
//! install media can never be modified. ISOs are referenced by *name* within it.
//!
//! This slice contributes only the read-only resolution the argv generator needs:
//! the ISO-Store-flavoured wrapper over the shared containment boundary
//! ([`resolve_in_store`]) — the SAME implementation the Image Store uses,
//! specialised by [`ISO_LABELS`]. The `IsoStore` listing lands in a later slice,
//! mirroring `../../src/instance/iso-store.ts`.

use super::store_path::{assert_valid_store_name, resolve_in_store, StoreError, StoreLabels};

/// ISO-Store wording for the shared containment boundary.
pub const ISO_LABELS: StoreLabels = StoreLabels {
    store: "ISO Store",
    entry: "ISO",
    env_var: "QMP_MCP_ISO_DIR",
};

/// Validate that an ISO name is a single, safe path segment (ISO-Store-flavoured
/// view of the shared allowlist). Mirrors the TS `assertValidIsoName`.
pub fn assert_valid_iso_name(name: &str) -> Result<(), StoreError> {
    assert_valid_store_name(name, &ISO_LABELS)
}

/// Resolve an ISO name against the ISO Store directory and return its safe absolute
/// path, or a [`StoreError`]. Delegates to the shared containment boundary — the
/// same code the Image Store uses. Mirrors `resolveIsoPath`.
pub fn resolve_iso_path(name: &str, store_dir: &str) -> Result<String, StoreError> {
    resolve_in_store(name, store_dir, &ISO_LABELS)
}
