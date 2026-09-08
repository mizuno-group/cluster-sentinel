//! `sentinel install` — generate systemd units and directories.
//!
//! Production deployment is systemd on a native host (IMPLEMENTATION.md §12).
//! The units generated here are hardened by default (IMPLEMENTATION.md §85):
//! Sentinel is a read-only monitoring daemon, and the unit should say so, so
//! that a bug in a probe cannot become a bug in the operating system.
//!
//! `install` also writes a complete configuration file and, on a controller,
//! generates the cluster credential. The alternative -- printing a list of
//! commands for the operator to run -- makes the first five minutes with the
//! binary an exercise in transcription, and the step people skip is always the
//! credential.
//!
//! The credential is written to a file, never passed on a command line or
//! through the environment of a unit someone might paste into a bug report.
//! Nothing that already exists is overwritten without `--force`, so running
//! `install` twice is safe.

use std::path::{Path, PathBuf};

use crate::config::DEFAULT_STATE_DIR;

/// Where the cluster credential lives, beside the configuration it belongs to.
///
/// Derived rather than fixed so that installing with `--config` somewhere else
/// keeps the token, the config and the unit describing the same deployment.
pub fn token_path(config: &Path) -> PathBuf {
    match config.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.join("token"),
        _ => PathBuf::from("token"),
    }
}

/// The systemd unit for the controller.
pub fn controller_unit(binary: &Path, config: &Path) -> String {
    unit(
        "Cluster Sentinel controller",
        &format!("{} --config {} controller", binary.display(), config.display()),
        // The controller owns the database, so it needs to write there.
        &[DEFAULT_STATE_DIR],
        &token_path(config),
    )
}

/// The systemd unit for the agent.
pub fn agent_unit(binary: &Path, config: &Path) -> String {
    unit(
        "Cluster Sentinel agent",
        &format!("{} --config {} agent", binary.display(), config.display()),
        &[DEFAULT_STATE_DIR],
        &token_path(config),
    )
}

/// Build a hardened unit.
fn unit(description: &str, exec_start: &str, writable: &[&str], token: &Path) -> String {
    let token = token.display();
    format!(
        "\
[Unit]
Description={description}
Documentation=https://github.com/mizuno-lab/cluster-sentinel
After=network-online.target
Wants=network-online.target

[Service]
Type=exec
ExecStart={exec_start}
Restart=always
RestartSec=5s

# The credential is read from a file, never from a command line or the
# environment of a unit an operator might paste into a bug report.
Environment=SENTINEL_TOKEN_FILE={token}

# A dedicated unprivileged user. Sentinel reads; it does not need to be root,
# and probes that cannot read something report UNSUPPORTED rather than failing.
User=sentinel
Group=sentinel

# Hardening (IMPLEMENTATION.md §85). Sentinel is a read-only monitoring daemon,
# so the unit says so: a bug in a probe must not become a bug in the OS.
NoNewPrivileges=true
PrivateTmp=true
ProtectHome=true
ProtectSystem=strict
ProtectKernelTunables=true
ProtectKernelModules=true
ProtectControlGroups=true
ProtectClock=true
RestrictSUIDSGID=true
RestrictRealtime=true
RestrictNamespaces=true
LockPersonality=true
MemoryDenyWriteExecute=true
SystemCallArchitectures=native
CapabilityBoundingSet=
AmbientCapabilities=
{}

# Journald is the log destination (IMPLEMENTATION.md §59).
StandardOutput=journal
StandardError=journal

[Install]
WantedBy=multi-user.target
",
        writable
            .iter()
            .map(|path| format!("ReadWritePaths={path}"))
            .collect::<Vec<_>>()
            .join("\n")
    )
}

/// The shell commands an operator runs to finish the install.
/// What `sentinel install` should do beyond writing the unit.
#[derive(Debug, Clone, Copy)]
pub struct Options {
    /// Print everything instead of writing it.
    pub dry_run: bool,
    /// Overwrite files that already exist.
    pub force: bool,
    /// Generate a cluster credential when installing a controller.
    pub credential: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            dry_run: false,
            force: false,
            credential: true,
        }
    }
}

