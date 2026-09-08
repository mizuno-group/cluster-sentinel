//! The built-in diagnosis rules.
//!
//! Rules are added as the evidence to support them arrives. A rule that cannot
//! be justified from stored observations does not belong here.

pub mod reachability;
pub mod services;
pub mod slurm;
pub mod storage;

use super::DiagnosisRule;

/// Every rule that ships with Sentinel, in evaluation order.
pub fn builtin() -> Vec<Box<dyn DiagnosisRule>> {
    vec![
        Box::new(reachability::HostUnreachable),
        Box::new(reachability::PathSpecificNetworkFailure),
        Box::new(services::SentinelAgentFailure),
        Box::new(services::SshServiceFailure),
        Box::new(slurm::SlurmOnlyDegradation),
        Box::new(slurm::SlurmdServiceFailure),
        Box::new(slurm::SlurmControlPlaneFailure),
        Box::new(slurm::ResourceConfigurationMismatch),
        Box::new(storage::StorageServiceFailure),
        Box::new(storage::SharedStorageFailure),
        Box::new(storage::ClientLocalStorageFailure),
    ]
}
