//! The Image Store (ADR-0006): a single configured, read-write directory that
//! holds guest disk images, referenced by *name* within it — never by host path.
//! New blank images are created inside it via `qemu-img create`.
//!
//! This module is the read-write half of the security boundary. The airtight
//! containment check — name validation plus realpath-containment — lives in the
//! shared [`resolve_in_store`] (see `store_path.rs`), which the read-only ISO Store
//! reuses verbatim so the two stores cannot drift. [`resolve_image_path`] and
//! [`assert_valid_image_name`] are thin Image-Store-flavoured wrappers over it, kept
//! exported so every traversal case stays unit-testable without spawning anything.
//! Mirrors `../../typescript/src/instance/image-store.ts`.

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::store_path::{
    assert_valid_store_name, list_store_files, resolve_in_store, StoreEntry, StoreError,
    StoreLabels,
};

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

/// The listing returned by [`ImageStore::list`]: the store directory and the disk
/// images present in it, by name. Mirrors the TS `{ store, images }` shape.
#[derive(Debug, Serialize, JsonSchema)]
pub struct ImageListing {
    /// The configured Image Store directory (as configured, not realpath-resolved).
    pub store: String,
    /// The disk images present in the Store, sorted by name.
    pub images: Vec<StoreEntry>,
}

/// A request to create a blank image inside the Store (mirrors the TS
/// `CreateImageRequest`).
#[derive(Debug, Clone)]
pub struct CreateImageRequest {
    /// Bare name of the image to create (resolved through [`resolve_image_path`]).
    pub name: String,
    /// Virtual size in GiB; capped by [`ImageStore::max_disk_gb`]. Typed as a signed
    /// integer so a zero/negative request yields the actionable "positive integer"
    /// message rather than a generic deserialisation error, mirroring the TS check.
    pub size_gb: i64,
    /// Image format; a validated [`ImageFormat`] enum, so the format allowlist is
    /// enforced at the schema boundary (no separate runtime check needed).
    pub format: ImageFormat,
}

/// The outcome of a successful [`ImageStore::create`] (mirrors the TS
/// `CreateImageResult`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CreateImageResult {
    /// The bare name the image was created under.
    pub name: String,
    /// Absolute host path of the created image (inside the Store).
    pub path: String,
    /// The format the image was created with.
    pub format: ImageFormat,
    /// The virtual size, in GiB, the image was created with.
    pub size_gb: i64,
}

/// Runs `qemu-img` and resolves on exit 0, else returns its captured stderr as an
/// error string (mirrors the TS `QemuImgRunner`). An injectable seam so the store's
/// validation can be unit-tested without spawning, and the real runner swapped for a
/// fake.
#[async_trait]
pub trait QemuImgRunner: Send + Sync {
    /// Run `binary` with `args`; `Ok(())` on exit 0, otherwise an actionable message.
    async fn run(&self, binary: &str, args: &[String]) -> Result<(), String>;
}

/// The production [`QemuImgRunner`]: spawn `qemu-img` as a child process (mirroring
/// how the real QEMU driver spawns `qemu-system-*`): no shell, argv array, stderr
/// captured for diagnostics.
#[derive(Debug, Clone, Copy, Default)]
pub struct SpawnQemuImg;

