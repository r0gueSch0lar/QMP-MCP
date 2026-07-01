//! The Image Store (ADR-0006): a single configured, read-write directory that
//! holds guest disk images, referenced by *name* within it — never by host path.
//!
//! This slice contributes only the read-only pieces the argv generator needs: the
//! disk-image format allowlist and the Image-Store-flavoured wrapper over the
//! shared containment boundary ([`resolve_in_store`]). The read-write `ImageStore`
//! (list/create via `qemu-img`) lands in a later slice, mirroring
//! `../../src/instance/image-store.ts`.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::store_path::{assert_valid_store_name, resolve_in_store, StoreError, StoreLabels};

/// Disk image formats this server will create and pin explicitly into argv
/// (mirrors the TS `IMAGE_FORMATS`). Pinning the format defeats QEMU's format
/// auto-probing, a known security footgun.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum ImageFormat {
    /// The default copy-on-write format.
    #[default]
    Qcow2,
    /// A raw block image.
    Raw,
}

impl ImageFormat {
    /// Canonical spelling emitted into the `-drive format=` property.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Qcow2 => "qcow2",
            Self::Raw => "raw",
        }
    }
}

/// Image-Store wording for the shared containment boundary, so every
/// traversal/injection failure surfaces with Image-Store phrasing.
pub const IMAGE_LABELS: StoreLabels = StoreLabels {
    store: "Image Store",
    entry: "Disk image",
    env_var: "QMP_MCP_IMAGE_DIR",
};

/// Validate that a disk name is a single, safe path segment (Image-Store-flavoured
/// view of the shared allowlist). Mirrors the TS `assertValidImageName`.
pub fn assert_valid_image_name(name: &str) -> Result<(), StoreError> {
    assert_valid_store_name(name, &IMAGE_LABELS)
}

/// Resolve a disk name against the Image Store directory and return its safe
/// absolute path, or a [`StoreError`]. Delegates to the shared containment
/// boundary (the same logic the ISO Store uses). Mirrors `resolveImagePath`.
pub fn resolve_image_path(name: &str, store_dir: &str) -> Result<String, StoreError> {
    resolve_in_store(name, store_dir, &IMAGE_LABELS)
}
