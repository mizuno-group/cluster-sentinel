//! Value precedence and provenance (IMPLEMENTATION.md §58).
//!
//! Each setting resolves through a fixed chain of layers, and the winning
//! layer is remembered so an operator can ask *why* a value is what it is.

use std::fmt;

use serde::{Deserialize, Serialize};

/// Where a resolved value came from, ordered lowest to highest priority.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ValueSource {
    /// Compiled-in default.
    BuiltinDefault,
    /// Detected at runtime on this machine.
    RuntimeDiscovery,
    /// Read from the configuration file.
    ConfigFile,
    /// Read from an environment variable.
    EnvironmentVariable,
    /// Passed on the command line.
    CommandLine,
}

impl ValueSource {
    /// Human-readable label used by `sentinel config check`.
    pub fn label(&self) -> &'static str {
        match self {
            ValueSource::BuiltinDefault => "built-in default",
            ValueSource::RuntimeDiscovery => "runtime discovery",
            ValueSource::ConfigFile => "config file",
            ValueSource::EnvironmentVariable => "environment variable",
            ValueSource::CommandLine => "command line",
        }
    }
}

impl fmt::Display for ValueSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

/// A value together with the layer that supplied it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layered<T> {
    value: T,
    source: ValueSource,
}

impl<T> Layered<T> {
    /// Start from a built-in default.
    pub fn builtin(value: T) -> Self {
        Self {
            value,
            source: ValueSource::BuiltinDefault,
        }
    }

    /// Start from an explicit layer.
    pub fn from(value: T, source: ValueSource) -> Self {
        Self { value, source }
    }

    /// Offer a value from `source`; it wins only if that layer outranks the
    /// current one. Passing `None` leaves the value untouched, so an absent
    /// CLI flag never clobbers a configured setting.
    pub fn offer(&mut self, value: Option<T>, source: ValueSource) -> &mut Self {
        if let Some(value) = value {
            if source >= self.source {
                self.value = value;
                self.source = source;
            }
        }
        self
    }

    /// The resolved value.
    pub fn value(&self) -> &T {
        &self.value
    }

    /// The layer that supplied the resolved value.
    pub fn source(&self) -> ValueSource {
        self.source
    }

    /// Consume and return the resolved value.
    pub fn into_value(self) -> T {
        self.value
    }
}

impl<T: fmt::Display> fmt::Display for Layered<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} ({})", self.value, self.source)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn precedence_order_matches_the_specification() {
        assert!(ValueSource::CommandLine > ValueSource::EnvironmentVariable);
        assert!(ValueSource::EnvironmentVariable > ValueSource::ConfigFile);
        assert!(ValueSource::ConfigFile > ValueSource::RuntimeDiscovery);
        assert!(ValueSource::RuntimeDiscovery > ValueSource::BuiltinDefault);
    }

    #[test]
    fn a_higher_layer_wins_and_is_recorded() {
        let mut value = Layered::builtin("0.0.0.0:7443".to_string());
        value.offer(Some("192.0.2.1:7443".to_string()), ValueSource::ConfigFile);
        assert_eq!(value.value(), "192.0.2.1:7443");
        assert_eq!(value.source(), ValueSource::ConfigFile);

        value.offer(Some("127.0.0.1:9000".to_string()), ValueSource::CommandLine);
        assert_eq!(value.value(), "127.0.0.1:9000");
        assert_eq!(value.source(), ValueSource::CommandLine);
    }

    #[test]
    fn a_lower_layer_never_overrides_a_higher_one() {
        let mut value = Layered::from(9u32, ValueSource::CommandLine);
        value.offer(Some(1), ValueSource::ConfigFile);
        value.offer(Some(2), ValueSource::RuntimeDiscovery);
        assert_eq!(*value.value(), 9);
        assert_eq!(value.source(), ValueSource::CommandLine);
    }

    #[test]
    fn an_absent_value_leaves_the_setting_alone() {
        let mut value = Layered::from("configured".to_string(), ValueSource::ConfigFile);
        value.offer(None, ValueSource::CommandLine);
        assert_eq!(value.value(), "configured");
        assert_eq!(value.source(), ValueSource::ConfigFile);
    }

    #[test]
    fn the_same_layer_offering_again_wins_last_write() {
        let mut value = Layered::builtin(1u32);
        value.offer(Some(2), ValueSource::ConfigFile);
        value.offer(Some(3), ValueSource::ConfigFile);
        assert_eq!(*value.value(), 3);
    }

    #[test]
    fn display_shows_the_provenance() {
        let value = Layered::from(3u32, ValueSource::EnvironmentVariable);
        assert_eq!(value.to_string(), "3 (environment variable)");
    }
}
