//! `sentinel version`.

use serde::Serialize;

use crate::{CONFIG_VERSION, PROTOCOL_VERSION, VERSION};

/// Version information. Binary and protocol versions are reported separately
/// because they evolve independently (IMPLEMENTATION.md §62).
#[derive(Debug, Serialize)]
struct VersionInfo {
    version: &'static str,
    protocol_version: u32,
    config_version: u32,
    target: &'static str,
}

fn info() -> VersionInfo {
    VersionInfo {
        version: VERSION,
        protocol_version: PROTOCOL_VERSION,
        config_version: CONFIG_VERSION,
        target: env!("SENTINEL_TARGET"),
    }
}

/// Run the command.
pub fn run(json: bool) -> anyhow::Result<i32> {
    let info = info();
    if json {
        println!("{}", serde_json::to_string_pretty(&info)?);
    } else {
        println!("sentinel {}", info.version);
        println!("protocol version: {}", info.protocol_version);
        println!("config version:   {}", info.config_version);
        println!("target:           {}", info.target);
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_information_is_populated() {
        let info = info();
        assert!(!info.version.is_empty());
        assert_eq!(info.protocol_version, PROTOCOL_VERSION);
        assert!(!info.target.is_empty());
    }

    #[test]
    fn the_command_succeeds_in_both_formats() {
        assert_eq!(run(false).expect("text"), 0);
        assert_eq!(run(true).expect("json"), 0);
    }
}
