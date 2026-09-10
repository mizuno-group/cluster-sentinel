//! Cluster Sentinel core library.
//!
//! The core of Sentinel knows nothing about Slurm, NFS, NVIDIA or any other
//! concrete technology. It only knows about:
//!
//! ```text
//! Environment / Cluster / ManagedEntity / Capability / Dependency
//! Probe -> Observation -> State -> Diagnosis -> Incident
//! ```
//!
//! Everything technology-specific lives in an *integration* (see
//! [`probes`] and [`inventory`]) and reaches the core only as capabilities,
//! observations and dependency edges.
//!
//! The module dependency direction is strictly:
//!
//! ```text
//! integrations/probes -> observation -> state -> diagnosis -> incident
//! ```
//!
//! The core must never depend on an integration.

pub mod agent;
pub mod audit;
pub mod capability;
pub mod cli;
pub mod command;
pub mod config;
pub mod controller;
pub mod dependency;
pub mod diagnosis;
pub mod entity;
pub mod incident;
pub mod integrations;
pub mod inventory;
pub mod notification;
pub mod observation;
pub mod persistence;
pub mod probes;
pub mod protocol;
pub mod state;
pub mod telemetry;
pub mod time;

/// Version of the `sentinel` binary.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Wire protocol version, deliberately independent from [`VERSION`].
///
/// The binary version and the controller/agent protocol version evolve
/// separately (IMPLEMENTATION.md §62).
pub const PROTOCOL_VERSION: u32 = 1;

/// Configuration schema version understood by this build.
pub const CONFIG_VERSION: u32 = 1;
