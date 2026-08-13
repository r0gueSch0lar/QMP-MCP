//! The ISO downloader (ADR-0018): fetch an OS image named in the catalog into the ISO
//! Store, on demand, over the network. It is an operator opt-in (`QMP_MCP_ALLOW_DOWNLOAD`)
//! because it is outbound egress that writes files; when disabled, `download_iso` fails
//! closed.
//!
//! Shape:
//!   - ONE download runs at a time (a single slot — the same "one managed thing" discipline
//!     as the single-instance Orchestrator). `download_iso` starts it and returns immediately;
//!     `get_download` reports live progress. Multi-GB images never block a tool call.
//!   - The image streams to a `<name>.part` temp file in the ISO Store, then is atomically
//!     renamed onto the final `<name>` on success — a partial download never appears as a
//!     usable ISO. Mirrors are tried in order; the first that succeeds wins.
//!   - Downloads are APPEND-ONLY: an existing Store file is never overwritten (this is the
//!     one relaxation of ADR-0006's read-only ISO Store — see ADR-0018), and the filename is
//!     the catalog's validated store-name leaf, so a download can never escape the Store.
//!
//! The network I/O is behind the [`Fetcher`] seam so the manager's state machine is unit-tested
//! against a fake, exactly as the Orchestrator is tested against a fake QEMU driver (ADR-0011).
//! Mirrors `../../typescript/src/instance/download.ts`.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Serialize;
use tokio::sync::Mutex;

use super::catalog::{Catalog, CatalogEntry};
use super::store_path::{resolve_in_store, StoreLabels};

/// ISO-Store wording for the shared containment boundary (the download target is resolved
/// against the ISO Store exactly like a referenced ISO).
const ISO_LABELS: StoreLabels = StoreLabels {
    store: "ISO Store",
    entry: "ISO",
    env_var: "QMP_MCP_ISO_DIR",
};

/// Abort a mirror if no bytes arrive within this window and try the next one, so a stalled
/// connection can never wedge the single download slot indefinitely.
const STALL_TIMEOUT: Duration = Duration::from_secs(60);

/// Raised for a `download_iso` request that cannot be started (disabled, unknown id, target
/// already present, or a download already in progress). The message is always actionable.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct DownloadError(pub String);

/// Raised inside the download loop when a single mirror fetch fails; the loop records it and
/// tries the next mirror. Never surfaced directly to the caller.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct FetchError(pub String);

/// The network transport seam: stream `url` into the file at `dest`, reporting the total size
/// (once known, from `Content-Length`) via `on_total` and the cumulative bytes written via
/// `on_progress`. Returns the number of bytes written, or a [`FetchError`] (the mirror failed).
/// The real implementation is [`ReqwestFetcher`]; tests inject a fake.
#[async_trait]
pub trait Fetcher: Send + Sync {
    async fn fetch_to_file(
        &self,
        url: &str,
        dest: &Path,
        on_total: &(dyn Fn(Option<u64>) + Send + Sync),
        on_progress: &(dyn Fn(u64) + Send + Sync),
    ) -> Result<u64, FetchError>;
}

/// The real [`Fetcher`]: a `reqwest` client that follows redirects (distros redirect heavily),
/// streams the body to disk with backpressure, and enforces a per-chunk stall timeout.
pub struct ReqwestFetcher {
    client: reqwest::Client,
}

impl ReqwestFetcher {
    /// Build the shared client. A generous connect timeout catches an unreachable mirror
    /// quickly, but there is deliberately NO overall request timeout — an ISO is gigabytes and
    /// legitimately takes minutes; stalls are caught per-chunk in [`Fetcher::fetch_to_file`].
    pub fn new() -> Self {
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(30))
            .user_agent(concat!("qmp-mcp/", env!("CARGO_PKG_VERSION")))
            .build()
            .expect("failed to build reqwest client");
        Self { client }
    }
}

