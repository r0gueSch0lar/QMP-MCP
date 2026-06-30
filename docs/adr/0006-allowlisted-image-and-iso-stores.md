# Disks and ISOs come from allowlisted stores, by name only

Guest disks resolve to names inside a single configured read-write **Image Store**
(`QMP_MCP_IMAGE_DIR`); installation media resolve to names inside a separate
**read-only ISO Store** (`QMP_MCP_ISO_DIR`). The agent never supplies an absolute or
relative host path; the server resolves each name against the store and rejects path
traversal (`..`, absolute paths, symlink escape). New blank disk images may be created
(`qemu-img create`) only inside the Image Store. The ISO Store is mounted/treated
read-only so install media cannot be modified.

We chose two allowlisted directories over accepting host paths because a structured
Hardware Spec is only as safe as its file references: an arbitrary `-drive file=...`
path reintroduces the host-file-read/write problem that ADR-0002 closed. Splitting
images (read-write) from ISOs (read-only) means the large, rewritable surface and the
fixed boot media have different permissions, limiting blast radius if either is abused.
