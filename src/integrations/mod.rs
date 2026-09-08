//! Technology-specific integrations.
//!
//! Everything Sentinel knows about a *particular* scheduler, filesystem or
//! device vendor lives here. The core (`entity`, `capability`, `dependency`,
//! `observation`, `state`, `diagnosis`, `incident`) never depends on this
//! module; integrations reach the core only as capabilities, observations and
//! dependency edges (SPEC.md §184).
//!
//! `inventory/` and `probes/` hold the core-facing traits and thin adapters
//! onto what lives here, so that a single integration's knowledge — how to
//! parse `scontrol`, say — has one home rather than two.

pub mod slurm;
