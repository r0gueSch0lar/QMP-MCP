//! The shared allowlisted-store boundary (ADR-0006). Both the read-write Image
//! Store and the read-only ISO Store reference their contents by *name* only —
//! never by host path — and both must reject the exact same family of attacks:
//! absolute paths, `..`/path-separator traversal, QemuOpts option-injection
//! characters in the name, and symlink escape out of the store.
//!
//! Rather than duplicate that security-critical logic per store (duplicated
//! security code inevitably diverges — one copy is hardened while the other rots),
//! the name-validation and the realpath-containment live HERE, once.
//! [`resolve_image_path`](super::image_store::resolve_image_path) and
//! [`resolve_iso_path`](super::iso_store::resolve_iso_path) are thin wrappers over
//! [`resolve_in_store`] that only supply store-specific [`StoreLabels`]. Touch the
//! boundary here and both stores move together.
//!
//! Subdirectory policy: a store name is a SINGLE path segment — no `/` or `\`, and
//! never `.`/`..`. Nested names are rejected. This keeps every store flat, makes
//! the traversal analysis trivial (the only path component below the store is the
//! leaf itself), and is the simplest thing that is obviously correct. Mirrors
//! `../../src/instance/store-path.ts`.

use std::path::{Path, PathBuf};

use schemars::JsonSchema;
use serde::Serialize;

/// Raised for any store-boundary violation (an invalid/traversing name, a symlink
/// that escapes the store, or a missing store directory). Distinct
/// store-flavoured wrappers surface it under their own labels, but the containment
/// logic is shared. The message is always actionable (mirrors the TS
/// `ImageStoreError`/`IsoStoreError`).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct StoreError(pub String);

/// Per-store wording that turns this generic boundary's messages into actionable,
/// store-specific ones (mirrors the TS `StoreLabels`).
#[derive(Debug, Clone, Copy)]
pub struct StoreLabels {
    /// The store's display name, e.g. `Image Store` or `ISO Store`.
    pub store: &'static str,
    /// The noun for a single entry, e.g. `Disk image` or `ISO`.
    pub entry: &'static str,
    /// The env var that configures the store directory, e.g. `QMP_MCP_IMAGE_DIR`.
    pub env_var: &'static str,
}

/// Conservative single-segment allowlist for store names, expressed as the exact
/// regex source the TS server reports in its error message: a leading alphanumeric
/// followed by alphanumerics, dot, underscore, or hyphen. This is the
/// security-critical rule — it excludes the comma, `=`, `:`, space, and leading
/// `-` that would otherwise let a name inject extra `-drive`/QemuOpts properties.
pub const VALID_STORE_NAME: &str = "^[A-Za-z0-9][A-Za-z0-9._-]*$";

/// True when `name` is a single safe segment: a leading ASCII alphanumeric then
/// alphanumerics, `.`, `_`, or `-`. Hand-rolled (no regex dependency) but exactly
/// the [`VALID_STORE_NAME`] charset.
fn is_valid_store_name(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphanumeric() => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// Validate that a store name is a single, safe path segment. Pure and
/// filesystem-free: rejects empties, NUL bytes, absolute paths, `.`/`..`, and any
/// name containing a path separator with their own actionable messages, then
/// enforces the [`VALID_STORE_NAME`] allowlist (no comma/`=`/`:`/space/leading
/// dash — values that would inject QemuOpts properties downstream). Mirrors the TS
/// `assertValidStoreName`.
pub fn assert_valid_store_name(name: &str, labels: &StoreLabels) -> Result<(), StoreError> {
    let entry = labels.entry;
    if name.trim().is_empty() {
        return Err(StoreError(format!(
            "{entry} name must be a non-empty string."
        )));
    }
    if name.contains('\0') {
        return Err(StoreError(format!(
            "{entry} name \"{name}\" contains a NUL byte."
        )));
    }
    if Path::new(name).is_absolute() {
        return Err(StoreError(format!(
            "{entry} name \"{name}\" must be a bare name inside the {store}, not an absolute path.",
            store = labels.store
        )));
    }
    if name == "." || name == ".." {
        return Err(StoreError(format!(
            "{entry} name \"{name}\" is not a valid file name."
        )));
    }
    if name.contains('/') || name.contains('\\') {
        return Err(StoreError(format!(
            "{entry} name \"{name}\" must not contain a path separator; subdirectories are not \
             allowed in the {store}.",
            store = labels.store
        )));
    }
    if !is_valid_store_name(name) {
        return Err(StoreError(format!(
            "{entry} name \"{name}\" must match {pat} — a single segment of letters, digits, dot, \
             underscore, or hyphen, with no leading hyphen and no comma, '=', ':', or spaces \
             (these could inject QEMU -drive properties).",
            pat = VALID_STORE_NAME
        )));
    }
    Ok(())
}

/// Resolve a name against a store directory and return its safe absolute path, or a
/// [`StoreError`]. The containment guarantee (mirrors the TS `resolveInStore`):
///
///  1. The name passes [`assert_valid_store_name`] (single safe segment).
///  2. The store directory's real (symlink-resolved) path is computed; a missing
///     store fails closed with an actionable message naming the store's env var.
///  3. If the target already exists, its real path must stay within the store's
///     real path — so a symlink (or dangling symlink) at the leaf that points
///     outside the store is rejected rather than followed.
///
/// Because the name is a single non-`..` segment joined onto the *canonical* store
/// path, a not-yet-existing target cannot escape; the realpath check closes the
/// symlink-at-the-leaf hole for targets that do exist.
pub fn resolve_in_store(
    name: &str,
    store_dir: &str,
    labels: &StoreLabels,
) -> Result<String, StoreError> {
    assert_valid_store_name(name, labels)?;

    let real_store = std::fs::canonicalize(store_dir).map_err(|_| {
        StoreError(format!(
            "{store} directory \"{store_dir}\" does not exist or is not accessible. Create it or \
             set {env} to an existing directory.",
            store = labels.store,
            env = labels.env_var
        ))
    })?;

    let candidate: PathBuf = real_store.join(name);

    // Only existing leaves can introduce a symlink escape; a missing leaf is safe
    // by construction (single segment under the canonical store path).
    if candidate.symlink_metadata().is_ok() {
        let real = std::fs::canonicalize(&candidate).map_err(|_| {
            StoreError(format!(
                "{entry} \"{name}\" is a dangling symlink; refusing to follow it out of the {store}.",
                entry = labels.entry,
                store = labels.store
            ))
        })?;
        if real != candidate && !real.starts_with(&real_store) {
            return Err(StoreError(format!(
                "{entry} \"{name}\" resolves outside the {store} (symlink escape); refusing.",
                entry = labels.entry,
                store = labels.store
            )));
        }
    }

    Ok(candidate.to_string_lossy().into_owned())
}

/// A single entry (regular file) present in a store, as reported by a store's
/// `list`. The Image Store and ISO Store report the identical shape, so — like the
/// containment boundary above — the type lives HERE once rather than being
/// duplicated per store (mirrors the TS `ImageInfo`/`IsoInfo`, which share a shape).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct StoreEntry {
    /// The bare name to reference this entry by inside its store.
    pub name: String,
    /// On-disk (host) size in bytes — sparse images report their allocated size.
    pub size_bytes: u64,
}