impl Default for ReqwestFetcher {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Fetcher for ReqwestFetcher {
    async fn fetch_to_file(
        &self,
        url: &str,
        dest: &Path,
        on_total: &(dyn Fn(Option<u64>) + Send + Sync),
        on_progress: &(dyn Fn(u64) + Send + Sync),
    ) -> Result<u64, FetchError> {
        use futures_util::StreamExt;
        use tokio::io::AsyncWriteExt;

        let resp = self
            .client
            .get(url)
            .send()
            .await
            .map_err(|e| FetchError(format!("request to {url} failed: {e}")))?
            .error_for_status()
            .map_err(|e| FetchError(format!("{url} returned an error status: {e}")))?;
        // Guard against a portal/login/error page being saved as an ISO: a real image is never
        // served as text/html (e.g. an auth-gated distro returns its download page). Reject it so
        // the mirror fails cleanly rather than writing an HTML file under an .iso name.
        if let Some(ct) = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
        {
            if ct
                .trim_start()
                .to_ascii_lowercase()
                .starts_with("text/html")
            {
                return Err(FetchError(format!(
                    "{url} returned an HTML page (content-type {ct}), not a file — likely a \
                     login/error page, not the image"
                )));
            }
        }
        on_total(resp.content_length());

        let mut file = tokio::fs::File::create(dest)
            .await
            .map_err(|e| FetchError(format!("cannot create {}: {e}", dest.display())))?;
        let mut stream = resp.bytes_stream();
        let mut downloaded: u64 = 0;
        loop {
            let next = tokio::time::timeout(STALL_TIMEOUT, stream.next())
                .await
                .map_err(|_| {
                    FetchError(format!(
                        "{url} stalled (no data for {}s)",
                        STALL_TIMEOUT.as_secs()
                    ))
                })?;
            match next {
                Some(Ok(chunk)) => {
                    file.write_all(&chunk).await.map_err(|e| {
                        FetchError(format!("write to {} failed: {e}", dest.display()))
                    })?;
                    downloaded += chunk.len() as u64;
                    on_progress(downloaded);
                }
                Some(Err(e)) => return Err(FetchError(format!("download from {url} failed: {e}"))),
                None => break,
            }
        }
        file.flush()
            .await
            .map_err(|e| FetchError(format!("flush {} failed: {e}", dest.display())))?;
        Ok(downloaded)
    }
}

/// The lifecycle state of a download (reported by `get_download`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum DownloadState {
    /// Actively fetching (bytes are streaming to the temp file).
    Downloading,
    /// Finished: the image was renamed onto its final ISO-Store path.
    Completed,
    /// Every mirror failed (or the target could not be finalized); `error` says why.
    Failed,
}

/// A snapshot of a single download's progress/outcome. Returned by `download_iso` (initial
/// snapshot) and `get_download` (current or last). Mirrors the TS `DownloadStatus`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DownloadStatus {
    /// The catalog id being downloaded.
    pub id: String,
    /// The distro's human-facing name.
    pub name: String,
    /// The bare name the image is saved as in the ISO Store.
    pub filename: String,
    /// Lifecycle state.
    pub state: DownloadState,
    /// Bytes written so far (of the current mirror attempt).
    pub bytes_downloaded: u64,
    /// Total size in bytes when the server advertised a `Content-Length`, else absent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_bytes: Option<u64>,
    /// Integer percent complete (0..100) when the total size is known, else absent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub percent: Option<u32>,
    /// The mirror URL currently being tried (or the one that succeeded/failed).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mirror: Option<String>,
    /// Zero-based index of `mirror` within the entry's mirror list.
    pub mirror_index: usize,
    /// Total number of mirrors for the entry.
    pub mirrors_total: usize,
    /// The final ISO-Store path, set once the download completes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// Why the download failed, set when `state` is `failed`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// The `get_download` report: the downloader capability + the current-or-last download.
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DownloadReport {
    /// Whether downloading is enabled (`QMP_MCP_ALLOW_DOWNLOAD=true`).
    pub available: bool,
    /// Why downloading is unavailable, or a confirming note when it is.
    pub reason: String,
    /// Where the catalog came from (`built-in` or a file path).
    pub catalog_source: String,
    /// Number of entries in the catalog.
    pub catalog_count: usize,
    /// Whether a download is currently in progress.
    pub active: bool,
    /// The current (if active) or most recent download's status, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub download: Option<DownloadStatus>,
    /// The configured ISO Store directory downloads land in.
    pub iso_dir: String,
}

