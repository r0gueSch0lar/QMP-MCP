//! The ISO Store (ADR-0006): a separate, *read-only* directory that holds
//! installation/boot ISO media, kept distinct from the read-write Image Store so
//! install media can never be modified. ISOs are referenced by *name* within it.
//!
//! This module is deliberately the read-only twin of `image_store.rs`: it lists
//! ISOs and resolves a name to a safe in-Store path, but has NO create operation and
//! never writes (no `mkdir`, no `qemu-img`). The containment boundary — name
//! validation plus realpath-containment — is NOT re-implemented here; it is the very
//! same shared [`resolve_in_store`] the Image Store uses, specialised only by
//! [`ISO_LABELS`]. Sharing one resolver is the whole point: the security-critical
//! code exists once and both stores move together. Mirrors
//! `../../src/instance/iso-store.ts`.

use schemars::JsonSchema;
use serde::Serialize;

use super::store_path::{
    assert_valid_store_name, list_store_files, resolve_in_store, StoreEntry, StoreError,
    StoreLabels,
};

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

/// The listing returned by [`IsoStore::list`]: the store directory and the ISO media
/// present in it, by name. Mirrors the TS `{ store, isos }` shape.
#[derive(Debug, Serialize, JsonSchema)]
pub struct IsoListing {
    /// The configured ISO Store directory (as configured, not realpath-resolved).
    pub store: String,
    /// The ISO media present in the Store, sorted by name.
    pub isos: Vec<StoreEntry>,
}

/// The read-only ISO Store: lists the ISO media present in the Store. There is
/// deliberately NO create/write path — install media is fixed and the Store is
/// treated read-only (ADR-0006), so this type never calls `mkdir` or spawns
/// anything. Trivially cloneable (mirrors the TS `IsoStore`).
#[derive(Debug, Clone)]
pub struct IsoStore {
    /// The configured ISO Store directory.
    pub dir: String,
}

impl IsoStore {
    /// Construct a read-only ISO Store over the given directory.
    pub fn new(dir: String) -> Self {
        Self { dir }
    }

    /// List the ISOs in the Store: regular files only, sorted by name, each with its
    /// host size (symlinks and subdirectories are skipped, never followed). Fails
    /// closed with an actionable message naming `QMP_MCP_ISO_DIR` when the Store is
    /// missing.
    pub async fn list(&self) -> Result<IsoListing, StoreError> {
        let isos = list_store_files(&self.dir, &ISO_LABELS).await?;
        Ok(IsoListing {
            store: self.dir.clone(),
            isos,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A best-effort self-cleaning temp directory (mirrors the Image Store tests).
    struct TempDir {
        path: std::path::PathBuf,
    }
    impl TempDir {
        fn new(prefix: &str) -> Self {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path =
                std::env::temp_dir().join(format!("{prefix}-{}-{nanos}", std::process::id()));
            std::fs::create_dir_all(&path).unwrap();
            Self { path }
        }
        fn dir(&self) -> String {
            self.path.to_string_lossy().into_owned()
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    #[test]
    fn resolve_and_name_validation_reject_traversal_absolute_and_injection() {
        // The read-only store still enforces the shared containment/name rules.
        assert!(assert_valid_iso_name("debian.iso").is_ok());
        assert!(assert_valid_iso_name("/etc/passwd")
            .unwrap_err()
            .0
            .contains("absolute path"));
        assert!(assert_valid_iso_name("../escape.iso")
            .unwrap_err()
            .0
            .contains("path separator"));
        assert!(assert_valid_iso_name("debian.iso,readonly=off")
            .unwrap_err()
            .0
            .contains("could inject QEMU -drive properties"));
        // The ISO-flavoured wording surfaces on the resolver too.
        assert!(resolve_iso_path("..", "/tmp")
            .unwrap_err()
            .0
            .contains("ISO name \"..\" is not a valid file name"));
    }

    #[tokio::test]
    async fn list_fails_closed_when_the_store_is_missing() {
        let err = IsoStore::new("/nonexistent/qmp-mcp-iso-store-xyz".to_string())
            .list()
            .await
            .unwrap_err();
        assert!(err.0.contains("QMP_MCP_ISO_DIR"));
        assert!(err.0.contains("does not exist or is not accessible"));
    }

    #[tokio::test]
    async fn list_reports_only_regular_files_sorted_skipping_symlinks_and_dirs() {
        let tmp = TempDir::new("iso-list");
        std::fs::write(tmp.path.join("ubuntu.iso"), b"ub").unwrap();
        std::fs::write(tmp.path.join("debian.iso"), b"deb1").unwrap();
        std::fs::create_dir(tmp.path.join("subdir")).unwrap();
        std::os::unix::fs::symlink("/etc/hostname", tmp.path.join("link.iso")).unwrap();

        let listing = IsoStore::new(tmp.dir()).list().await.unwrap();
        assert_eq!(listing.store, tmp.dir());
        let names: Vec<&str> = listing.isos.iter().map(|i| i.name.as_str()).collect();
        // Sorted, symlink + subdir excluded (never followed).
        assert_eq!(names, vec!["debian.iso", "ubuntu.iso"]);
        assert_eq!(listing.isos[0].size_bytes, 4);
        assert_eq!(listing.isos[1].size_bytes, 2);
    }
}
