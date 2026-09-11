//! `sentinel config check` and `sentinel config show`.

use serde::Serialize;

use crate::config::{validate, Config, IssueSeverity};

use super::{Cli, ConfigCommand};

/// Run a configuration subcommand.
pub async fn run(cli: &Cli, command: &ConfigCommand) -> anyhow::Result<i32> {
    match command {
        ConfigCommand::Check { json } => check(cli, *json),
        ConfigCommand::Show { json } => show(cli, *json),
        ConfigCommand::Init {
            role,
            output,
            dry_run,
            force,
        } => init(cli, role, output.as_deref(), *dry_run, *force),
    }
}

/// `sentinel config init`.
pub fn init(
    cli: &Cli,
    role: &str,
    output: Option<&std::path::Path>,
    dry_run: bool,
    force: bool,
) -> anyhow::Result<i32> {
    let role = super::generate::Role::parse(role)
        .ok_or_else(|| anyhow::anyhow!("unknown role {role:?}; expected \"controller\" or \"agent\""))?;
    let contents = super::generate::config_file_for(
        role,
        super::generate::Detected::on_this_host(&crate::agent::system::LinuxInspector::new()),
    );

    if dry_run {
        print!("{contents}");
        return Ok(0);
    }

    let path = output.unwrap_or(cli.config.as_path());
    // Never silently over an existing file: a configuration someone tuned is
    // not something to lose to a mistyped command.
    if path.exists() && !force {
        anyhow::bail!(
            "{} already exists; pass --force to overwrite, or --dry-run to see what would be written",
            path.display()
        );
    }
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).map_err(|e| anyhow::anyhow!("cannot create {}: {e}", parent.display()))?;
        }
    }
    std::fs::write(path, &contents).map_err(|e| anyhow::anyhow!("cannot write {}: {e}", path.display()))?;

    println!("wrote {} ({} 設定)", path.display(), role.as_str());
    println!();
    println!("次に:");
    println!("  1. \"{}\" の行を書き換える", super::generate::PLACEHOLDER);
    println!("  2. sentinel --config {} config check", path.display());
    Ok(0)
}

#[derive(Debug, Serialize)]
struct IssueJson {
    severity: String,
    location: String,
    message: String,
}

#[derive(Debug, Serialize)]
struct CheckJson {
    path: String,
    ok: bool,
    issues: Vec<IssueJson>,
}

fn check(cli: &Cli, json: bool) -> anyhow::Result<i32> {
    let config = match Config::load(&cli.config) {
        Ok(config) => config,
        Err(error) => {
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&CheckJson {
                        path: cli.config.display().to_string(),
                        ok: false,
                        issues: vec![IssueJson {
                            severity: "error".into(),
                            location: "<file>".into(),
                            message: error.to_string(),
                        }],
                    })?
                );
            } else {
                eprintln!("error: {error}");
            }
            return Ok(1);
        }
    };

    let report = validate(&config);
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&CheckJson {
                path: cli.config.display().to_string(),
                ok: report.is_ok(),
                issues: report
                    .issues
                    .iter()
                    .map(|issue| IssueJson {
                        severity: issue.severity.to_string(),
                        location: issue.location.clone(),
                        message: issue.message.clone(),
                    })
                    .collect(),
            })?
        );
    } else {
        for issue in &report.issues {
            let stream: &mut dyn std::io::Write = &mut std::io::stderr();
            let _ = writeln!(stream, "{issue}");
        }
        if report.is_ok() {
            let warnings = report
                .issues
                .iter()
                .filter(|i| i.severity == IssueSeverity::Warning)
                .count();
            println!("{}: ok ({warnings} warning(s))", cli.config.display());
        } else {
            println!("{}: invalid", cli.config.display());
        }
    }

    Ok(if report.is_ok() { 0 } else { 1 })
}

/// What a redacted secret is replaced with.
///
/// Named rather than blanked so the reader can tell "this was set and is
/// hidden" from "this was never set", which are different problems.
pub const REDACTED: &str = "(redacted — see the configuration file)";

/// Hide the secrets in a configuration that is about to be printed.
///
/// A webhook URL is a credential: anyone holding one can post into the channel
/// it names. It is the reason the configuration file is not world-readable,
/// and printing it here undid that -- this output is exactly the sort of thing
/// that gets pasted into a chat message or an issue while someone asks for
/// help. The credential file is kept out of unit files and environments for
/// the same reason.
fn redacted(mut config: Config) -> Config {
    for webhook in &mut config.notification.webhooks {
        if !webhook.url.is_empty() {
            webhook.url = REDACTED.to_string();
        }
    }
    config
}