/// Options for constructing a [`DownloadManager`].
pub struct DownloadManagerOptions {
    /// The ISO Store directory downloads are written into.
    pub iso_dir: String,
    /// Whether downloading is enabled (`QMP_MCP_ALLOW_DOWNLOAD`).
    pub allow_download: bool,
    /// The validated catalog.
    pub catalog: Catalog,
    /// The network transport (real reqwest client, or a fake in tests).
    pub fetcher: Arc<dyn Fetcher>,
}

/// The mutable slot: at most one active download, plus the last finished snapshot.
#[derive(Default)]
struct ManagerState {
    /// The in-flight download's shared, live-updated status (a `std::sync::Mutex` so the
    /// fetcher's synchronous progress callbacks can update it without an async lock).
    active: Option<Arc<StdMutex<DownloadStatus>>>,
    /// The most recently finished (completed/failed) snapshot, for `get_download` when idle.
    last: Option<DownloadStatus>,
}

/// The immutable core shared with the spawned download task.
struct Inner {
    iso_dir: String,
    allow_download: bool,
    catalog: Catalog,
    fetcher: Arc<dyn Fetcher>,
    state: Mutex<ManagerState>,
}

/// The ISO downloader. Cloneable (a shared `Arc` handle), so the server holds one and every
/// tool call drives the same single slot.
#[derive(Clone)]
pub struct DownloadManager {
    inner: Arc<Inner>,
}

impl DownloadManager {
    /// Construct the downloader from its options.
    pub fn new(options: DownloadManagerOptions) -> Self {
        Self {
            inner: Arc::new(Inner {
                iso_dir: options.iso_dir,
                allow_download: options.allow_download,
                catalog: options.catalog,
                fetcher: options.fetcher,
                state: Mutex::new(ManagerState::default()),
            }),
        }
    }

    /// The catalog, for the `list_iso_catalog` tool.
    pub fn catalog(&self) -> &Catalog {
        &self.inner.catalog
    }

    /// The `get_download` report: capability + the current-or-last download.
    pub async fn describe(&self) -> DownloadReport {
        let state = self.inner.state.lock().await;
        let (active, download) = match &state.active {
            Some(shared) => (true, Some(shared.lock().unwrap().clone())),
            None => (false, state.last.clone()),
        };
        let (available, reason) = if self.inner.allow_download {
            (
                true,
                format!(
                    "Downloading is available ({} entries in the catalog from {}).",
                    self.inner.catalog.entries.len(),
                    self.inner.catalog.source
                ),
            )
        } else {
            (
                false,
                "Downloading is disabled. Set QMP_MCP_ALLOW_DOWNLOAD=true to enable download_iso \
                 (it fetches an OS image over the network into the ISO Store)."
                    .to_string(),
            )
        };
        DownloadReport {
            available,
            reason,
            catalog_source: self.inner.catalog.source.clone(),
            catalog_count: self.inner.catalog.entries.len(),
            active,
            download,
            iso_dir: self.inner.iso_dir.clone(),
        }
    }

    /// Start downloading the catalog entry with the given `id` into the ISO Store, returning the
    /// initial (downloading) status immediately. The actual fetch runs in a background task; poll
    /// `get_download` for progress. Fails closed when downloading is disabled, the id is unknown,
    /// the target file already exists (append-only), or a download is already in progress.
    pub async fn start(&self, id: &str) -> Result<DownloadStatus, DownloadError> {
        if !self.inner.allow_download {
            return Err(DownloadError(
                "Downloading is disabled. Set QMP_MCP_ALLOW_DOWNLOAD=true to enable download_iso \
                 (it fetches an OS image over the network into the ISO Store)."
                    .to_string(),
            ));
        }
        let entry = self
            .inner
            .catalog
            .find(id)
            .ok_or_else(|| {
                DownloadError(format!(
                    "unknown ISO id \"{id}\". Use list_iso_catalog to see the available ids ({}).",
                    self.inner.catalog.ids_hint()
                ))
            })?
            .clone();

        // Resolve (and validate) the target inside the ISO Store. This also fails closed with an
        // actionable message when the Store directory does not exist, and rejects symlink escape.
        let dest = resolve_in_store(&entry.filename, &self.inner.iso_dir, &ISO_LABELS)
            .map_err(|e| DownloadError(e.0))?;
        let dest_path = PathBuf::from(&dest);
        // Append-only (ADR-0018 relaxes ADR-0006 to "add new media", never overwrite existing).
        if dest_path.exists() {
            return Err(DownloadError(format!(
                "\"{}\" is already present in the ISO Store; downloading would overwrite it. \
                 Delete it first, or it is already available (see list_isos).",
                entry.filename
            )));
        }

        // Claim the single download slot atomically under the manager lock.
        let mut state = self.inner.state.lock().await;
        if let Some(active) = &state.active {
            let cur = active.lock().unwrap().clone();
            return Err(DownloadError(format!(
                "a download is already in progress (\"{}\", id \"{}\"). Only one download runs at \
                 a time — wait for it to finish (poll get_download) or let it fail.",
                cur.filename, cur.id
            )));
        }
        let initial = DownloadStatus {
            id: entry.id.clone(),
            name: entry.name.clone(),
            filename: entry.filename.clone(),
            state: DownloadState::Downloading,
            bytes_downloaded: 0,
            total_bytes: None,
            percent: None,
            mirror: entry.mirrors.first().cloned(),
            mirror_index: 0,
            mirrors_total: entry.mirrors.len(),
            path: None,
            error: None,
        };
        let shared = Arc::new(StdMutex::new(initial.clone()));
        state.active = Some(Arc::clone(&shared));
        drop(state);

        // Run the fetch off the request path.
        let inner = Arc::clone(&self.inner);
        tokio::spawn(async move {
            run_download(inner, entry, dest_path, shared).await;
        });

        Ok(initial)
    }
}

