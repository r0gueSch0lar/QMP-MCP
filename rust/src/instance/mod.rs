//! The Instance domain: the validated Hardware Spec, the allowlisted Image/ISO
//! Store boundary, and the pure `qemu-system-*` argv generator (ADR-0002, 0006,
//! 0008, 0009). A second implementation of the shared bounded context, mirroring
//! `../../src/instance/*` so the two servers can be cross-validated against the
//! same domain ADRs and the shared golden fixtures (ADR-0012).

pub mod event_buffer;
pub mod hardware_spec;
pub mod image_store;
pub mod iso_store;
pub mod orchestrator;
pub mod store_path;
