//! Command-line interface.
//!
//! One binary, many subcommands (SPEC.md §40, IMPLEMENTATION.md §1). Anything
//! not yet implemented says so rather than pretending to succeed.

mod config_cmd;
mod daemon_cmd;
pub mod generate;
mod install_cmd;
mod run_cmd;
mod status_cmd;
mod version_cmd;

pub use status_cmd::{EntityStatus, StatusReport};

use std::path::PathBuf;

use clap::{Parser, Subcommand};

use crate::config::DEFAULT_CONFIG_PATH;
use crate::telemetry::LogFormat;

/// Cluster Sentinel.
#[derive(Debug, Parser)]
#[command(name = "sentinel", version, about, long_about = None)]
pub struct Cli {
    /// Path to the configuration file.
    #[arg(long, short = 'c', global = true, env = "SENTINEL_CONFIG", default_value = DEFAULT_CONFIG_PATH)]
    pub config: PathBuf,

    /// Increase log verbosity; repeat for more.
    #[arg(long, short = 'v', global = true, action = clap::ArgAction::Count)]
    pub verbose: u8,

    /// Emit logs as JSON.
    #[arg(long, global = true)]
    pub log_json: bool,

    /// The subcommand to run.
    #[command(subcommand)]
    pub command: Command,
}

impl Cli {
    /// Log format implied by the flags.
    pub fn log_format(&self) -> LogFormat {
        if self.log_json {
            LogFormat::Json
        } else {
            LogFormat::Auto
        }
    }
}

/// Top-level subcommands.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Print version information.
    Version {
        /// Emit JSON instead of text.
        #[arg(long)]
        json: bool,
    },
    /// Inspect and validate configuration.
    Config {
        /// The configuration subcommand.
        #[command(subcommand)]
        command: ConfigCommand,
    },
    /// Show the state of the environment.
    Status {
        /// Emit JSON instead of text.
        #[arg(long)]
        json: bool,
    },
    /// Run one inventory discovery cycle now.
    Discover {
        /// Emit JSON instead of text.
        #[arg(long)]
        json: bool,
    },
    /// Inspect entities.
    Entity {
        /// The entity subcommand.
        #[command(subcommand)]
        command: EntityCommand,
    },
    /// Inspect the dependency graph.
    Dependency {
        /// The dependency subcommand.
        #[command(subcommand)]
        command: DependencyCommand,
    },
    /// Inspect incidents.
    Incident {
        /// The incident subcommand.
        #[command(subcommand)]
        command: IncidentCommand,
    },
    /// Show who observes whom.
    Peers {
        /// Emit JSON instead of text.
        #[arg(long)]
        json: bool,
    },
    /// Explain what is currently wrong, and why.
    Diagnose {
        /// Emit JSON instead of text.
        #[arg(long)]
        json: bool,
    },
    /// Run the controller daemon.
    Controller,
    /// Run the agent daemon.
    Agent,
    /// Set this host up for a role: config file, systemd unit, directories.
    Install {
        /// `controller` or `agent`.
        role: String,
        /// Where to write the systemd unit.
        #[arg(long, default_value = "/etc/systemd/system")]
        output_dir: PathBuf,
        /// Print everything that would be written, without writing it.
        #[arg(long)]
        dry_run: bool,
        /// Overwrite files that already exist.
        #[arg(long)]
        force: bool,
        /// Do not generate a cluster credential on the controller.
        #[arg(long)]
        no_credential: bool,
    },
    /// Delete records that have outlived their retention period.
    Prune {
        /// Report what would be deleted without deleting it.
        #[arg(long)]
        dry_run: bool,
        /// Return freed space to the filesystem afterwards.
        ///
        /// Rewrites the whole database, so it is opt-in.
        #[arg(long)]
        vacuum: bool,
        /// Override the configured observation retention for this run.
        #[arg(long, value_name = "PERIOD")]
        observations: Option<String>,
        /// Emit JSON instead of text.
        #[arg(long)]
        json: bool,
    },
    /// Report what this host looks like to Sentinel, and why.
    Doctor {
        /// Emit JSON instead of text.
        #[arg(long)]
        json: bool,
    },
}