/// One file `install` produced, or would have.
#[derive(Debug, Clone, PartialEq)]
pub struct Written {
    /// Where it went.
    pub path: PathBuf,
    /// What happened to it.
    pub outcome: Outcome,
}

/// What happened to one file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// Written.
    Created,
    /// Left alone because it already existed.
    Kept,
    /// Overwritten because `--force` was given.
    Replaced,
}

impl Outcome {
    fn label(&self) -> &'static str {
        match self {
            Outcome::Created => "created",
            Outcome::Kept => "kept (already present)",
            Outcome::Replaced => "replaced",
        }
    }
}

/// A cluster credential.
///
/// 32 bytes of randomness, hex encoded. Generated here rather than left to the
/// operator because "run this openssl incantation" is a step people skip, and
/// the failure mode of skipping it is a cluster with no credential at all.
fn generate_credential() -> anyhow::Result<String> {
    let mut bytes = [0u8; 32];
    // /dev/urandom rather than a crate: the requirement is 32 unpredictable
    // bytes, and every platform Sentinel runs on has it.
    use std::io::Read;
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut bytes))
        .map_err(|e| anyhow::anyhow!("cannot read /dev/urandom: {e}"))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// Write a file, respecting `--force` and reporting what happened.
fn place(path: &Path, contents: &str, mode: Option<u32>, options: Options) -> anyhow::Result<Written> {
    let exists = path.exists();
    if exists && !options.force {
        return Ok(Written {
            path: path.to_path_buf(),
            outcome: Outcome::Kept,
        });
    }
    if !options.dry_run {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| anyhow::anyhow!("cannot create {}: {e}", parent.display()))?;
            }
        }
        std::fs::write(path, contents).map_err(|e| anyhow::anyhow!("cannot write {}: {e}", path.display()))?;
        if let Some(mode) = mode {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
                    .map_err(|e| anyhow::anyhow!("cannot set mode on {}: {e}", path.display()))?;
            }
            #[cfg(not(unix))]
            let _ = mode;
        }
    }
    Ok(Written {
        path: path.to_path_buf(),
        outcome: if exists { Outcome::Replaced } else { Outcome::Created },
    })
}

/// What to do after `sentinel install`.
///
/// Only the steps `install` genuinely cannot do itself: creating a system
/// user, distributing a credential to other machines, and deciding when to
/// start a service. Everything else has already happened by the time this
/// prints.
pub fn setup_instructions(role: &str, written: &[Written], credential_generated: bool) -> String {
    let mut lines: Vec<String> = vec!["次に:".into(), String::new()];
    let mut step = 0;
    let push = |lines: &mut Vec<String>, step: &mut usize, title: &str, commands: &[String]| {
        *step += 1;
        lines.push(format!("  {}. {title}", *step));
        lines.push(String::new());
        for command in commands {
            lines.push(format!("     {command}"));
        }
        lines.push(String::new());
    };

    push(
        &mut lines,
        &mut step,
        "サービスユーザーとディレクトリを作る:",
        &[
            "sudo useradd --system --no-create-home --shell /usr/sbin/nologin sentinel".into(),
            format!("sudo install -d -o sentinel -g sentinel -m 0750 {DEFAULT_STATE_DIR}"),
            "sudo chown -R sentinel:sentinel /etc/sentinel".into(),
        ],
    );

    let token = written
        .iter()
        .find(|w| w.path.file_name().is_some_and(|n| n == "token"))
        .map(|w| w.path.display().to_string())
        .unwrap_or_else(|| "/etc/sentinel/token".into());

    if credential_generated {
        push(
            &mut lines,
            &mut step,
            "生成した credential を、同じ environment の全 host に同じ内容で配る:",
            &[format!("sudo scp {token} <host>:/etc/sentinel/token")],
        );
    } else {
        push(
            &mut lines,
            &mut step,
            "controller と同じ credential を配置する:",
            &[format!("sudo install -o sentinel -g sentinel -m 0400 token {token}")],
        );
    }

    if let Some(config) = written.iter().find(|w| w.path.extension().is_some_and(|e| e == "toml")) {
        let path = config.path.display().to_string();
        push(
            &mut lines,
            &mut step,
            &format!(
                "{} の \"{}\" の行を書き換えて検証する:",
                path,
                crate::cli::generate::PLACEHOLDER
            ),
            &[format!("sudo -u sentinel sentinel --config {path} config check")],
        );
    }

    push(
        &mut lines,
        &mut step,
        "起動する:",
        &[
            "sudo systemctl daemon-reload".into(),
            format!("sudo systemctl enable --now sentinel-{role}"),
            format!("systemctl status sentinel-{role}"),
        ],
    );

    lines.join("\n")
}

