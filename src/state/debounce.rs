//! Debounce / hysteresis (SPEC.md §90, IMPLEMENTATION.md §70).
//!
//! Defaults: two consecutive bad observations to warn, three to declare
//! unavailable, two consecutive good ones to recover. Per-probe overrides are
//! expected, so the policy is a value, not a constant.
//!
//! An explicitly reported state — a scheduler saying a node is DRAIN — is a
//! fact rather than a flaky measurement and is allowed to take effect
//! immediately via [`Debouncer::force`].

use serde::{Deserialize, Serialize};

use super::Health;

/// How many consecutive results are needed to change health.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct DebouncePolicy {
    /// Bad observations before reporting [`Health::Degraded`].
    pub warning_threshold: u32,
    /// Bad observations before reporting [`Health::Unavailable`].
    pub critical_threshold: u32,
    /// Good observations before returning to [`Health::Healthy`].
    pub recovery_threshold: u32,
}

impl Default for DebouncePolicy {
    fn default() -> Self {
        Self {
            warning_threshold: 2,
            critical_threshold: 3,
            recovery_threshold: 2,
        }
    }
}

impl DebouncePolicy {
    /// A policy that reacts to every single observation, for probes whose
    /// result is an authoritative statement rather than a measurement.
    pub fn immediate() -> Self {
        Self {
            warning_threshold: 1,
            critical_threshold: 1,
            recovery_threshold: 1,
        }
    }
}

/// Tracks consecutive outcomes for one component and derives its health.
#[derive(Debug, Clone, PartialEq)]
pub struct Debouncer {
    policy: DebouncePolicy,
    health: Health,
    consecutive_failures: u32,
    consecutive_successes: u32,
}

impl Debouncer {
    /// A debouncer starting from [`Health::Unknown`].
    pub fn new(policy: DebouncePolicy) -> Self {
        Self {
            policy,
            health: Health::Unknown,
            consecutive_failures: 0,
            consecutive_successes: 0,
        }
    }

    /// A debouncer starting from a known health.
    pub fn starting_at(policy: DebouncePolicy, health: Health) -> Self {
        Self {
            policy,
            health,
            consecutive_failures: 0,
            consecutive_successes: 0,
        }
    }

    /// Current health.
    pub fn health(&self) -> Health {
        self.health
    }

    /// Consecutive bad observations so far.
    pub fn consecutive_failures(&self) -> u32 {
        self.consecutive_failures
    }

    /// Consecutive good observations so far.
    pub fn consecutive_successes(&self) -> u32 {
        self.consecutive_successes
    }

    /// Feed one outcome. Returns `Some(previous)` if health changed.
    pub fn observe(&mut self, bad: bool) -> Option<Health> {
        if bad {
            self.consecutive_successes = 0;
            self.consecutive_failures = self.consecutive_failures.saturating_add(1);
        } else {
            self.consecutive_failures = 0;
            self.consecutive_successes = self.consecutive_successes.saturating_add(1);
        }

        let next = if self.consecutive_failures >= self.policy.critical_threshold {
            Health::Unavailable
        } else if self.consecutive_failures >= self.policy.warning_threshold {
            Health::Degraded
        } else if self.consecutive_successes >= self.policy.recovery_threshold {
            Health::Healthy
        } else {
            // Not enough evidence yet: hold the previous verdict rather than
            // flapping on a single sample.
            self.health
        };

        self.transition_to(next)
    }

    /// Set health directly, bypassing the thresholds. Used for authoritative
    /// statements such as a scheduler-reported DRAIN.
    pub fn force(&mut self, health: Health) -> Option<Health> {
        self.consecutive_failures = 0;
        self.consecutive_successes = 0;
        self.transition_to(health)
    }

    fn transition_to(&mut self, next: Health) -> Option<Health> {
        if next == self.health {
            return None;
        }
        let previous = self.health;
        self.health = next;
        Some(previous)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_single_failure_does_not_change_health() {
        let mut debouncer = Debouncer::starting_at(DebouncePolicy::default(), Health::Healthy);
        assert_eq!(debouncer.observe(true), None);
        assert_eq!(
            debouncer.health(),
            Health::Healthy,
            "one dropped packet is not an outage"
        );
    }

    #[test]
    fn two_failures_warn_and_three_go_unavailable() {
        let mut debouncer = Debouncer::starting_at(DebouncePolicy::default(), Health::Healthy);
        debouncer.observe(true);
        assert_eq!(debouncer.observe(true), Some(Health::Healthy));
        assert_eq!(debouncer.health(), Health::Degraded);
        assert_eq!(debouncer.observe(true), Some(Health::Degraded));
        assert_eq!(debouncer.health(), Health::Unavailable);
    }

    #[test]
    fn recovery_needs_two_successes() {
        let mut debouncer = Debouncer::starting_at(DebouncePolicy::default(), Health::Unavailable);
        assert_eq!(debouncer.observe(false), None, "one success is not a recovery");
        assert_eq!(debouncer.health(), Health::Unavailable);
        assert_eq!(debouncer.observe(false), Some(Health::Unavailable));
        assert_eq!(debouncer.health(), Health::Healthy);
    }

    #[test]
    fn an_intervening_success_resets_the_failure_run() {
        let mut debouncer = Debouncer::starting_at(DebouncePolicy::default(), Health::Healthy);
        debouncer.observe(true);
        debouncer.observe(false);
        debouncer.observe(true);
        assert_eq!(debouncer.health(), Health::Healthy);
        assert_eq!(debouncer.consecutive_failures(), 1);
    }

    #[test]
    fn an_immediate_policy_reacts_to_the_first_observation() {
        let mut debouncer = Debouncer::starting_at(DebouncePolicy::immediate(), Health::Healthy);
        assert_eq!(debouncer.observe(true), Some(Health::Healthy));
        assert_eq!(debouncer.health(), Health::Unavailable);
    }

    #[test]
    fn force_sets_health_without_thresholds_and_reports_the_change() {
        let mut debouncer = Debouncer::starting_at(DebouncePolicy::default(), Health::Healthy);
        assert_eq!(debouncer.force(Health::Degraded), Some(Health::Healthy));
        assert_eq!(debouncer.health(), Health::Degraded);
        assert_eq!(debouncer.force(Health::Degraded), None, "no change, no transition");
    }

    #[test]
    fn counters_do_not_overflow() {
        let mut debouncer = Debouncer::new(DebouncePolicy::default());
        for _ in 0..1000 {
            debouncer.observe(true);
        }
        assert_eq!(debouncer.health(), Health::Unavailable);
        assert_eq!(debouncer.consecutive_failures(), 1000);
    }
}