/// `sentinel entity ...`.
#[derive(Debug, Subcommand)]
pub enum EntityCommand {
    /// List entities.
    List {
        /// Only entities of this type.
        #[arg(long = "type")]
        entity_type: Option<String>,
        /// Emit JSON instead of text.
        #[arg(long)]
        json: bool,
    },
    /// Show one entity in detail, by name or by id.
    Show {
        /// Entity canonical name or id.
        name: String,
        /// Emit JSON instead of text.
        #[arg(long)]
        json: bool,
    },
}

/// `sentinel incident ...`.
#[derive(Debug, Subcommand)]
pub enum IncidentCommand {
    /// List incidents.
    List {
        /// Include resolved incidents as well as active ones.
        #[arg(long)]
        all: bool,
        /// Emit JSON instead of text.
        #[arg(long)]
        json: bool,
    },
    /// Show one incident in full, with its timeline and evidence.
    Show {
        /// Incident id.
        id: String,
        /// Emit JSON instead of text.
        #[arg(long)]
        json: bool,
    },
}

/// `sentinel dependency ...`.
#[derive(Debug, Subcommand)]
pub enum DependencyCommand {
    /// List dependency edges.
    List {
        /// Emit JSON instead of text.
        #[arg(long)]
        json: bool,
    },
}

/// `sentinel config ...`.
#[derive(Debug, Subcommand)]
pub enum ConfigCommand {
    /// Validate the configuration file.
    Check {
        /// Emit JSON instead of text.
        #[arg(long)]
        json: bool,
    },
    /// Show the effective configuration and where each value came from.
    Show {
        /// Emit JSON instead of text.
        #[arg(long)]
        json: bool,
    },
    /// Write a complete configuration file with every setting at its default.
    Init {
        /// `controller` or `agent`.
        #[arg(long, default_value = "agent")]
        role: String,
        /// Where to write it. Defaults to the configured config path.
        #[arg(long)]
        output: Option<PathBuf>,
        /// Print it instead of writing it.
        #[arg(long)]
        dry_run: bool,
        /// Overwrite an existing file.
        #[arg(long)]
        force: bool,
    },
}

