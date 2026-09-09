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
//! `install` twice is safe -- and an existing credential is never overwritten
//! at all, because rotating one locks out every agent in the cluster and is
//! not something a flag meaning "write my files again" should do.

use std::path::{Path, PathBuf};

use crate::config::DEFAULT_STATE_DIR;

/// The unprivileged user the systemd units run as.
///
/// Named here as well as in the unit because the files this command writes
/// have to be readable by it: a configuration owned by root at mode 0640 is
/// one the service cannot open, and the symptom is a restart loop rather than
/// anything that mentions permissions.
pub const SERVICE_USER: &str = "sentinel";

/// Look up a user's ids in the contents of `/etc/passwd`.
///
/// Parsed rather than resolved through libc, so this needs no C dependency and
/// can be tested against a string. `None` means the user does not exist yet,
/// which is the ordinary case on a fresh host: the operator creates it from
/// the instructions this command prints, and the chown comes with it.
pub fn passwd_ids(passwd: &str, user: &str) -> Option<(u32, u32)> {
    passwd.lines().find_map(|line| {
        let mut fields = line.split(':');
        (fields.next()? == user).then_some(())?;
        let uid = fields.nth(1)?.parse().ok()?;
        let gid = fields.next()?.parse().ok()?;
        Some((uid, gid))
    })
}

/// The service user's ids on this host, if it exists.
#[cfg(unix)]
fn service_user_ids() -> Option<(u32, u32)> {
    passwd_ids(&std::fs::read_to_string("/etc/passwd").ok()?, SERVICE_USER)
}

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
Documentation=https://github.com/Lzh-Function/cluster-sentinel
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

# systemd creates this on first start, owned by the service user, so a host
# that has only ever had the binary copied onto it needs no mkdir.
StateDirectory=sentinel

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
pub struct Options<'a> {
    /// Where the unit should say the binary lives.
    ///
    /// Defaults to the running executable, which is right once the binary is
    /// installed and wrong while it is still in a download directory -- the
    /// unit would point at a path that will not exist tomorrow.
    pub binary: Option<&'a Path>,
    /// Print everything instead of writing it.
    pub dry_run: bool,
    /// Overwrite files that already exist.
    pub force: bool,
    /// Generate a cluster credential when installing a controller.
    pub credential: bool,
}

