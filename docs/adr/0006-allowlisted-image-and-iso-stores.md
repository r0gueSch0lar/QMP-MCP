# Disks and ISOs come from allowlisted stores, by name only

Guest disks resolve to names inside a single configured read-write **Image Store**
(`QMP_MCP_IMAGE_DIR`); installation media resolve to names inside a separate
**read-only ISO Store** (`QMP_MCP_ISO_DIR`). The agent never supplies an absolute or
relative host path; the server resolves each name against the store and rejects path
traversal (`..`, absolute paths, symlink escape). New blank disk images may be created
(`qemu-img create`) only inside the Image Store. The ISO Store is mounted/treated
read-only so install media cannot be modified.

> **Amendment (ADR-0018): the ISO Store is append-only, not strictly read-only.** The ISO
> downloader (`download_iso`) may *add* new media to the ISO Store, but never *modify* existing
> media — a download whose target `filename` already exists is refused, and the image is
> atomically renamed into place only on success. So the property this ADR protects (already-present
> install media is immutable to a running Guest) still holds; only the "the Store is never written
> at all" phrasing is relaxed, and only for the operator-gated downloader (`QMP_MCP_ALLOW_DOWNLOAD`),
> never for a Guest.

We chose two allowlisted directories over accepting host paths because a structured
Hardware Spec is only as safe as its file references: an arbitrary `-drive file=...`
path reintroduces the host-file-read/write problem that ADR-0002 closed. Splitting
images (read-write) from ISOs (read-only) means the large, rewritable surface and the
fixed boot media have different permissions, limiting blast radius if either is abused.