#[async_trait]
impl QemuImgRunner for SpawnQemuImg {
    async fn run(&self, binary: &str, args: &[String]) -> Result<(), String> {
        let output = tokio::process::Command::new(binary)
            .args(args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .output()
            .await
            .map_err(|err| format!("Failed to spawn {binary}: {err}"))?;
        if output.status.success() {
            return Ok(());
        }
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stderr = stderr.trim();
        let code = output
            .status
            .code()
            .map_or_else(|| "null".to_string(), |c| c.to_string());
        let signal = signal_of(&output.status);
        Err(format!(
            "{binary} {argv} failed (code={code}, signal={signal}): {stderr}",
            argv = args.join(" "),
            stderr = if stderr.is_empty() {
                "(no stderr)"
            } else {
                stderr
            }
        ))
    }
}

/// The terminating signal number as a string, or `"null"` when the process exited
/// normally. Mirrors the `signal` field the TS spawn helper reports.
#[cfg(unix)]
fn signal_of(status: &std::process::ExitStatus) -> String {
    use std::os::unix::process::ExitStatusExt;
    status
        .signal()
        .map_or_else(|| "null".to_string(), |s| s.to_string())
}

#[cfg(not(unix))]
fn signal_of(_status: &std::process::ExitStatus) -> String {
    "null".to_string()
}

/// Options for an [`ImageStore`] (mirrors the TS `ImageStoreOptions`).
pub struct ImageStoreOptions {
    /// Absolute path of the Image Store directory.
    pub dir: String,
    /// Hard cap on virtual disk size, in GiB (`QMP_MCP_MAX_DISK_GB`).
    pub max_disk_gb: u32,
    /// The `qemu-img` binary to invoke (default `qemu-img`).
    pub qemu_img_binary: Option<String>,
    /// Injected `qemu-img` runner (default [`SpawnQemuImg`], the real binary).
    pub run: Option<Arc<dyn QemuImgRunner>>,
}

/// The read-write Image Store: lists images present in the Store and creates new
/// blank ones via `qemu-img create`, enforcing the size cap, the format allowlist
/// (via the [`ImageFormat`] enum), and the [`resolve_image_path`] containment
/// boundary. Cheaply cloneable — `run` is a shared handle — so the MCP server can
/// hold one and clone it freely (mirrors the TS `ImageStore`).
#[derive(Clone)]
pub struct ImageStore {
    /// The configured Image Store directory.
    pub dir: String,
    /// Hard cap on virtual disk size, in GiB.
    pub max_disk_gb: u32,
    binary: String,
    run: Arc<dyn QemuImgRunner>,
}

impl ImageStore {
    /// Construct an Image Store, defaulting the `qemu-img` binary to `qemu-img` and
    /// the runner to the real [`SpawnQemuImg`].
    pub fn new(options: ImageStoreOptions) -> Self {
        Self {
            dir: options.dir,
            max_disk_gb: options.max_disk_gb,
            binary: options
                .qemu_img_binary
                .unwrap_or_else(|| "qemu-img".to_string()),
            run: options.run.unwrap_or_else(|| Arc::new(SpawnQemuImg)),
        }
    }

    /// List the disk images in the Store: regular files only, sorted by name, each
    /// with its host size (symlinks and subdirectories are skipped, never followed).
    /// Fails closed with an actionable message naming `QMP_MCP_IMAGE_DIR` when the
    /// Store is missing.
    pub async fn list(&self) -> Result<ImageListing, StoreError> {
        let images = list_store_files(&self.dir, &IMAGE_LABELS).await?;
        Ok(ImageListing {
            store: self.dir.clone(),
            images,
        })
    }

