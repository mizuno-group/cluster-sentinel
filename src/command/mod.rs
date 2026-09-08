//! The common external-command runner (IMPLEMENTATION.md §50).
//!
//! Every external command — `scontrol`, `systemctl`, `journalctl`,
//! `nvidia-smi`, `zpool` — goes through here. Scattered `Command::new()` calls
//! are forbidden, because each one is a chance to forget a timeout, and a
//! monitoring daemon that blocks on a hung command during an incident is worse
//! than no monitoring at all.
//!
//! Guarantees:
//!
//! * a timeout, always;
//! * bounded stdout and stderr, with truncation recorded rather than hidden;
//! * the child is killed when the timeout fires;
//! * only allow-listed programs may run (SPEC.md §116).

mod allowlist;

pub use allowlist::{Allowlist, AllowlistError};

use std::ffi::OsStr;
use std::process::Stdio;
use std::time::{Duration, Instant};

use thiserror::Error;
use tokio::io::AsyncReadExt;

/// Default cap on captured output. Enough for any `scontrol` output; small
/// enough that a runaway `journalctl` cannot exhaust memory.
pub const DEFAULT_OUTPUT_LIMIT: usize = 1024 * 1024;

/// Why a command could not produce a usable result.
#[derive(Debug, Error)]
pub enum CommandError {
    /// The program is not on the allowlist.
    #[error(transparent)]
    NotAllowed(#[from] AllowlistError),
    /// The program could not be started.
    #[error("cannot execute {program}: {source}")]
    Spawn {
        /// The program that could not start.
        program: String,
        /// The underlying I/O error.
        #[source]
        source: std::io::Error,
    },
    /// The command did not finish within its timeout and was killed.
    #[error("{program} exceeded its {timeout:?} timeout and was terminated")]
    Timeout {
        /// The program that hung.
        program: String,
        /// The timeout that fired.
        timeout: Duration,
    },
    /// Reading the child's output failed.
    #[error("cannot read output of {program}: {source}")]
    Io {
        /// The program whose output could not be read.
        program: String,
        /// The underlying I/O error.
        #[source]
        source: std::io::Error,
    },
}

/// A finished command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandOutput {
    /// The program that ran.
    pub program: String,
    /// Its arguments.
    pub args: Vec<String>,
    /// Exit status, `None` if it was terminated by a signal.
    pub exit_code: Option<i32>,
    /// Captured stdout, possibly truncated.
    pub stdout: String,
    /// Captured stderr, possibly truncated.
    pub stderr: String,
    /// Whether stdout was truncated at the limit.
    pub stdout_truncated: bool,
    /// Whether stderr was truncated at the limit.
    pub stderr_truncated: bool,
    /// How long it took.
    pub duration: Duration,
}

impl CommandOutput {
    /// Whether the command exited zero.
    pub fn is_success(&self) -> bool {
        self.exit_code == Some(0)
    }

    /// A short description for an observation's evidence.
    pub fn command_line(&self) -> String {
        if self.args.is_empty() {
            self.program.clone()
        } else {
            format!("{} {}", self.program, self.args.join(" "))
        }
    }
}

/// A command to run.
#[derive(Debug, Clone)]
pub struct CommandRunner {
    program: String,
    args: Vec<String>,
    timeout: Duration,
    output_limit: usize,
}

impl CommandRunner {
    /// Prepare a command. Nothing runs until [`CommandRunner::run`].
    pub fn new(program: impl Into<String>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            timeout: Duration::from_secs(5),
            output_limit: DEFAULT_OUTPUT_LIMIT,
        }
    }

    /// Builder: add an argument.
    pub fn arg(mut self, arg: impl AsRef<OsStr>) -> Self {
        self.args.push(arg.as_ref().to_string_lossy().into_owned());
        self
    }

    /// Builder: add several arguments.
    pub fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        for arg in args {
            self.args.push(arg.as_ref().to_string_lossy().into_owned());
        }
        self
    }

    /// Builder: set the timeout.
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Builder: set the captured-output limit in bytes.
    pub fn output_limit(mut self, limit: usize) -> Self {
        self.output_limit = limit;
        self
    }

    /// Run the command, enforcing the allowlist, the timeout and the limits.
    pub async fn run(&self, allowlist: &Allowlist) -> Result<CommandOutput, CommandError> {
        allowlist.check(&self.program)?;

        let started = Instant::now();
        let mut child = tokio::process::Command::new(&self.program)
            .args(&self.args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|source| CommandError::Spawn {
                program: self.program.clone(),
                source,
            })?;

        let mut stdout_pipe = child.stdout.take();
        let mut stderr_pipe = child.stderr.take();
        let limit = self.output_limit;

        let collect = async {
            // Read both pipes concurrently: a child that fills stderr while we
            // are still draining stdout would otherwise deadlock.
            let stdout = async {
                match stdout_pipe.as_mut() {
                    Some(pipe) => read_limited(pipe, limit).await,
                    None => Ok((Vec::new(), false)),
                }
            };
            let stderr = async {
                match stderr_pipe.as_mut() {
                    Some(pipe) => read_limited(pipe, limit).await,
                    None => Ok((Vec::new(), false)),
                }
            };
            let (stdout, stderr) = tokio::try_join!(stdout, stderr)?;
            let status = child.wait().await?;
            Ok::<_, std::io::Error>((stdout, stderr, status))
        };

        match tokio::time::timeout(self.timeout, collect).await {
            Ok(Ok(((stdout, stdout_truncated), (stderr, stderr_truncated), status))) => Ok(CommandOutput {
                program: self.program.clone(),
                args: self.args.clone(),
                exit_code: status.code(),
                stdout: String::from_utf8_lossy(&stdout).into_owned(),
                stderr: String::from_utf8_lossy(&stderr).into_owned(),
                stdout_truncated,
                stderr_truncated,
                duration: started.elapsed(),
            }),
            Ok(Err(source)) => Err(CommandError::Io {
                program: self.program.clone(),
                source,
            }),
            Err(_) => {
                // `kill_on_drop` handles the process; be explicit anyway so the
                // child is gone before we return.
                let _ = child.start_kill();
                Err(CommandError::Timeout {
                    program: self.program.clone(),
                    timeout: self.timeout,
                })
            }
        }
    }
}