/// Run `sentinel install <role>`.
///
/// Writes everything a host of this role needs: a complete configuration file
/// with every setting at its default, a hardened systemd unit, and -- on a
/// controller -- a generated cluster credential. Nothing that already exists is
/// touched without `--force`, so running it twice is safe.
pub fn run(role: &str, output_dir: &Path, config: &Path, options: Options) -> anyhow::Result<i32> {
    let parsed = crate::cli::generate::Role::parse(role)
        .ok_or_else(|| anyhow::anyhow!("unknown role {role:?}; expected \"controller\" or \"agent\""))?;
    let binary = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("/usr/local/bin/sentinel"));

    let (unit_name, unit_contents) = match parsed {
        crate::cli::generate::Role::Controller => ("sentinel-controller.service", controller_unit(&binary, config)),
        crate::cli::generate::Role::Agent => ("sentinel-agent.service", agent_unit(&binary, config)),
    };
    let unit_path = output_dir.join(unit_name);
    let config_contents = crate::cli::generate::config_file(parsed);

    if options.dry_run {
        println!("# {}\n", unit_path.display());
        print!("{unit_contents}");
        println!("\n# {}\n", config.display());
        print!("{config_contents}");
        return Ok(0);
    }

    let mut written = vec![
        place(config, &config_contents, Some(0o640), options)?,
        place(&unit_path, &unit_contents, Some(0o644), options)?,
    ];

    // Only the controller generates one. An agent that generated its own would
    // produce a credential nothing else in the cluster knows.
    let mut credential_generated = false;
    if parsed == crate::cli::generate::Role::Controller && options.credential {
        let token_file = token_path(config);
        let existing = token_file.exists();
        let token = if existing && !options.force {
            String::new()
        } else {
            generate_credential()?
        };
        let entry = place(&token_file, &format!("{token}\n"), Some(0o400), options)?;
        credential_generated = entry.outcome != Outcome::Kept;
        written.push(entry);
    }

    for entry in &written {
        println!("{:<40} {}", entry.path.display(), entry.outcome.label());
    }
    println!();
    print!("{}", setup_instructions(role, &written, credential_generated));

    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::DEFAULT_CONFIG_PATH;

    fn controller() -> String {
        controller_unit(Path::new("/usr/local/bin/sentinel"), Path::new(DEFAULT_CONFIG_PATH))
    }

    /// A temp directory standing in for /etc/sentinel and /etc/systemd/system.
    struct Sandbox {
        dir: tempfile::TempDir,
    }

    impl Sandbox {
        fn new() -> Self {
            Self {
                dir: tempfile::tempdir().expect("tempdir"),
            }
        }

        fn config(&self) -> PathBuf {
            self.dir.path().join("etc/config.toml")
        }

        fn units(&self) -> PathBuf {
            self.dir.path().join("units")
        }

        fn install(&self, role: &str, options: Options) -> anyhow::Result<i32> {
            run(role, &self.units(), &self.config(), options)
        }
    }

    #[test]
    fn the_unit_runs_the_right_subcommand() {
        assert!(
            controller().contains("ExecStart=/usr/local/bin/sentinel --config /etc/sentinel/config.toml controller")
        );

        let agent = agent_unit(Path::new("/usr/local/bin/sentinel"), Path::new(DEFAULT_CONFIG_PATH));
        assert!(agent.contains("agent"));
        assert!(!agent.contains("controller"));
    }

    #[test]
    fn every_hardening_directive_the_specification_asks_for_is_present() {
        // IMPLEMENTATION.md §85.
        let unit = controller();
        for directive in [
            "NoNewPrivileges=true",
            "PrivateTmp=true",
            "ProtectHome=true",
            "ProtectSystem=strict",
            "ProtectKernelTunables=true",
            "ProtectControlGroups=true",
            "RestrictSUIDSGID=true",
        ] {
            assert!(unit.contains(directive), "missing {directive}");
        }
    }

    #[test]
    fn the_daemon_runs_unprivileged_with_no_capabilities() {
        let unit = controller();
        assert!(unit.contains("User=sentinel"));
        assert!(!unit.contains("User=root"));
        assert!(
            unit.contains("CapabilityBoundingSet="),
            "no capabilities are needed to read"
        );
        assert!(unit.contains("AmbientCapabilities="));
    }

    #[test]
    fn only_the_state_directory_is_writable() {
        // ProtectSystem=strict makes everything read-only; this is the one
        // exception, and it should stay the only one.
        let unit = controller();
        assert_eq!(unit.matches("ReadWritePaths=").count(), 1);
        assert!(unit.contains(&format!("ReadWritePaths={DEFAULT_STATE_DIR}")));
    }

    #[test]
    fn no_credential_is_written_into_the_unit() {
        // A unit file gets pasted into bug reports and checked into
        // configuration management. It must never carry a secret.
        let unit = controller();
        assert!(unit.contains("SENTINEL_TOKEN_FILE="), "the unit points at a file");
        assert!(!unit.contains("SENTINEL_TOKEN="), "and never carries the value");
    }

    #[test]
    fn the_token_lives_beside_the_configuration_it_belongs_to() {
        // Installing elsewhere with --config must not leave the unit pointing
        // at a token from a different deployment.
        let unit = controller_unit(
            Path::new("/usr/local/bin/sentinel"),
            Path::new("/opt/sentinel/config.toml"),
        );
        assert!(unit.contains("SENTINEL_TOKEN_FILE=/opt/sentinel/token"), "{unit}");
    }

    #[test]
    fn the_unit_restarts_and_logs_to_the_journal() {
        let unit = controller();
        assert!(
            unit.contains("Restart=always"),
            "monitoring that stays down helps nobody"
        );
        assert!(unit.contains("StandardOutput=journal"));
    }

    #[test]
    fn a_dry_run_writes_nothing() {
        let sandbox = Sandbox::new();
        let options = Options {
            dry_run: true,
            ..Options::default()
        };
        assert_eq!(sandbox.install("controller", options).expect("dry run"), 0);
        assert!(!sandbox.units().join("sentinel-controller.service").exists());
        assert!(!sandbox.config().exists());
        assert!(!token_path(&sandbox.config()).exists());
    }

    #[test]
    fn installing_an_agent_writes_a_unit_and_a_configuration() {
        let sandbox = Sandbox::new();
        assert_eq!(sandbox.install("agent", Options::default()).expect("install"), 0);

        let unit = std::fs::read_to_string(sandbox.units().join("sentinel-agent.service")).expect("unit");
        assert!(unit.contains("Cluster Sentinel agent"));

        // And the configuration is usable as written, apart from the lines an
        // operator must fill in.
        let config = std::fs::read_to_string(sandbox.config()).expect("config");
        assert!(config.contains("[agent]"));
        assert!(config.contains(crate::cli::generate::PLACEHOLDER));
        let parsed = crate::config::Config::from_toml(&config, &sandbox.config()).expect("parses");
        assert!(crate::config::validate(&parsed).is_ok());
    }

    #[test]
    fn an_agent_is_never_given_a_credential_of_its_own() {
        // One generated locally would be a credential nothing else in the
        // cluster knows, and the agent would fail to register with a message
        // about authentication rather than about configuration.
        let sandbox = Sandbox::new();
        sandbox.install("agent", Options::default()).expect("install");
        assert!(!token_path(&sandbox.config()).exists());
    }

    #[test]
    fn installing_a_controller_generates_a_credential() {
        let sandbox = Sandbox::new();
        sandbox.install("controller", Options::default()).expect("install");

        let token = std::fs::read_to_string(token_path(&sandbox.config())).expect("token");
        let token = token.trim();
        assert!(
            token.len() >= crate::protocol::ClusterCredential::MIN_LENGTH,
            "generated credential is too short: {} chars",
            token.len()
        );
        assert!(crate::protocol::ClusterCredential::new(token).is_strong());
    }

    #[test]
    fn two_generated_credentials_differ() {
        let a = generate_credential().expect("credential");
        let b = generate_credential().expect("credential");
        assert_ne!(a, b);
    }

    #[test]
    #[cfg(unix)]
    fn the_credential_is_readable_only_by_its_owner() {
        use std::os::unix::fs::PermissionsExt;
        let sandbox = Sandbox::new();
        sandbox.install("controller", Options::default()).expect("install");
        let mode = std::fs::metadata(token_path(&sandbox.config()))
            .expect("metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o400, "credential mode is {mode:o}");
    }

    #[test]
    fn running_twice_keeps_what_is_already_there() {
        // Re-running install after an upgrade must not overwrite a tuned
        // configuration, and must not rotate the cluster credential -- which
        // would lock out every agent at once.
        let sandbox = Sandbox::new();
        sandbox.install("controller", Options::default()).expect("first");

        std::fs::write(sandbox.config(), "config_version = 1\nenvironment = \"tuned\"\n").expect("edit");
        let token = std::fs::read_to_string(token_path(&sandbox.config())).expect("token");

        sandbox.install("controller", Options::default()).expect("second");

        assert!(std::fs::read_to_string(sandbox.config())
            .expect("config")
            .contains("tuned"));
        assert_eq!(
            std::fs::read_to_string(token_path(&sandbox.config())).expect("token"),
            token
        );
    }

    #[test]
    fn force_replaces_what_is_there() {
        let sandbox = Sandbox::new();
        sandbox.install("agent", Options::default()).expect("first");
        std::fs::write(sandbox.config(), "overwritten by the operator\n").expect("edit");

        let options = Options {
            force: true,
            ..Options::default()
        };
        sandbox.install("agent", options).expect("second");
        assert!(std::fs::read_to_string(sandbox.config())
            .expect("config")
            .contains("[agent]"));
    }

    #[test]
    fn credential_generation_can_be_declined() {
        // For a site whose credentials come from a secret manager.
        let sandbox = Sandbox::new();
        let options = Options {
            credential: false,
            ..Options::default()
        };
        sandbox.install("controller", options).expect("install");
        assert!(!token_path(&sandbox.config()).exists());
    }

    #[test]
    fn an_unknown_role_is_refused() {
        let sandbox = Sandbox::new();
        assert!(sandbox.install("fileserver", Options::default()).is_err());
    }

    #[test]
    fn the_instructions_cover_the_steps_install_cannot_do_itself() {
        let written = vec![Written {
            path: PathBuf::from("/etc/sentinel/config.toml"),
            outcome: Outcome::Created,
        }];
        let instructions = setup_instructions("controller", &written, true);
        assert!(instructions.contains("useradd --system"), "{instructions}");
        assert!(instructions.contains("config check"), "{instructions}");
        assert!(
            instructions.contains("全 host に同じ内容で"),
            "the credential must be described as cluster-wide: {instructions}"
        );
    }
}
