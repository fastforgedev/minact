//! Build-matrix expansion.
//!
//! Turns a `strategy.matrix` block into the concrete list of job instances to
//! run, following GitHub's rules:
//!
//! 1. Take the cartesian product of the axes, in declaration order.
//! 2. Drop every combination matched by an `exclude` entry.
//! 3. Apply `include` entries — merged into the combinations they match, or
//!    appended as new combinations when they match none.
//!
//! `include` is deliberately processed *after* `exclude`, which is what lets a
//! workflow exclude a broad set and then add one specific case back.

use crate::types::Value;
use crate::workflow::MatrixConfig;

/// One expanded combination: the values of `${{ matrix.* }}` for one instance.
///
/// Insertion order follows axis declaration order, so the generated instance
/// names read the way the workflow was written.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MatrixCombination {
    entries: Vec<(String, Value)>,
}

impl MatrixCombination {
    pub fn new() -> Self {
        Self::default()
    }

    /// Look up one matrix value.
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.entries
            .iter()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value)
    }

    /// Insert or overwrite a value, keeping first-insertion position.
    pub fn insert(&mut self, key: String, value: Value) {
        match self.entries.iter_mut().find(|(name, _)| *name == key) {
            Some((_, existing)) => *existing = value,
            None => self.entries.push((key, value)),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = (&String, &Value)> {
        self.entries.iter().map(|(k, v)| (k, v))
    }

    /// The `(ubuntu-latest, 20)` suffix GitHub appends to a matrix job's name.
    pub fn display_suffix(&self) -> String {
        if self.entries.is_empty() {
            return String::new();
        }
        let values: Vec<String> = self.entries.iter().map(|(_, v)| v.to_string()).collect();
        format!(" ({})", values.join(", "))
    }
}

impl From<MatrixCombination> for std::collections::HashMap<String, Value> {
    fn from(combination: MatrixCombination) -> Self {
        combination.entries.into_iter().collect()
    }
}

/// Expand a matrix into the combinations to run.
///
/// Always returns at least one combination; a matrix that expands to nothing
/// yields a single empty combination so the job still runs once.
pub fn expand(config: &MatrixConfig) -> Vec<MatrixCombination> {
    let mut combinations = cartesian_product(config);
    combinations.retain(|combination| !is_excluded(combination, config));
    apply_includes(&mut combinations, config);

    if combinations.is_empty() {
        combinations.push(MatrixCombination::new());
    }
    combinations
}

/// The cartesian product of the axes, first axis varying slowest.
fn cartesian_product(config: &MatrixConfig) -> Vec<MatrixCombination> {
    let mut combinations = vec![MatrixCombination::new()];

    for axis in &config.axes {
        let mut expanded =
            Vec::with_capacity(combinations.len() * axis.values.values().len().max(1));
        for combination in &combinations {
            for value in axis.values.values() {
                let mut next = combination.clone();
                next.insert(axis.name.clone(), yaml_to_value(value));
                expanded.push(next);
            }
        }
        combinations = expanded;
    }

    combinations
}

/// An `exclude` entry matches when every key it names has an equal value.
fn is_excluded(combination: &MatrixCombination, config: &MatrixConfig) -> bool {
    config.exclude.values().iter().any(|entry| {
        entry.iter().all(|(key, value)| {
            let Some(key) = key.as_str() else {
                return false;
            };
            combination.get(key) == Some(&yaml_to_value(value))
        })
    })
}

/// Apply `include` entries.
///
/// An entry merges into every combination it is compatible with — that is,
/// every key it shares with the base axes already has the same value. If it is
/// compatible with none, it becomes a combination of its own.
fn apply_includes(combinations: &mut Vec<MatrixCombination>, config: &MatrixConfig) {
    let axis_names: Vec<&str> = config.axes.iter().map(|axis| axis.name.as_str()).collect();

    for entry in config.include.values() {
        let mut merged_anywhere = false;

        for combination in combinations.iter_mut() {
            // Only the keys that are real axes decide compatibility; keys the
            // matrix does not declare are additions, not constraints.
            let compatible = entry.iter().all(|(key, value)| {
                let Some(key) = key.as_str() else {
                    return false;
                };
                if !axis_names.contains(&key) {
                    return true;
                }
                combination.get(key) == Some(&yaml_to_value(value))
            });

            if compatible {
                merged_anywhere = true;
                for (key, value) in entry {
                    if let Some(key) = key.as_str() {
                        combination.insert(key.to_string(), yaml_to_value(value));
                    }
                }
            }
        }

        if !merged_anywhere {
            let mut combination = MatrixCombination::new();
            for (key, value) in entry {
                if let Some(key) = key.as_str() {
                    combination.insert(key.to_string(), yaml_to_value(value));
                }
            }
            combinations.push(combination);
        }
    }
}

/// Convert a YAML value into the runtime value used by expressions.
pub fn yaml_to_value(value: &serde_yaml::Value) -> Value {
    match value {
        serde_yaml::Value::Null => Value::Null,
        serde_yaml::Value::Bool(b) => Value::Bool(*b),
        serde_yaml::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Value::Int(i)
            } else if let Some(f) = n.as_f64() {
                Value::Float(f)
            } else {
                Value::String(n.to_string())
            }
        }
        serde_yaml::Value::String(s) => Value::String(s.clone()),
        serde_yaml::Value::Sequence(seq) => Value::Array(seq.iter().map(yaml_to_value).collect()),
        serde_yaml::Value::Mapping(map) => Value::Map(
            map.iter()
                .filter_map(|(k, v)| k.as_str().map(|k| (k.to_string(), yaml_to_value(v))))
                .collect(),
        ),
        serde_yaml::Value::Tagged(tagged) => yaml_to_value(&tagged.value),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workflow::MatrixConfig;

    fn matrix(yaml: &str) -> MatrixConfig {
        serde_yaml::from_str(yaml).expect("matrix should parse")
    }

    /// Render combinations as `key=value;key=value` for compact assertions.
    fn rendered(combinations: &[MatrixCombination]) -> Vec<String> {
        combinations
            .iter()
            .map(|c| {
                c.iter()
                    .map(|(k, v)| format!("{}={}", k, v))
                    .collect::<Vec<_>>()
                    .join(";")
            })
            .collect()
    }

    #[test]
    fn expands_a_single_axis() {
        let config = matrix("os: [linux, macos]");
        assert_eq!(rendered(&expand(&config)), vec!["os=linux", "os=macos"]);
    }

    #[test]
    fn expands_the_cartesian_product_in_declaration_order() {
        let config = matrix("os: [linux, macos]\nnode: [18, 20]");
        assert_eq!(
            rendered(&expand(&config)),
            vec![
                "os=linux;node=18",
                "os=linux;node=20",
                "os=macos;node=18",
                "os=macos;node=20",
            ]
        );
    }

    #[test]
    fn excludes_matching_combinations() {
        let config =
            matrix("os: [linux, macos]\nnode: [18, 20]\nexclude:\n  - os: macos\n    node: 18\n");
        assert_eq!(
            rendered(&expand(&config)),
            vec!["os=linux;node=18", "os=linux;node=20", "os=macos;node=20"]
        );
    }

    #[test]
    fn exclude_matches_partially() {
        // Naming only one axis drops every combination on that axis.
        let config = matrix("os: [linux, macos]\nnode: [18, 20]\nexclude:\n  - os: macos\n");
        assert_eq!(
            rendered(&expand(&config)),
            vec!["os=linux;node=18", "os=linux;node=20"]
        );
    }

    #[test]
    fn include_adds_values_to_matching_combinations() {
        let config =
            matrix("os: [linux, macos]\ninclude:\n  - os: macos\n    experimental: true\n");
        assert_eq!(
            rendered(&expand(&config)),
            vec!["os=linux", "os=macos;experimental=true"]
        );
    }

    #[test]
    fn include_appends_when_nothing_matches() {
        let config = matrix("os: [linux, macos]\ninclude:\n  - os: windows\n");
        assert_eq!(
            rendered(&expand(&config)),
            vec!["os=linux", "os=macos", "os=windows"]
        );
    }

    #[test]
    fn include_with_only_new_keys_applies_to_every_combination() {
        let config = matrix("os: [linux, macos]\ninclude:\n  - shared: yes\n");
        assert_eq!(
            rendered(&expand(&config)),
            vec!["os=linux;shared=yes", "os=macos;shared=yes"]
        );
    }

    #[test]
    fn include_runs_after_exclude_so_a_case_can_be_added_back() {
        let config = matrix(
            "os: [linux, macos]\nnode: [18, 20]\n\
             exclude:\n  - os: macos\n\
             include:\n  - os: macos\n    node: 22\n",
        );
        assert_eq!(
            rendered(&expand(&config)),
            vec!["os=linux;node=18", "os=linux;node=20", "os=macos;node=22"]
        );
    }

    #[test]
    fn an_empty_matrix_still_runs_once() {
        let config = MatrixConfig::default();
        let combinations = expand(&config);
        assert_eq!(combinations.len(), 1);
        assert!(combinations[0].is_empty());
    }

    #[test]
    fn a_fully_excluded_matrix_still_runs_once() {
        let config = matrix("os: [linux]\nexclude:\n  - os: linux\n");
        let combinations = expand(&config);
        assert_eq!(combinations.len(), 1);
        assert!(combinations[0].is_empty());
    }

    #[test]
    fn display_suffix_matches_github_naming() {
        let config = matrix("os: [ubuntu-latest]\nnode: [20]");
        assert_eq!(expand(&config)[0].display_suffix(), " (ubuntu-latest, 20)");
    }

    #[test]
    fn keeps_structured_values() {
        let config = matrix("target:\n  - name: apk\n    args: [--split]\n");
        let combinations = expand(&config);
        let target = combinations[0].get("target").unwrap();
        match target {
            Value::Map(map) => {
                assert_eq!(map.get("name"), Some(&Value::String("apk".to_string())));
                assert_eq!(
                    map.get("args"),
                    Some(&Value::Array(vec![Value::String("--split".to_string())]))
                );
            }
            other => panic!("expected a map, got {:?}", other),
        }
    }

    /// Structured values are backed by an unordered map, so their rendering
    /// has to be sorted — otherwise the instance id of a job would change
    /// between runs.
    #[test]
    fn structured_values_render_deterministically() {
        let config = matrix("target:\n  - platform: android\n    format: apk\n");
        let first = expand(&config)[0].display_suffix();
        for _ in 0..50 {
            assert_eq!(expand(&config)[0].display_suffix(), first);
        }
        assert_eq!(first, " ({format: apk, platform: android})");
    }

    #[test]
    fn numbers_stay_numbers() {
        let config = matrix("node: [18, 20.5]");
        let combinations = expand(&config);
        assert_eq!(combinations[0].get("node"), Some(&Value::Int(18)));
        assert_eq!(combinations[1].get("node"), Some(&Value::Float(20.5)));
    }
}
