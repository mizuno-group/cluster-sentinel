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
    let contents = super::generate::config_file(role);

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

fn show(cli: &Cli, json: bool) -> anyhow::Result<i32> {
    let config = Config::load(&cli.config)?;
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
}
