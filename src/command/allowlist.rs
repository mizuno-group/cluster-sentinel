//! Which external programs a probe may execute.
//!
//! Sentinel runs a fixed, small set of read-only diagnostic tools. The
//! allowlist makes that a checked property rather than a convention, and it is
//! the reason the agent RPC can never be turned into a remote shell
//! (SPEC.md §116).

use std::collections::BTreeSet;
use std::path::Path;

use thiserror::Error;

/// A program was not on the allowlist.
#[derive(Debug, Error, PartialEq, Eq)]
#[error("{program} is not an allowed external command")]
pub struct AllowlistError {
    /// The refused program.
    pub program: String,
}

/// The set of programs probes may run.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Allowlist {
    programs: BTreeSet<String>,
}

impl Allowlist {
    /// An empty allowlist: nothing may run.
    pub fn new() -> Self {
        Self::default()
    }

    /// The programs the built-in integrations need. All are read-only
    /// diagnostic tools (SPEC.md §120).
    pub fn builtin() -> Self {
        Self::from([
            "scontrol",
            "sinfo",
            "squeue",
            "systemctl",
            "journalctl",
            "nvidia-smi",
            "zpool",
            "zfs",
            "smartctl",
            "nvme",
            "findmnt",
            "ss",
            "ping",
        ])
    }

    /// Add a program.
    pub fn allow(&mut self, program: impl Into<String>) -> &mut Self {
        self.programs.insert(program.into());
        self
    }

    /// Whether a program may run.
    ///
    /// A program given as a path is matched on its file name, so an operator
    /// may point at `/opt/slurm/bin/scontrol` without widening the allowlist.
    /// The file name is what is checked, never the directory, so no path
    /// prefix can smuggle a different program in.
    pub fn is_allowed(&self, program: &str) -> bool {
        if self.programs.contains(program) {
            return true;
        }
        match Path::new(program).file_name().and_then(|n| n.to_str()) {
            Some(name) => self.programs.contains(name),
            None => false,
        }
    }

    /// Check a program, returning a structured error if it is refused.
    pub fn check(&self, program: &str) -> Result<(), AllowlistError> {
        if self.is_allowed(program) {
            Ok(())
        } else {
            Err(AllowlistError {
                program: program.to_string(),
            })
        }
    }

    /// The allowed programs, in stable order.
    pub fn programs(&self) -> impl Iterator<Item = &str> {
        self.programs.iter().map(String::as_str)
    }
}

impl<I, S> From<I> for Allowlist
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    fn from(programs: I) -> Self {
        Self {
            programs: programs.into_iter().map(Into::into).collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_allowlist_refuses_everything() {
        let allowlist = Allowlist::new();
        assert!(!allowlist.is_allowed("scontrol"));
        assert!(allowlist.check("scontrol").is_err());
    }

    #[test]
    fn a_listed_program_is_allowed_by_bare_name_or_absolute_path() {
        let allowlist = Allowlist::from(["scontrol"]);
        assert!(allowlist.is_allowed("scontrol"));
        assert!(allowlist.is_allowed("/usr/bin/scontrol"));
        assert!(allowlist.is_allowed("/opt/slurm/bin/scontrol"));
    }

    #[test]
    fn an_unlisted_program_stays_refused_whatever_path_it_is_given() {
        let allowlist = Allowlist::from(["scontrol"]);
        assert!(!allowlist.is_allowed("rm"));
        assert!(!allowlist.is_allowed("/bin/rm"));
        assert!(!allowlist.is_allowed("/usr/bin/scontrol/../rm"));
        assert!(!allowlist.is_allowed("scontrol-evil"));
        assert!(
            !allowlist.is_allowed("/opt/scontrol/rm"),
            "the directory must not authorise the program"
        );
    }

    #[test]
    fn the_builtin_list_covers_the_documented_tools_and_nothing_destructive() {
        let allowlist = Allowlist::builtin();
        for expected in [
            "scontrol",
            "squeue",
            "systemctl",
            "journalctl",
            "nvidia-smi",
            "zpool",
            "smartctl",
        ] {
            assert!(allowlist.is_allowed(expected), "{expected} should be allowed");
        }
        for forbidden in [
            "sh",
            "bash",
            "rm",
            "dd",
            "mount",
            "reboot",
            "systemd-run",
            "scancel",
            "srun",
        ] {
            assert!(!allowlist.is_allowed(forbidden), "{forbidden} must never be allowed");
        }
    }

    #[test]
    fn the_error_names_the_refused_program() {
        let error = Allowlist::new().check("rm").expect_err("must refuse");
        assert_eq!(error.program, "rm");
        assert!(error.to_string().contains("rm"));
    }

    #[test]
    fn a_trailing_separator_does_not_bypass_the_check() {
        let allowlist = Allowlist::from(["scontrol"]);
        assert!(!allowlist.is_allowed("/usr/bin/"));
        assert!(!allowlist.is_allowed(""));
    }
}
