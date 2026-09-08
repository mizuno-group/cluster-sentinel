//! Slurm integration.
//!
//! Slurm is **one integration among several**, not the inventory itself
//! (SPEC.md §5, §31, §180). Hosts Slurm has never heard of are first-class
//! monitored entities, and a Slurm outage must not blind Sentinel to the rest
//! of the infrastructure.
//!
//! The MVP shells out to `scontrol` rather than linking `libslurm`
//! (SPEC.md §64): a single portable binary that works across Slurm versions is
//! worth more here than the efficiency of the C API.

pub mod detect;
pub mod hostlist;
pub mod observe;
pub mod parser;

use std::path::PathBuf;
use std::time::Duration;

use crate::command::{Allowlist, CommandError, CommandOutput, CommandRunner};

pub use crate::inventory::slurm::SlurmView;

/// Program name used when the operator has not given an explicit path.
pub const SCONTROL: &str = "scontrol";

/// Runs `scontrol` and returns its raw output.
///
/// Every call has a timeout: a wedged controller must slow discovery down, not
/// stop the daemon (SPEC.md §120).
#[derive(Debug, Clone)]
pub struct ScontrolClient {
    program: String,
    timeout: Duration,
}

impl Default for ScontrolClient {
    fn default() -> Self {
        Self {
            program: SCONTROL.to_string(),
            timeout: Duration::from_secs(10),
        }
    }
}

impl ScontrolClient {
    /// A client using `scontrol` from `PATH`.
    pub fn new() -> Self {
        Self::default()
    }

    /// A client using an explicit `scontrol` path.
    pub fn with_path(path: Option<PathBuf>) -> Self {
        match path {
            Some(path) => Self {
                program: path.display().to_string(),
                ..Self::default()
            },
            None => Self::default(),
        }
    }

    /// Builder: set the per-invocation timeout.
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// The program this client will run.
    pub fn program(&self) -> &str {
        &self.program
    }

    /// `scontrol show nodes -o`.
    pub async fn show_nodes(&self, allowlist: &Allowlist) -> Result<CommandOutput, CommandError> {
        self.run(&["show", "nodes", "-o"], allowlist).await
    }

    /// `scontrol show partitions -o`.
    pub async fn show_partitions(&self, allowlist: &Allowlist) -> Result<CommandOutput, CommandError> {
        self.run(&["show", "partitions", "-o"], allowlist).await
    }

    /// `scontrol ping`.
    pub async fn ping(&self, allowlist: &Allowlist) -> Result<CommandOutput, CommandError> {
        self.run(&["ping"], allowlist).await
    }

    async fn run(&self, args: &[&str], allowlist: &Allowlist) -> Result<CommandOutput, CommandError> {
        CommandRunner::new(&self.program)
            .args(args)
            .timeout(self.timeout)
            .run(allowlist)
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_client_uses_scontrol_from_path() {
        assert_eq!(ScontrolClient::new().program(), SCONTROL);
        assert_eq!(ScontrolClient::with_path(None).program(), SCONTROL);
    }

    #[test]
    fn an_explicit_path_is_honoured_for_a_non_standard_install() {
        let client = ScontrolClient::with_path(Some(PathBuf::from("/opt/slurm/bin/scontrol")));
        assert_eq!(client.program(), "/opt/slurm/bin/scontrol");
    }

    #[tokio::test]
    async fn a_scontrol_outside_the_allowlist_is_refused() {
        let client = ScontrolClient::with_path(Some(PathBuf::from("/tmp/not-scontrol")));
        let error = client.ping(&Allowlist::builtin()).await.expect_err("must refuse");
        assert!(matches!(error, CommandError::NotAllowed(_)), "{error:?}");
    }

    #[tokio::test]
    async fn an_explicit_path_ending_in_scontrol_passes_the_allowlist_check() {
        // It will fail to spawn (the file does not exist) rather than being
        // refused, which is the distinction under test.
        let client = ScontrolClient::with_path(Some(PathBuf::from("/opt/slurm/bin/scontrol")));
        let error = client.ping(&Allowlist::builtin()).await.expect_err("no such file");
        assert!(matches!(error, CommandError::Spawn { .. }), "{error:?}");
    }
}
