//! Logging setup (IMPLEMENTATION.md §82).
//!
//! Structured `tracing` output, journald-friendly by default. Secrets and bulk
//! command output must never be logged; probes attach large material to an
//! observation's evidence instead, where retention limits apply.

use std::io::IsTerminal;

use tracing_subscriber::EnvFilter;

/// Output shape for logs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogFormat {
    /// Human-readable, for a terminal.
    Text,
    /// One JSON object per line, for log shipping.
    Json,
    /// Chosen from whether stderr is a terminal.
    Auto,
}

/// Initialise logging. Called once per process; a second call is ignored so a
/// test harness cannot fail on double initialisation.
pub fn init(verbosity: u8, format: LogFormat) {
    let filter = EnvFilter::try_from_env("SENTINEL_LOG").unwrap_or_else(|_| EnvFilter::new(default_level(verbosity)));

    let json = match format {
        LogFormat::Json => true,
        LogFormat::Text => false,
        LogFormat::Auto => !std::io::stderr().is_terminal(),
    };

    let builder = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr);
    let result = if json {
        builder.json().try_init()
    } else {
        builder.with_target(false).try_init()
    };
    let _ = result;
}

/// Map `-v` repetitions to a filter directive.
fn default_level(verbosity: u8) -> &'static str {
    match verbosity {
        0 => "sentinel=info,warn",
        1 => "sentinel=debug,info",
        _ => "sentinel=trace,debug",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verbosity_increases_the_level_monotonically() {
        assert!(default_level(0).contains("info"));
        assert!(default_level(1).contains("debug"));
        assert!(default_level(2).contains("trace"));
    }

    #[test]
    fn initialising_twice_does_not_panic() {
        init(0, LogFormat::Json);
        init(2, LogFormat::Json);
    }
}