/// List the regular files present in a store directory, each with its host size in
/// bytes, sorted by name. This is the shared body behind both stores' `list`
/// (mirrors the identical `list()` methods of the TS Image/ISO stores):
///
///  - Regular files ONLY: symlinks are skipped rather than followed, so a planted
///    symlink can never surface as a listable entry. Subdirectories are skipped too.
///  - A missing/inaccessible store fails closed with an actionable message naming
///    the store's env var, exactly as [`resolve_in_store`] does.
///  - A leaf that races away between `read_dir` and `stat` is silently skipped.
pub async fn list_store_files(
    dir: &str,
    labels: &StoreLabels,
) -> Result<Vec<StoreEntry>, StoreError> {
    let missing = || {
        StoreError(format!(
            "{store} directory \"{dir}\" does not exist or is not accessible. Create it or set \
             {env} to an existing directory.",
            store = labels.store,
            env = labels.env_var
        ))
    };

    let mut read_dir = tokio::fs::read_dir(dir).await.map_err(|_| missing())?;
    let mut entries: Vec<StoreEntry> = Vec::new();
    loop {
        // Stop enumerating on end-of-dir or a mid-iteration error, returning what we
        // have — the store was reachable, so this is not a fail-closed condition.
        let entry = match read_dir.next_entry().await {
            Ok(Some(entry)) => entry,
            Ok(None) | Err(_) => break,
        };
        // Skip anything that is not a regular file (symlinks report as symlink, not
        // file, so they are never followed; directories are skipped as well).
        match entry.file_type().await {
            Ok(file_type) if file_type.is_file() => {}
            _ => continue,
        }
        // `DirEntry::metadata` does not traverse symlinks, but the leaf is already a
        // regular file, so its length is the on-disk size. Skip if it raced away.
        let size_bytes = match entry.metadata().await {
            Ok(metadata) => metadata.len(),
            Err(_) => continue,
        };
        entries.push(StoreEntry {
            name: entry.file_name().to_string_lossy().into_owned(),
            size_bytes,
        });
    }
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::*;

    const LBL: StoreLabels = StoreLabels {
        store: "Image Store",
        entry: "Disk image",
        env_var: "QMP_MCP_IMAGE_DIR",
    };

    #[test]
    fn accepts_a_plain_single_segment_name() {
        assert!(assert_valid_store_name("root.qcow2", &LBL).is_ok());
        assert!(assert_valid_store_name("disk-1_v2.raw", &LBL).is_ok());
    }

    #[test]
    fn rejects_empty_absolute_dotdot_separators_and_injection() {
        assert!(assert_valid_store_name("", &LBL).is_err());
        assert!(assert_valid_store_name("   ", &LBL).is_err());
        assert!(assert_valid_store_name("/etc/passwd", &LBL)
            .unwrap_err()
            .0
            .contains("absolute path"));
        assert!(assert_valid_store_name("..", &LBL)
            .unwrap_err()
            .0
            .contains("not a valid file name"));
        assert!(assert_valid_store_name("a/b", &LBL)
            .unwrap_err()
            .0
            .contains("path separator"));
        // Option-injection characters are outside the allowlist.
        assert!(assert_valid_store_name("root.qcow2,readonly=on", &LBL).is_err());
        assert!(assert_valid_store_name("-leadingdash", &LBL).is_err());
        // A NUL byte is rejected outright (not cleanly representable in the shared
        // JSON corpus, so its branch is pinned here).
        assert!(assert_valid_store_name("a\0b", &LBL)
            .unwrap_err()
            .0
            .contains("NUL byte"));
    }
}
