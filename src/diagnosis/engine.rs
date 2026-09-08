//! The diagnosis engine.
//!
//! Rules are typed Rust, evaluated in a fixed order, over data that is already
//! stored. The same inputs always produce the same diagnoses, and every
//! diagnosis names the rule that produced it and the observations it rests on
//! (SPEC.md §94, IMPLEMENTATION.md §71, §72).
//!
//! There is no rule DSL and no language model in this path. An operator woken
//! at 3am is entitled to ask "why do you think that", and the answer has to be
//! something they can check.

use super::{Diagnosis, DiagnosisContext, RuleId};

/// One rule.
///
/// A rule looks at the context and returns the diagnoses it is prepared to
/// support. Returning nothing is the normal case and is not a failure.
pub trait DiagnosisRule: Send + Sync {
    /// Identifier, recorded on every diagnosis this rule produces.
    fn id(&self) -> RuleId;

    /// What this rule is for, shown by `sentinel doctor`.
    fn description(&self) -> &str;

    /// Evaluate against the current picture.
    fn evaluate(&self, context: &DiagnosisContext) -> Vec<Diagnosis>;
}

/// Evaluates every registered rule.
#[derive(Default)]
pub struct DiagnosisEngine {
    rules: Vec<Box<dyn DiagnosisRule>>,
}

impl DiagnosisEngine {
    /// An engine with no rules.
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a rule.
    pub fn register(&mut self, rule: Box<dyn DiagnosisRule>) -> &mut Self {
        self.rules.push(rule);
        self
    }

    /// The registered rules' ids.
    pub fn rule_ids(&self) -> Vec<RuleId> {
        self.rules.iter().map(|r| r.id()).collect()
    }

    /// How many rules are registered.
    pub fn len(&self) -> usize {
        self.rules.len()
    }

    /// Whether no rule is registered.
    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    /// Run every rule.
    ///
    /// A rule that panics is reported and skipped rather than taking the
    /// diagnosis pass down: one broken rule must not cost the operator every
    /// other conclusion (SPEC.md §119).
    pub fn diagnose(&self, context: &DiagnosisContext) -> Vec<Diagnosis> {
        let mut diagnoses = Vec::new();

        for rule in &self.rules {
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| rule.evaluate(context)));
            match outcome {
                Ok(produced) => diagnoses.extend(produced),
                Err(_) => {
                    tracing::error!(rule = %rule.id(), "diagnosis rule panicked; skipping it");
                }
            }
        }

        // Most severe first, so the most useful conclusion is the one an
        // operator reads first.
        diagnoses.sort_by(|a, b| {
            b.confidence
                .cmp(&a.confidence)
                .then(a.diagnosis_type.cmp(&b.diagnosis_type))
        });
        diagnoses
    }
}

/// The rules that ship with Sentinel.
pub fn builtin_rules() -> DiagnosisEngine {
    let mut engine = DiagnosisEngine::new();
    for rule in super::rules::builtin() {
        engine.register(rule);
    }
    engine
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diagnosis::{Confidence, ObservationIndex};
    use crate::inventory::Inventory;
    use std::collections::HashMap;

    struct Fixed {
        id: RuleId,
        diagnoses: Vec<Diagnosis>,
    }

    impl DiagnosisRule for Fixed {
        fn id(&self) -> RuleId {
            self.id.clone()
        }
        fn description(&self) -> &str {
            "a fixed rule, for tests"
        }
        fn evaluate(&self, _: &DiagnosisContext) -> Vec<Diagnosis> {
            self.diagnoses.clone()
        }
    }

    struct Exploding;

    impl DiagnosisRule for Exploding {
        fn id(&self) -> RuleId {
            RuleId::new("test.exploding")
        }
        fn description(&self) -> &str {
            "a rule that panics"
        }
        fn evaluate(&self, _: &DiagnosisContext) -> Vec<Diagnosis> {
            panic!("this rule is broken");
        }
    }

    fn with_context<T>(body: impl FnOnce(&DiagnosisContext) -> T) -> T {
        let inventory = Inventory::new();
        let states = HashMap::new();
        let observations = ObservationIndex::new();
        let context = DiagnosisContext {
            environment: "lab",
            inventory: &inventory,
            states: &states,
            observations: &observations,
        };
        body(&context)
    }

    #[test]
    fn an_engine_with_no_rules_concludes_nothing() {
        let engine = DiagnosisEngine::new();
        assert!(engine.is_empty());
        assert!(with_context(|context| engine.diagnose(context)).is_empty());
    }

    #[test]
    fn every_registered_rule_is_evaluated() {
        let mut engine = DiagnosisEngine::new();
        engine.register(Box::new(Fixed {
            id: RuleId::new("test.a"),
            diagnoses: vec![Diagnosis::new("A", "test.a", Confidence::Medium)],
        }));
        engine.register(Box::new(Fixed {
            id: RuleId::new("test.b"),
            diagnoses: vec![Diagnosis::new("B", "test.b", Confidence::Medium)],
        }));

        assert_eq!(with_context(|context| engine.diagnose(context)).len(), 2);
    }

    #[test]
    fn a_panicking_rule_does_not_cost_the_others() {
        // One broken rule must not deny an operator every other conclusion.
        let mut engine = DiagnosisEngine::new();
        engine.register(Box::new(Exploding));
        engine.register(Box::new(Fixed {
            id: RuleId::new("test.good"),
            diagnoses: vec![Diagnosis::new("GOOD", "test.good", Confidence::High)],
        }));

        let diagnoses = with_context(|context| engine.diagnose(context));
        assert_eq!(diagnoses.len(), 1);
        assert!(diagnoses[0].is("GOOD"));
    }

    #[test]
    fn diagnoses_come_back_most_confident_first() {
        let mut engine = DiagnosisEngine::new();
        engine.register(Box::new(Fixed {
            id: RuleId::new("test.weak"),
            diagnoses: vec![Diagnosis::new("WEAK", "test.weak", Confidence::Low)],
        }));
        engine.register(Box::new(Fixed {
            id: RuleId::new("test.strong"),
            diagnoses: vec![Diagnosis::new("STRONG", "test.strong", Confidence::High)],
        }));

        let diagnoses = with_context(|context| engine.diagnose(context));
        assert!(diagnoses[0].is("STRONG"));
        assert!(diagnoses[1].is("WEAK"));
    }

    #[test]
    fn evaluation_is_deterministic() {
        // The same inputs must always give the same answer: an explanation that
        // changes between runs cannot be checked.
        let engine = builtin_rules();
        let first = with_context(|context| engine.diagnose(context));
        let second = with_context(|context| engine.diagnose(context));

        let types = |d: &[Diagnosis]| d.iter().map(|d| d.diagnosis_type.to_string()).collect::<Vec<_>>();
        assert_eq!(types(&first), types(&second));
    }

    #[test]
    fn the_builtin_engine_has_rules_and_they_are_uniquely_identified() {
        let engine = builtin_rules();
        assert!(!engine.is_empty());

        let ids = engine.rule_ids();
        let unique: std::collections::BTreeSet<_> = ids.iter().collect();
        assert_eq!(ids.len(), unique.len(), "two rules share an id: {ids:?}");
    }

    #[test]
    fn a_healthy_cluster_produces_no_diagnoses() {
        // Silence is the correct output when nothing is wrong.
        assert!(with_context(|context| builtin_rules().diagnose(context)).is_empty());
    }
}