impl Default for Options<'_> {
    fn default() -> Self {
        Self {
            binary: None,
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

        // Hand the file to the service user when that user already exists.
        // Without this, installing a second role beside one that is already
        // running -- an agent on the controller host, say -- writes a
        // root-owned 0640 config the service cannot read, and systemd reports
        // only a restart loop. Best effort: on a fresh host the user does not
        // exist yet and the printed instructions still cover it.
        #[cfg(unix)]
        if let Some((uid, gid)) = service_user_ids() {
            let _ = std::os::unix::fs::chown(path, Some(uid), Some(gid));
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
pub fn setup_instructions(
    role: &str,
    written: &[Written],
    credential_generated: bool,
    service_user_exists: bool,
) -> String {
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

    // A host that already runs one role has the user and the directory, and
    // printing "create the service user" there invites the whole step to be
    // skipped -- including the parts that still matter. This happens whenever
    // a second role is installed beside a running one, so it is worth saying
    // precisely which of it is already done.
    if service_user_exists {
        lines.push("  サービスユーザー sentinel は既にあります。".into());
        lines.push("  この install が書いたファイルの所有者も設定済みです。".into());
        lines.push(String::new());
    } else {
        push(
            &mut lines,
            &mut step,
            "サービスユーザーとディレクトリを作る:",
            &[
                "sudo useradd --system --no-create-home --shell /usr/sbin/nologin sentinel".into(),
                format!("sudo install -d -o sentinel -g sentinel -m 0750 {DEFAULT_STATE_DIR}"),
                "sudo chown -R sentinel:sentinel /etc/sentinel".into(),
                // Without this the journal probe reports UNSUPPORTED on a host
                // that has a perfectly readable journal, and the kernel events
                // it exists to preserve are never collected.
                "sudo usermod -aG systemd-journal sentinel   # journal.events を有効にする".into(),
            ],
        );
    }

    let token = written
        .iter()
        .find(|w| w.path.file_name().is_some_and(|n| n == "token"))
        .map(|w| w.path.display().to_string())
        .unwrap_or_else(|| "/etc/sentinel/token".into());

    // Already present whenever another role is installed here, and telling
    // someone to place a credential that is sitting next to the config they
    // just wrote is how a working one gets replaced.
    let token_exists = written
        .iter()
        .find(|w| w.path.file_name().is_some_and(|n| n == "token"))
        .map(|w| w.path.clone())
        .unwrap_or_else(|| PathBuf::from("/etc/sentinel/token"))
        .exists();

    if credential_generated {
        push(
            &mut lines,
            &mut step,
            "生成した credential を、同じ environment の全 host に同じ内容で配る:",
            &[format!("sudo scp {token} <host>:/etc/sentinel/token")],
        );
    } else if !token_exists {
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

/// Where an installed binary is expected to live.
pub const DEFAULT_BINARY_PATH: &str = "/usr/local/bin/sentinel";

/// Directories a binary can live in and still be there after a reboot.
const DURABLE_BINARY_DIRECTORIES: &[&str] = &["/usr/local/bin", "/usr/bin", "/usr/sbin", "/opt", "/usr/local/sbin"];

/// Warn if the unit would point somewhere the binary will not stay.
///
/// The likeliest first run is `./sentinel-x86_64-... install agent` from a
/// download directory, which writes a unit naming a path in `/home` or `/tmp`.
/// It starts once and fails after the directory is cleaned up, with an error
/// that says nothing about why.
fn transient_binary_warning(binary: &Path) -> Option<String> {
    let durable = DURABLE_BINARY_DIRECTORIES.iter().any(|dir| binary.starts_with(dir));
    if durable {
        return None;
    }
    Some(format!(
        "the unit will run {}, which is not a durable location. Install the \
         binary first:\n  sudo install -m 0755 {} {DEFAULT_BINARY_PATH}\n\
         then run this again, or pass --binary {DEFAULT_BINARY_PATH}.",
        binary.display(),
        binary.display()
    ))
}

/// Run `sentinel install <role>`.
///
/// Writes everything a host of this role needs: a complete configuration file
/// with every setting at its default, a hardened systemd unit, and -- on a
/// controller -- a generated cluster credential. Nothing that already exists is
/// touched without `--force`, so running it twice is safe.
pub fn run(role: &str, output_dir: &Path, config: &Path, options: Options<'_>) -> anyhow::Result<i32> {
    let parsed = crate::cli::generate::Role::parse(role)
        .ok_or_else(|| anyhow::anyhow!("unknown role {role:?}; expected \"controller\" or \"agent\""))?;
    let binary = match options.binary {
        Some(path) => path.to_path_buf(),
        None => std::env::current_exe().unwrap_or_else(|_| PathBuf::from(DEFAULT_BINARY_PATH)),
    };

    let (unit_name, unit_contents) = match parsed {
        crate::cli::generate::Role::Controller => ("sentinel-controller.service", controller_unit(&binary, config)),
        crate::cli::generate::Role::Agent => ("sentinel-agent.service", agent_unit(&binary, config)),
    };
    let unit_path = output_dir.join(unit_name);
    let config_contents = crate::cli::generate::config_file_for(
        parsed,
        crate::cli::generate::Detected::on_this_host(&crate::agent::system::LinuxInspector::new()),
    );

    // Before the dry-run return: a rehearsal is exactly when an operator wants
    // to be told the unit would name a path that will not survive.
    if let Some(warning) = transient_binary_warning(&binary) {
        eprintln!("warning: {warning}\n");
    }

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
    //
    // `--force` deliberately does not reach this file. Its purpose is "write
    // my files again", and someone upgrading runs it to pick up a new unit.
    // If it rotated the credential too, that upgrade would lock every agent in
    // the cluster out at once, from a flag that said nothing about
    // credentials. Rotating one is a separate act with cluster-wide
    // consequences: delete the file and run `install` again.
    let mut credential_generated = false;
    if parsed == crate::cli::generate::Role::Controller && options.credential {
        let token_file = token_path(config);
        let keep_existing = Options {
            force: false,
            ..options
        };
        let entry = if token_file.exists() {
            place(&token_file, "", Some(0o400), keep_existing)?
        } else {
            place(
                &token_file,
                &format!("{}\n", generate_credential()?),
                Some(0o400),
                keep_existing,
            )?
        };
        credential_generated = entry.outcome != Outcome::Kept;
        written.push(entry);
    }

    for entry in &written {
        println!("{:<40} {}", entry.path.display(), entry.outcome.label());
    }
    println!();
    let service_user_exists = {
        #[cfg(unix)]
        {
            service_user_ids().is_some()
        }
        #[cfg(not(unix))]
        {
            false
        }
    };
    print!(
        "{}",
        setup_instructions(role, &written, credential_generated, service_user_exists)
    );

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
    fn systemd_creates_the_state_directory() {
        // A host that has only ever had the binary copied onto it should not
        // need a mkdir before the service will start.
        assert!(controller().contains("StateDirectory=sentinel"));
    }

    #[test]
    fn installing_from_a_download_directory_warns() {
        // The likeliest first run: `./sentinel-x86_64-... install agent` in a
        // home directory. The unit would name a path that stops existing.
        let warning =
            transient_binary_warning(Path::new("/home/li/sentinel-x86_64-unknown-linux-musl")).expect("a warning");
        assert!(warning.contains(DEFAULT_BINARY_PATH), "{warning}");
        assert!(warning.contains("sudo install"), "{warning}");
    }

    #[test]
    fn an_installed_binary_does_not_warn() {
        for path in [
            "/usr/local/bin/sentinel",
            "/usr/bin/sentinel",
            "/opt/sentinel/bin/sentinel",
        ] {
            assert!(transient_binary_warning(Path::new(path)).is_none(), "{path}");
        }
    }

    #[test]
    fn the_unit_can_be_told_which_binary_to_run() {
        // So a release binary can be installed and the unit written in one
        // step, before the file is in its final place.
        let sandbox = Sandbox::new();
        let options = Options {
            binary: Some(Path::new("/usr/local/bin/sentinel")),
            ..Options::default()
        };
        sandbox.install("agent", options).expect("install");

        let unit = std::fs::read_to_string(sandbox.units().join("sentinel-agent.service")).expect("unit");
        assert!(unit.contains("ExecStart=/usr/local/bin/sentinel"), "{unit}");
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
    fn force_does_not_rotate_an_existing_credential() {
        // The upgrade footgun: someone re-runs install with --force to pick up
        // a new unit, and every agent in the cluster is locked out by a flag
        // that said nothing about credentials.
        let sandbox = Sandbox::new();
        sandbox.install("controller", Options::default()).expect("first");
        let token = std::fs::read_to_string(token_path(&sandbox.config())).expect("token");

        let options = Options {
            force: true,
            ..Options::default()
        };
        sandbox.install("controller", options).expect("second");

        assert_eq!(
            std::fs::read_to_string(token_path(&sandbox.config())).expect("token"),
            token,
            "--force rotated the cluster credential"
        );
    }

    #[test]
    fn force_still_replaces_the_unit_and_the_configuration() {
        // Which is what it is for: an upgrade picks up a unit with new
        // directives without the operator editing it by hand.
        let sandbox = Sandbox::new();
        sandbox.install("controller", Options::default()).expect("first");
        std::fs::write(sandbox.config(), "replaced by hand\n").expect("edit");

        let options = Options {
            force: true,
            ..Options::default()
        };
        sandbox.install("controller", options).expect("second");

        let config = std::fs::read_to_string(sandbox.config()).expect("config");
        assert!(config.contains("[controller]"), "{config}");
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
        let instructions = setup_instructions("controller", &written, true, false);
        assert!(instructions.contains("useradd --system"), "{instructions}");
        assert!(instructions.contains("config check"), "{instructions}");
        assert!(
            instructions.contains("全 host に同じ内容で"),
            "the credential must be described as cluster-wide: {instructions}"
        );
    }

    #[test]
    fn passwd_is_parsed_for_the_service_users_ids() {
        let passwd = "root:x:0:0:root:/root:/bin/bash\n\
                      sentinel:x:999:998::/nonexistent:/usr/sbin/nologin\n";
        assert_eq!(passwd_ids(passwd, SERVICE_USER), Some((999, 998)));
        assert_eq!(passwd_ids(passwd, "root"), Some((0, 0)));
        assert_eq!(passwd_ids(passwd, "nobody"), None, "a user that is not there");
        assert_eq!(passwd_ids("", SERVICE_USER), None);
        assert_eq!(passwd_ids("sentinel:x:notanumber:998::\n", SERVICE_USER), None);
    }

    #[test]
    fn a_second_role_beside_a_running_one_is_not_told_to_create_what_exists() {
        // Installing an agent on the controller host printed "create the
        // service user" as step one, which reads as skippable when the user is
        // already there -- and buried in that step was the chown the new files
        // needed. The service then failed to read its own configuration and
        // systemd reported a restart loop.
        let dir = tempfile::tempdir().expect("tempdir");
        let token = dir.path().join("token");
        std::fs::write(&token, "existing").expect("token");
        let written = vec![
            Written {
                path: dir.path().join("agent.toml"),
                outcome: Outcome::Created,
            },
            Written {
                path: token,
                outcome: Outcome::Kept,
            },
        ];

        let instructions = setup_instructions("agent", &written, false, true);

        assert!(!instructions.contains("useradd"), "{instructions}");
        assert!(
            !instructions.contains("install -o sentinel -g sentinel -m 0400"),
            "a credential that is already there must not be replaced: {instructions}"
        );
        assert!(instructions.contains("所有者も設定済み"), "{instructions}");
        // The steps that still matter are still there.
        assert!(instructions.contains("config check"), "{instructions}");
        assert!(
            instructions.contains("systemctl enable --now sentinel-agent"),
            "{instructions}"
        );
    }

    #[test]
    fn a_fresh_host_is_still_told_everything() {
        let dir = tempfile::tempdir().expect("tempdir");
        let written = vec![Written {
            path: dir.path().join("config.toml"),
            outcome: Outcome::Created,
        }];

        let instructions = setup_instructions("agent", &written, false, false);

        assert!(instructions.contains("useradd"), "{instructions}");
        assert!(instructions.contains("chown -R sentinel:sentinel"), "{instructions}");
        assert!(instructions.contains("systemd-journal"), "{instructions}");
        assert!(
            instructions.contains("install -o sentinel -g sentinel -m 0400"),
            "no credential here yet, so it has to be placed: {instructions}"
        );
    }
}
