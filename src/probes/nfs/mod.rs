//! NFS and shared storage probes.
//!
//! # Why this module is unusually careful
//!
//! A hung NFS mount puts the calling process into uninterruptible sleep. The
//! process cannot be killed, the syscall cannot be cancelled, and the thread is
//! gone until the server comes back. A monitoring agent that calls `stat()` on
//! every mount every thirty seconds will, during an outage, accumulate blocked
//! threads until it dies — taking the monitoring down at exactly the moment it
//! is needed (SPEC.md §75, §121, IMPLEMENTATION.md §53).
//!
//! So the probes here are split by how dangerous they are:
//!
//! | Probe | Touches the filesystem | Safe during an outage |
//! | --- | --- | --- |
//! | `nfs.client.mount` | no — reads `/proc/self/mounts` | yes |
//! | `nfs.server.port` | no — TCP connect | yes |
//! | `nfs.server.exports` | no — reads a local file | yes |
//! | `nfs.client.io` | **yes** — one bounded `stat()` | **no**, and it is limited to one at a time |
//!
//! Everything that can answer without touching the mount does so. The one probe
//! that must touch it runs at most once per mount, and if that one is still
//! stuck, the next turn is skipped rather than joined.

mod client;
mod server;

pub use client::{NfsClientIoProbe, NfsMountProbe, NfsStatus, PROBE_CLIENT_IO, PROBE_CLIENT_MOUNT};
pub use server::{NfsExportsProbe, NfsPortProbe, PROBE_SERVER_EXPORTS, PROBE_SERVER_PORT};

/// The port NFS listens on.
pub const NFS_PORT: u16 = 2049;