/// Read at most `limit` bytes, reporting whether more was available.
async fn read_limited<R: tokio::io::AsyncRead + Unpin>(
    reader: &mut R,
    limit: usize,
) -> std::io::Result<(Vec<u8>, bool)> {
    let mut buffer = Vec::new();
    // Read one byte past the limit so truncation can be detected rather than
    // guessed from an exactly-full buffer.
    let mut handle = reader.take((limit + 1) as u64);
    handle.read_to_end(&mut buffer).await?;
    let truncated = buffer.len() > limit;
    buffer.truncate(limit);
    Ok((buffer, truncated))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_allowlist() -> Allowlist {
        Allowlist::from([
            "/bin/sh",
            "/bin/echo",
            "/bin/sleep",
            "sh",
            "echo",
            "sleep",
            "true",
            "false",
        ])
    }

    #[tokio::test]
    async fn a_successful_command_captures_stdout() {
        let output = CommandRunner::new("sh")
            .args(["-c", "printf 'hello world'"])
            .run(&test_allowlist())
            .await
            .expect("run");
        assert!(output.is_success());
        assert_eq!(output.stdout, "hello world");
        assert!(!output.stdout_truncated);
    }

    #[tokio::test]
    async fn a_failing_command_reports_its_exit_code_rather_than_erroring() {
        // A non-zero exit is data, not a runner failure: `scontrol` failing is
        // exactly what a probe needs to observe.
        let output = CommandRunner::new("sh")
            .args(["-c", "exit 3"])
            .run(&test_allowlist())
            .await
            .expect("run");
        assert_eq!(output.exit_code, Some(3));
        assert!(!output.is_success());
    }

    #[tokio::test]
    async fn stderr_is_captured_separately() {
        let output = CommandRunner::new("sh")
            .args(["-c", "printf out; printf err >&2"])
            .run(&test_allowlist())
            .await
            .expect("run");
        assert_eq!(output.stdout, "out");
        assert_eq!(output.stderr, "err");
    }

    #[tokio::test]
    async fn a_hanging_command_is_killed_at_the_timeout() {
        let started = Instant::now();
        let error = CommandRunner::new("sh")
            .args(["-c", "sleep 30"])
            .timeout(Duration::from_millis(200))
            .run(&test_allowlist())
            .await
            .expect_err("must time out");
        assert!(matches!(error, CommandError::Timeout { .. }), "{error:?}");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the timeout must actually cut it short"
        );
    }

    #[tokio::test]
    async fn oversized_output_is_truncated_and_the_truncation_is_recorded() {
        let output = CommandRunner::new("sh")
            .args(["-c", "printf 'x%.0s' $(seq 1 5000)"])
            .output_limit(100)
            .run(&test_allowlist())
            .await
            .expect("run");
        assert_eq!(output.stdout.len(), 100);
        assert!(output.stdout_truncated, "truncation must be visible, not silent");
    }

    #[tokio::test]
    async fn output_exactly_at_the_limit_is_not_reported_as_truncated() {
        let output = CommandRunner::new("sh")
            .args(["-c", "printf '0123456789'"])
            .output_limit(10)
            .run(&test_allowlist())
            .await
            .expect("run");
        assert_eq!(output.stdout.len(), 10);
        assert!(!output.stdout_truncated);
    }

    #[tokio::test]
    async fn a_large_stderr_does_not_deadlock_a_large_stdout() {
        let output = CommandRunner::new("sh")
            .args(["-c", "printf 'y%.0s' $(seq 1 20000); printf 'z%.0s' $(seq 1 20000) >&2"])
            .timeout(Duration::from_secs(10))
            .run(&test_allowlist())
            .await
            .expect("run");
        assert_eq!(output.stdout.len(), 20000);
        assert_eq!(output.stderr.len(), 20000);
    }

    #[tokio::test]
    async fn a_program_outside_the_allowlist_is_refused_before_it_runs() {
        let error = CommandRunner::new("rm")
            .args(["-rf", "/"])
            .run(&test_allowlist())
            .await
            .expect_err("must refuse");
        assert!(matches!(error, CommandError::NotAllowed(_)), "{error:?}");
    }

    #[tokio::test]
    async fn a_missing_program_reports_a_spawn_error() {
        let allowlist = Allowlist::from(["definitely-not-a-real-program"]);
        let error = CommandRunner::new("definitely-not-a-real-program")
            .run(&allowlist)
            .await
            .expect_err("must fail");
        assert!(matches!(error, CommandError::Spawn { .. }), "{error:?}");
    }

    #[tokio::test]
    async fn the_command_line_is_reconstructable_for_evidence() {
        let output = CommandRunner::new("echo")
            .args(["a", "b"])
            .run(&test_allowlist())
            .await
            .expect("run");
        assert_eq!(output.command_line(), "echo a b");
    }
}