/// The `<name>.part` temp path alongside the final destination (same directory → the rename is
/// atomic and never crosses a filesystem).
fn temp_path_for(dest: &Path) -> PathBuf {
    let mut os = dest.as_os_str().to_os_string();
    os.push(".part");
    PathBuf::from(os)
}

/// The background download loop: try each mirror in turn, streaming into the temp file; on the
/// first success atomically rename it onto the final path and mark `completed`; if every mirror
/// fails, remove the temp file and mark `failed`. Always releases the download slot at the end.
async fn run_download(
    inner: Arc<Inner>,
    entry: CatalogEntry,
    dest: PathBuf,
    shared: Arc<StdMutex<DownloadStatus>>,
) {
    let temp = temp_path_for(&dest);
    // Clear any stale partial from a previously-interrupted attempt.
    let _ = tokio::fs::remove_file(&temp).await;

    let mut last_err = "no mirrors were configured for this entry".to_string();
    for (index, url) in entry.mirrors.iter().enumerate() {
        {
            let mut s = shared.lock().unwrap();
            s.mirror = Some(url.clone());
            s.mirror_index = index;
            s.bytes_downloaded = 0;
            s.total_bytes = None;
            s.percent = None;
        }
        let on_total = {
            let shared = Arc::clone(&shared);
            move |total: Option<u64>| {
                let mut s = shared.lock().unwrap();
                s.total_bytes = total;
            }
        };
        let on_progress = {
            let shared = Arc::clone(&shared);
            move |downloaded: u64| {
                let mut s = shared.lock().unwrap();
                s.bytes_downloaded = downloaded;
                if let Some(total) = s.total_bytes {
                    if total > 0 {
                        let pct =
                            (u128::from(downloaded.min(total)) * 100 / u128::from(total)) as u32;
                        s.percent = Some(pct);
                    }
                }
            }
        };

        match inner
            .fetcher
            .fetch_to_file(url, &temp, &on_total, &on_progress)
            .await
        {
            Ok(_bytes) => match tokio::fs::rename(&temp, &dest).await {
                Ok(()) => {
                    {
                        let mut s = shared.lock().unwrap();
                        s.state = DownloadState::Completed;
                        s.path = Some(dest.to_string_lossy().into_owned());
                        if s.total_bytes.is_some() {
                            s.percent = Some(100);
                        }
                    }
                    tracing::info!("download complete: {} -> {}", entry.id, dest.display());
                    finish(&inner, &shared).await;
                    return;
                }
                Err(e) => {
                    // A rename failure is not a mirror problem; stop and report it.
                    let _ = tokio::fs::remove_file(&temp).await;
                    last_err = format!("failed to finalize {}: {e}", dest.display());
                    break;
                }
            },
            Err(e) => {
                let _ = tokio::fs::remove_file(&temp).await;
                tracing::warn!("download mirror failed ({}): {}", url, e.0);
                last_err = e.0;
            }
        }
    }

    {
        let mut s = shared.lock().unwrap();
        s.state = DownloadState::Failed;
        s.error = Some(last_err);
    }
    finish(&inner, &shared).await;
}