fn show(cli: &Cli, json: bool) -> anyhow::Result<i32> {
    let config = redacted(Config::load(&cli.config)?);
    if json {
        println!("{}", serde_json::to_string_pretty(&config)?);
    } else {
        println!("{}", toml::to_string_pretty(&config)?);
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::Command;
    use std::io::Write;

    fn cli_for(path: &std::path::Path) -> Cli {
        Cli {
            config: path.to_path_buf(),
            verbose: 0,
            log_json: false,
            command: Command::Config {
                command: ConfigCommand::Check { json: false },
            },
        }
    }

    fn write_config(text: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        let mut file = std::fs::File::create(&path).expect("create");
        file.write_all(text.as_bytes()).expect("write");
        (dir, path)
    }

    #[test]
    fn a_valid_config_checks_out() {
        let (_dir, path) = write_config("config_version = 1\nenvironment = \"lab\"\n");
        assert_eq!(check(&cli_for(&path), false).expect("check"), 0);
        assert_eq!(check(&cli_for(&path), true).expect("check json"), 0);
    }

    #[test]
    fn an_invalid_config_exits_nonzero() {
        let (_dir, path) = write_config("config_version = 1\nenvironment = \"\"\n");
        assert_eq!(check(&cli_for(&path), false).expect("check"), 1);
    }

    #[test]
    fn a_missing_file_exits_nonzero_without_panicking() {
        let cli = cli_for(std::path::Path::new("/nonexistent/sentinel/config.toml"));
        assert_eq!(check(&cli, false).expect("check"), 1);
        assert_eq!(check(&cli, true).expect("check json"), 1);
    }

    #[test]
    fn a_future_config_version_exits_nonzero() {
        let (_dir, path) = write_config("config_version = 4242\n");
        assert_eq!(check(&cli_for(&path), false).expect("check"), 1);
    }

    #[test]
    fn show_renders_the_effective_configuration() {
        let (_dir, path) = write_config("config_version = 1\nenvironment = \"lab\"\n");
        assert_eq!(show(&cli_for(&path), true).expect("show json"), 0);
        assert_eq!(show(&cli_for(&path), false).expect("show toml"), 0);
    }

    #[test]
    fn a_webhook_url_is_not_printed() {
        // It is a credential: anyone holding it can post into that channel.
        // This output gets pasted into chat messages while asking for help,
        // which is the whole reason the configuration file is not
        // world-readable in the first place.
        let config = Config::from_toml(
            "config_version = 1\nenvironment = \"lab\"\n\n\
             [[notification.webhooks]]\nname = \"ops\"\n\
             url = \"https://hooks.slack.com/services/T0/B0/XXXXXXXX\"\n",
            std::path::Path::new("test.toml"),
        )
        .expect("config");

        let shown = redacted(config);
        assert_eq!(shown.notification.webhooks[0].url, REDACTED);
        assert_eq!(
            shown.notification.webhooks[0].name, "ops",
            "the name still has to be usable with --provider"
        );

        let json = serde_json::to_string(&shown).expect("json");
        assert!(!json.contains("XXXXXXXX"), "{json}");
        assert!(!json.contains("hooks.slack.com"), "{json}");
    }

    #[test]
    fn a_configuration_with_no_webhooks_is_unchanged() {
        let config = Config::from_toml(
            "config_version = 1\nenvironment = \"lab\"\n",
            std::path::Path::new("test.toml"),
        )
        .expect("config");
        assert_eq!(redacted(config.clone()), config);
    }

    #[test]
    fn everything_the_ansible_role_reads_survives_redaction() {
        // The role distributes settings by reading `config show --json` on the
        // controller. Redaction must not take away what it needs, or a fleet
        // stops being configurable from one place.
        let config = Config::from_toml(
            "config_version = 1\nenvironment = \"lab\"\n\n\
             [controller]\nlisten = \"0.0.0.0:7443\"\n\n\
             [probes.\"network.tcp\"]\ninterval = \"9s\"\n\n\
             [[notification.webhooks]]\nname = \"ops\"\nurl = \"https://example.invalid/x\"\n",
            std::path::Path::new("test.toml"),
        )
        .expect("config");

        let shown = redacted(config);
        assert_eq!(shown.environment, "lab");
        assert_eq!(shown.controller.listen, "0.0.0.0:7443");
        assert!(shown.probes.get("network.tcp").is_some());
    }
}