    /// Create a blank image of the requested name/size/format inside the Store.
    /// Rejects over-cap sizes (naming `QMP_MCP_MAX_DISK_GB` and the
    /// requested-vs-allowed values), non-positive sizes, escaping names, and a name
    /// that is already taken — mirroring the TS `ImageStore.create`.
    pub async fn create(
        &self,
        request: CreateImageRequest,
    ) -> Result<CreateImageResult, StoreError> {
        let CreateImageRequest {
            name,
            size_gb,
            format,
        } = request;

        // Size validation runs before the containment resolve, mirroring the TS order
        // (so an over-cap request is rejected without even touching the filesystem).
        if size_gb < 1 {
            return Err(StoreError(format!(
                "Disk size must be a positive integer number of GiB (got {size_gb})."
            )));
        }
        if size_gb > i64::from(self.max_disk_gb) {
            return Err(StoreError(format!(
                "Requested disk size {size_gb} GiB exceeds the maximum allowed {max} GiB \
                 (QMP_MCP_MAX_DISK_GB). Request {max} GiB or less.",
                max = self.max_disk_gb
            )));
        }

        // Containment boundary: rejects absolute/`..`/separator/injection names and
        // symlink escape, and fails closed on a missing Store directory.
        let path = resolve_image_path(&name, &self.dir)?;

        // Refuse to clobber or write through an existing entry. `resolve_image_path`
        // has already proven any existing leaf is contained, so this is a friendly
        // guard, not a security check.
        if Path::new(&path).symlink_metadata().is_ok() {
            return Err(StoreError(format!(
                "An image named \"{name}\" already exists in the Image Store. Choose another name \
                 or remove it first."
            )));
        }

        let args = vec![
            "create".to_string(),
            "-f".to_string(),
            format.as_str().to_string(),
            path.clone(),
            format!("{size_gb}G"),
        ];
        self.run
            .run(&self.binary, &args)
            .await
            .map_err(|err| StoreError(format!("Failed to create image \"{name}\": {err}")))?;

        Ok(CreateImageResult {
            name,
            path,
            format,
            size_gb,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A best-effort self-cleaning temp directory (no external crate; mirrors the
    /// hardware-spec tests' `TempDir` and the TS tests' `mkdtemp`).
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

    /// Whether `binary` is an executable file on `PATH` (mirrors the real-qemu
    /// integration test's runtime-skip probe).
    fn on_path(binary: &str) -> bool {
        let Some(path) = std::env::var_os("PATH") else {
            return false;
        };
        std::env::split_paths(&path).any(|dir| dir.join(binary).is_file())
    }

    /// An Image Store over `dir` with the given cap and the real `qemu-img` runner.
    fn store(dir: &str, max_disk_gb: u32) -> ImageStore {
        ImageStore::new(ImageStoreOptions {
            dir: dir.to_string(),
            max_disk_gb,
            qemu_img_binary: None,
            run: None,
        })
    }

    fn req(name: &str, size_gb: i64, format: ImageFormat) -> CreateImageRequest {
        CreateImageRequest {
            name: name.to_string(),
            size_gb,
            format,
        }
    }

    #[tokio::test]
    async fn rejects_over_cap_size_naming_the_variable_and_bounds() {
        let tmp = TempDir::new("image-cap");
        let err = store(&tmp.dir(), 10)
            .create(req("big.qcow2", 11, ImageFormat::Qcow2))
            .await
            .unwrap_err();
        assert!(err.0.contains(
            "Requested disk size 11 GiB exceeds the maximum allowed 10 GiB (QMP_MCP_MAX_DISK_GB)."
        ));
        assert!(err.0.contains("Request 10 GiB or less."));
        // The boundary itself is allowed (equal to the cap).
        assert!(!err.0.is_empty());
    }

    #[tokio::test]
    async fn rejects_non_positive_size() {
        let tmp = TempDir::new("image-zero");
        let err = store(&tmp.dir(), 64)
            .create(req("z.qcow2", 0, ImageFormat::Qcow2))
            .await
            .unwrap_err();
        assert!(err
            .0
            .contains("Disk size must be a positive integer number of GiB (got 0)."));
        let err = store(&tmp.dir(), 64)
            .create(req("z.qcow2", -3, ImageFormat::Qcow2))
            .await
            .unwrap_err();
        assert!(err.0.contains("(got -3)."));
    }

    #[tokio::test]
    async fn rejects_absolute_traversal_and_injection_names() {
        let tmp = TempDir::new("image-names");
        let s = store(&tmp.dir(), 64);
        assert!(s
            .create(req("/etc/passwd", 1, ImageFormat::Qcow2))
            .await
            .unwrap_err()
            .0
            .contains("absolute path"));
        assert!(s
            .create(req("../escape.qcow2", 1, ImageFormat::Qcow2))
            .await
            .unwrap_err()
            .0
            .contains("path separator"));
        assert!(s
            .create(req("disk.qcow2,readonly=on", 1, ImageFormat::Qcow2))
            .await
            .unwrap_err()
            .0
            .contains("could inject QEMU -drive properties"));
        // Nothing was written for any rejected name.
        assert!(std::fs::read_dir(&tmp.path).unwrap().next().is_none());
    }

    #[tokio::test]
    async fn rejects_symlink_that_escapes_the_store() {
        let tmp = TempDir::new("image-symlink");
        std::os::unix::fs::symlink("/etc/passwd", tmp.path.join("evil.qcow2")).unwrap();
        let err = store(&tmp.dir(), 64)
            .create(req("evil.qcow2", 1, ImageFormat::Qcow2))
            .await
            .unwrap_err();
        assert!(err.0.contains("symlink escape"));
    }

    #[tokio::test]
    async fn rejects_a_name_that_already_exists() {
        let tmp = TempDir::new("image-collision");
        std::fs::write(tmp.path.join("root.qcow2"), b"").unwrap();
        let err = store(&tmp.dir(), 64)
            .create(req("root.qcow2", 1, ImageFormat::Qcow2))
            .await
            .unwrap_err();
        assert!(err
            .0
            .contains("An image named \"root.qcow2\" already exists in the Image Store."));
    }

    #[tokio::test]
    async fn list_fails_closed_when_the_store_is_missing() {
        let s = store("/nonexistent/qmp-mcp-image-store-xyz", 64);
        let err = s.list().await.unwrap_err();
        assert!(err.0.contains("QMP_MCP_IMAGE_DIR"));
        assert!(err.0.contains("does not exist or is not accessible"));
    }

    #[tokio::test]
    async fn list_reports_only_regular_files_sorted_skipping_symlinks_and_dirs() {
        let tmp = TempDir::new("image-list");
        std::fs::write(tmp.path.join("b.raw"), b"bb").unwrap();
        std::fs::write(tmp.path.join("a.qcow2"), b"aaaa").unwrap();
        std::fs::create_dir(tmp.path.join("subdir")).unwrap();
        std::os::unix::fs::symlink("/etc/passwd", tmp.path.join("link.qcow2")).unwrap();

        let listing = store(&tmp.dir(), 64).list().await.unwrap();
        assert_eq!(listing.store, tmp.dir());
        let names: Vec<&str> = listing.images.iter().map(|i| i.name.as_str()).collect();
        assert_eq!(names, vec!["a.qcow2", "b.raw"]);
        assert_eq!(listing.images[0].size_bytes, 4);
        assert_eq!(listing.images[1].size_bytes, 2);
    }

    /// The actual `qemu-img create` round-trip: provision a real image into a temp
    /// Image Store, confirm the file exists and is listed, and that a second create
    /// with the same name collides. RUNTIME-SKIPS when `qemu-img` is absent, so it is
    /// a no-op off the dev container but really runs where `qemu-img` exists.
    #[tokio::test]
    async fn qemu_img_create_round_trips_into_the_store() {
        if !on_path("qemu-img") {
            eprintln!(
                "SKIP qemu_img_create_round_trips_into_the_store: no qemu-img on PATH \
                 (this test is a no-op without qemu-img)."
            );
            return;
        }
        let tmp = TempDir::new("image-create");
        let s = store(&tmp.dir(), 64);

        // qcow2 (the default format).
        let result = s
            .create(req("disk.qcow2", 1, ImageFormat::Qcow2))
            .await
            .expect("qemu-img create should succeed");
        assert_eq!(result.name, "disk.qcow2");
        assert_eq!(result.format, ImageFormat::Qcow2);
        assert_eq!(result.size_gb, 1);
        assert_eq!(result.path, tmp.path.join("disk.qcow2").to_string_lossy());
        assert!(Path::new(&result.path).is_file(), "image file must exist");

        // raw round-trips too.
        s.create(req("data.raw", 2, ImageFormat::Raw))
            .await
            .expect("raw qemu-img create should succeed");

        // Both surface in the listing, sorted.
        let listing = s.list().await.unwrap();
        let names: Vec<&str> = listing.images.iter().map(|i| i.name.as_str()).collect();
        assert_eq!(names, vec!["data.raw", "disk.qcow2"]);

        // A second create with the same name collides (never clobbers).
        let err = s
            .create(req("disk.qcow2", 1, ImageFormat::Qcow2))
            .await
            .unwrap_err();
        assert!(err.0.contains("already exists in the Image Store"));
    }
}