/// Release the download slot: snapshot the terminal status into `last` and clear `active`.
async fn finish(inner: &Arc<Inner>, shared: &Arc<StdMutex<DownloadStatus>>) {
    let snapshot = shared.lock().unwrap().clone();
    let mut state = inner.state.lock().await;
    state.active = None;
    state.last = Some(snapshot);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::instance::catalog::parse_catalog;
    use std::collections::HashMap;

    /// A self-cleaning temp directory for the ISO Store under test.
    struct TempDir {
        path: PathBuf,
    }
    impl TempDir {
        fn new() -> Self {
            // A process-wide sequence number keeps parallel tests apart even when two land on
            // the same clock tick: with nanos alone, colliding tests shared a directory and one
            // test's Drop could delete it mid-download (seen as a Failed download on CI). The
            // nanos stay for cross-run hygiene should a recycled pid meet a leftover directory.
            static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path =
                std::env::temp_dir().join(format!("qmp-dl-{}-{nanos}-{seq}", std::process::id()));
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

    /// A fake [`Fetcher`] driven by an exact url→outcome map. An optional `hold` gate keeps the
    /// call pending until the test releases it, so the "already in progress" path is deterministic.
    struct FakeFetcher {
        outcomes: HashMap<String, Result<Vec<u8>, String>>,
        hold: Option<Arc<tokio::sync::Notify>>,
    }
    #[async_trait]
    impl Fetcher for FakeFetcher {
        async fn fetch_to_file(
            &self,
            url: &str,
            dest: &Path,
            on_total: &(dyn Fn(Option<u64>) + Send + Sync),
            on_progress: &(dyn Fn(u64) + Send + Sync),
        ) -> Result<u64, FetchError> {
            if let Some(h) = &self.hold {
                h.notified().await;
            }
            match self.outcomes.get(url) {
                Some(Ok(bytes)) => {
                    on_total(Some(bytes.len() as u64));
                    tokio::fs::write(dest, bytes)
                        .await
                        .map_err(|e| FetchError(e.to_string()))?;
                    on_progress(bytes.len() as u64);
                    Ok(bytes.len() as u64)
                }
                Some(Err(msg)) => Err(FetchError(msg.clone())),
                None => Err(FetchError(format!("no fake outcome for {url}"))),
            }
        }
    }

    fn catalog_json() -> &'static str {
        r#"{"isos":[
            {"id":"one","name":"One","filename":"one.iso","mirrors":["https://a/one.iso","https://b/one.iso"]}
        ]}"#
    }

    fn manager(iso_dir: String, allow: bool, fetcher: FakeFetcher) -> DownloadManager {
        DownloadManager::new(DownloadManagerOptions {
            iso_dir,
            allow_download: allow,
            catalog: parse_catalog(catalog_json(), "test").unwrap(),
            fetcher: Arc::new(fetcher),
        })
    }

    fn fetcher(
        outcomes: &[(&str, Result<&[u8], &str>)],
        hold: Option<Arc<tokio::sync::Notify>>,
    ) -> FakeFetcher {
        let map = outcomes
            .iter()
            .map(|(u, r)| {
                let v = match r {
                    Ok(b) => Ok(b.to_vec()),
                    Err(m) => Err((*m).to_string()),
                };
                ((*u).to_string(), v)
            })
            .collect();
        FakeFetcher {
            outcomes: map,
            hold,
        }
    }

    /// Poll `get_download` until the download reaches a terminal state (bounded), returning it.
    async fn await_terminal(mgr: &DownloadManager) -> DownloadStatus {
        for _ in 0..200 {
            let report = mgr.describe().await;
            if let Some(d) = report.download {
                if d.state != DownloadState::Downloading {
                    return d;
                }
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("download did not reach a terminal state");
    }

    #[tokio::test]
    async fn disabled_gate_fails_closed_naming_the_variable() {
        let tmp = TempDir::new();
        let mgr = manager(tmp.dir(), false, fetcher(&[], None));
        let err = mgr.start("one").await.unwrap_err();
        assert!(err.0.contains("QMP_MCP_ALLOW_DOWNLOAD"));
        assert!(!mgr.describe().await.available);
    }

    #[tokio::test]
    async fn unknown_id_is_rejected_with_the_available_ids() {
        let tmp = TempDir::new();
        let mgr = manager(tmp.dir(), true, fetcher(&[], None));
        let err = mgr.start("nope").await.unwrap_err();
        assert!(err.0.contains("unknown ISO id"));
        assert!(err.0.contains("one"));
    }

    #[tokio::test]
    async fn existing_file_is_not_overwritten() {
        let tmp = TempDir::new();
        std::fs::write(self_join(&tmp, "one.iso"), b"existing").unwrap();
        let mgr = manager(
            tmp.dir(),
            true,
            fetcher(&[("https://a/one.iso", Ok(b"new"))], None),
        );
        let err = mgr.start("one").await.unwrap_err();
        assert!(err.0.contains("already present"));
    }

    #[tokio::test]
    async fn downloads_the_first_working_mirror_and_renames_atomically() {
        let tmp = TempDir::new();
        let mgr = manager(
            tmp.dir(),
            true,
            fetcher(&[("https://a/one.iso", Ok(b"ISOBYTES"))], None),
        );
        let initial = mgr.start("one").await.unwrap();
        assert_eq!(initial.state, DownloadState::Downloading);
        let done = await_terminal(&mgr).await;
        assert_eq!(done.state, DownloadState::Completed);
        assert_eq!(done.bytes_downloaded, 8);
        assert_eq!(done.percent, Some(100));
        // The final file exists with the fetched bytes; no .part remains.
        let final_path = self_join(&tmp, "one.iso");
        assert_eq!(std::fs::read(&final_path).unwrap(), b"ISOBYTES");
        assert!(!self_join(&tmp, "one.iso.part").exists());
    }

    #[tokio::test]
    async fn falls_over_to_the_second_mirror_when_the_first_fails() {
        let tmp = TempDir::new();
        let mgr = manager(
            tmp.dir(),
            true,
            fetcher(
                &[
                    ("https://a/one.iso", Err("503 from a")),
                    ("https://b/one.iso", Ok(b"FROM_B")),
                ],
                None,
            ),
        );
        mgr.start("one").await.unwrap();
        let done = await_terminal(&mgr).await;
        assert_eq!(done.state, DownloadState::Completed);
        assert_eq!(done.mirror_index, 1);
        assert_eq!(
            std::fs::read(self_join(&tmp, "one.iso")).unwrap(),
            b"FROM_B"
        );
    }

    #[tokio::test]
    async fn all_mirrors_failing_marks_failed_and_leaves_no_partial() {
        let tmp = TempDir::new();
        let mgr = manager(
            tmp.dir(),
            true,
            fetcher(
                &[
                    ("https://a/one.iso", Err("boom a")),
                    ("https://b/one.iso", Err("boom b")),
                ],
                None,
            ),
        );
        mgr.start("one").await.unwrap();
        let done = await_terminal(&mgr).await;
        assert_eq!(done.state, DownloadState::Failed);
        assert!(done.error.unwrap().contains("boom b"));
        assert!(!self_join(&tmp, "one.iso").exists());
        assert!(!self_join(&tmp, "one.iso.part").exists());
    }

    #[tokio::test]
    async fn rejects_a_second_download_while_one_is_in_progress() {
        let tmp = TempDir::new();
        let gate = Arc::new(tokio::sync::Notify::new());
        let mgr = manager(
            tmp.dir(),
            true,
            fetcher(&[("https://a/one.iso", Ok(b"X"))], Some(Arc::clone(&gate))),
        );
        // First download claims the slot and blocks on the gate.
        mgr.start("one").await.unwrap();
        assert!(mgr.describe().await.active);
        // A second start is refused while the slot is held.
        let err = mgr.start("one").await.unwrap_err();
        assert!(err.0.contains("already in progress"));
        // Release the gate; the first completes and frees the slot.
        gate.notify_one();
        let done = await_terminal(&mgr).await;
        assert_eq!(done.state, DownloadState::Completed);
        assert!(!mgr.describe().await.active);
    }

    /// Join a leaf onto the temp dir (test helper).
    fn self_join(tmp: &TempDir, leaf: &str) -> PathBuf {
        tmp.path.join(leaf)
    }
}