/// Run the CLI. Returns the process exit code.
pub async fn run(cli: Cli) -> anyhow::Result<i32> {
    match &cli.command {
        Command::Version { json } => version_cmd::run(*json),
        Command::Config { command } => config_cmd::run(&cli, command).await,
        Command::Status { json } => run_cmd::status(&cli, *json).await,
        Command::Discover { json } => run_cmd::discover(&cli, *json).await,
        Command::Entity { command } => run_cmd::entity(&cli, command).await,
        Command::Dependency { command } => run_cmd::dependency(&cli, command).await,
        Command::Incident { command } => run_cmd::incident(&cli, command).await,
        Command::Peers { json } => run_cmd::peers(&cli, *json).await,
        Command::Diagnose { json } => run_cmd::diagnose(&cli, *json).await,
        Command::Controller => daemon_cmd::controller(&cli).await,
        Command::Agent => daemon_cmd::agent(&cli).await,
        Command::Install {
            role,
            output_dir,
            dry_run,
            force,
            no_credential,
        } => install_cmd::run(
            role,
            output_dir,
            &cli.config,
            install_cmd::Options {
                dry_run: *dry_run,
                force: *force,
                credential: !*no_credential,
            },
        ),
        Command::Prune {
            dry_run,
            vacuum,
            observations,
            json,
        } => run_cmd::prune(&cli, *dry_run, *vacuum, observations.as_deref(), *json).await,
        Command::Doctor { json } => daemon_cmd::doctor(&cli, *json).await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn the_cli_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn version_parses() {
        let cli = Cli::try_parse_from(["sentinel", "version"]).expect("parse");
        assert!(matches!(cli.command, Command::Version { json: false }));
    }

    #[test]
    fn config_check_parses_with_a_custom_path() {
        let cli = Cli::try_parse_from(["sentinel", "--config", "/tmp/x.toml", "config", "check"]).expect("parse");
        assert_eq!(cli.config, PathBuf::from("/tmp/x.toml"));
        assert!(matches!(
            cli.command,
            Command::Config {
                command: ConfigCommand::Check { json: false }
            }
        ));
    }

    #[test]
    fn the_config_path_defaults_to_the_documented_location() {
        let cli = Cli::try_parse_from(["sentinel", "version"]).expect("parse");
        assert_eq!(cli.config, PathBuf::from(DEFAULT_CONFIG_PATH));
    }

    #[test]
    fn verbosity_counts_up() {
        let cli = Cli::try_parse_from(["sentinel", "-vv", "version"]).expect("parse");
        assert_eq!(cli.verbose, 2);
    }

    #[test]
    fn the_read_only_verbs_parse() {
        assert!(matches!(
            Cli::try_parse_from(["sentinel", "status"]).expect("parse").command,
            Command::Status { json: false }
        ));
        assert!(matches!(
            Cli::try_parse_from(["sentinel", "entity", "list"])
                .expect("parse")
                .command,
            Command::Entity {
                command: EntityCommand::List {
                    entity_type: None,
                    json: false
                }
            }
        ));
        assert!(matches!(
            Cli::try_parse_from(["sentinel", "dependency", "list"])
                .expect("parse")
                .command,
            Command::Dependency {
                command: DependencyCommand::List { json: false }
            }
        ));
    }

    #[test]
    fn entity_show_takes_a_name_and_entity_list_takes_a_type_filter() {
        let cli = Cli::try_parse_from(["sentinel", "entity", "show", "node-a"]).expect("parse");
        match cli.command {
            Command::Entity {
                command: EntityCommand::Show { name, .. },
            } => assert_eq!(name, "node-a"),
            other => panic!("unexpected command {other:?}"),
        }

        let cli = Cli::try_parse_from(["sentinel", "entity", "list", "--type", "storage"]).expect("parse");
        match cli.command {
            Command::Entity {
                command: EntityCommand::List { entity_type, .. },
            } => {
                assert_eq!(entity_type.as_deref(), Some("storage"));
            }
            other => panic!("unexpected command {other:?}"),
        }
    }

    #[test]
    fn every_verb_supports_json_output_for_scripting() {
        for args in [
            vec!["sentinel", "status", "--json"],
            vec!["sentinel", "discover", "--json"],
            vec!["sentinel", "entity", "list", "--json"],
            vec!["sentinel", "dependency", "list", "--json"],
            vec!["sentinel", "diagnose", "--json"],
            vec!["sentinel", "peers", "--json"],
            vec!["sentinel", "incident", "list", "--json"],
            vec!["sentinel", "version", "--json"],
            vec!["sentinel", "config", "check", "--json"],
        ] {
            assert!(Cli::try_parse_from(&args).is_ok(), "{args:?} should parse");
        }
    }

    #[test]
    fn the_daemon_verbs_parse() {
        assert!(matches!(
            Cli::try_parse_from(["sentinel", "controller"]).expect("parse").command,
            Command::Controller
        ));
        assert!(matches!(
            Cli::try_parse_from(["sentinel", "agent"]).expect("parse").command,
            Command::Agent
        ));
        assert!(matches!(
            Cli::try_parse_from(["sentinel", "doctor"]).expect("parse").command,
            Command::Doctor { json: false }
        ));
    }

    #[test]
    fn install_parses() {
        let cli = Cli::try_parse_from(["sentinel", "install", "agent", "--dry-run"]).expect("parse");
        match cli.command {
            Command::Install { role, dry_run, .. } => {
                assert_eq!(role, "agent");
                assert!(dry_run);
            }
            other => panic!("unexpected command {other:?}"),
        }
    }

    #[test]
    fn there_is_no_subcommand_that_runs_something_on_a_host() {
        // SPEC.md §116, at the CLI surface as well as the wire surface.
        for forbidden in ["exec", "run", "shell", "restart", "reboot", "drain", "resume"] {
            assert!(
                Cli::try_parse_from(["sentinel", forbidden]).is_err(),
                "`sentinel {forbidden}` must not exist"
            );
        }
    }

    #[test]
    fn an_unknown_subcommand_is_rejected() {
        assert!(Cli::try_parse_from(["sentinel", "teleport"]).is_err());
    }
}
