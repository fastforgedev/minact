//! Workflow model types representing the YAML workflow schema.
//!
//! Mirrors the GitHub Actions workflow YAML structure:
//! - `name`, `on` (triggers), `env`, `jobs`, `defaults`

use serde::{Deserialize, Serialize};

use crate::types::WorkflowError;
use std::collections::HashMap;

/// Render a scalar YAML value the way GitHub Actions does when it coerces
/// `env:` / `with:` values to strings.
///
/// Workflows in the wild write `fetch-depth: 0` and `env: { RETRIES: 3 }`;
/// deserializing those straight into `String` fails, so coerce scalars here.
fn scalar_to_string(value: &serde_yaml::Value) -> Option<String> {
    match value {
        serde_yaml::Value::String(s) => Some(s.clone()),
        serde_yaml::Value::Bool(b) => Some(b.to_string()),
        serde_yaml::Value::Number(n) => Some(n.to_string()),
        serde_yaml::Value::Null => Some(String::new()),
        // Sequences/mappings have no scalar form; `with:` values that are
        // structured are serialized back to YAML so the action still sees them.
        other => serde_yaml::to_string(other)
            .ok()
            .map(|s| s.trim_end().to_string()),
    }
}

/// Deserialize a `HashMap<String, String>` that tolerates non-string scalars.
fn de_string_map<'de, D>(deserializer: D) -> Result<HashMap<String, String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::Error;

    let value = serde_yaml::Value::deserialize(deserializer)?;
    match value {
        serde_yaml::Value::Null => Ok(HashMap::new()),
        serde_yaml::Value::Mapping(map) => {
            let mut result = HashMap::new();
            for (key, val) in map {
                let key = match &key {
                    serde_yaml::Value::String(s) => s.clone(),
                    other => scalar_to_string(other).ok_or_else(|| {
                        D::Error::custom(format!("unsupported mapping key: {:?}", other))
                    })?,
                };
                let val = scalar_to_string(&val).ok_or_else(|| {
                    D::Error::custom(format!("unsupported value for key '{}'", key))
                })?;
                result.insert(key, val);
            }
            Ok(result)
        }
        other => Err(D::Error::custom(format!(
            "expected a mapping, got {:?}",
            other
        ))),
    }
}

/// Deserialize `runs-on:`, which GitHub accepts in three shapes:
/// `runs-on: ubuntu-latest`, `runs-on: [self-hosted, linux]`, and
/// `runs-on: { group: g, labels: [...] }`.
///
/// All of them reduce to the single label a runner mapping is keyed by.
fn de_runs_on<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = serde_yaml::Value::deserialize(deserializer)?;
    Ok(match value {
        serde_yaml::Value::Null => None,
        serde_yaml::Value::String(s) => Some(s),
        serde_yaml::Value::Sequence(seq) => {
            seq.first().and_then(|v| v.as_str()).map(|s| s.to_string())
        }
        serde_yaml::Value::Mapping(map) => map
            .get(serde_yaml::Value::String("labels".to_string()))
            .and_then(|labels| match labels {
                serde_yaml::Value::String(s) => Some(s.clone()),
                serde_yaml::Value::Sequence(seq) => {
                    seq.first().and_then(|v| v.as_str()).map(|s| s.to_string())
                }
                _ => None,
            })
            .or_else(|| {
                map.get(serde_yaml::Value::String("group".to_string()))
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
            }),
        // A non-string scalar is not a usable label; ignore rather than fail.
        _ => None,
    })
}

/// Deserialize `needs:` which GitHub accepts as either a string or a sequence.
fn de_needs<'de, D>(deserializer: D) -> Result<Option<Vec<String>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::Error;

    let value = serde_yaml::Value::deserialize(deserializer)?;
    match value {
        serde_yaml::Value::Null => Ok(None),
        // `needs: build`
        serde_yaml::Value::String(s) => Ok(Some(vec![s])),
        // `needs: [build, test]`
        serde_yaml::Value::Sequence(seq) => {
            let mut needs = Vec::with_capacity(seq.len());
            for item in seq {
                match item {
                    serde_yaml::Value::String(s) => needs.push(s),
                    other => {
                        return Err(D::Error::custom(format!(
                            "expected a job id string in `needs`, got {:?}",
                            other
                        )))
                    }
                }
            }
            Ok(Some(needs))
        }
        other => Err(D::Error::custom(format!(
            "expected a string or sequence for `needs`, got {:?}",
            other
        ))),
    }
}

/// Top-level workflow definition.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Workflow {
    /// Human-readable workflow name.
    #[serde(default)]
    pub name: String,

    /// Event triggers that activate this workflow.
    #[serde(default)]
    pub on: OnConfig,

    /// Global environment variables available to all jobs.
    #[serde(default, deserialize_with = "de_string_map")]
    pub env: HashMap<String, String>,

    /// Default settings applied to all jobs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub defaults: Option<Defaults>,

    /// The set of jobs in this workflow.
    pub jobs: HashMap<String, Job>,

    /// The file path this workflow was loaded from (not serialized).
    #[serde(skip)]
    pub file_path: Option<std::path::PathBuf>,
}

/// Event trigger configuration.
///
/// Supports three YAML forms:
/// - String: `on: push`
/// - Sequence: `on: [push, pull_request]`
/// - Map: `on: { push: { branches: [main] } }`
#[derive(Debug, Clone, Default, Serialize)]
pub struct OnConfig {
    /// Push event trigger.
    pub push: Option<PushConfig>,

    /// Pull request event trigger.
    pub pull_request: Option<BranchFilter>,

    /// Release event trigger.
    pub release: Option<ReleaseConfig>,

    /// Manual workflow dispatch trigger.
    pub workflow_dispatch: Option<WorkflowDispatchConfig>,

    /// Scheduled trigger via cron.
    pub schedule: Option<Vec<ScheduleConfig>>,

    /// Additional event triggers stored as raw map for extensibility.
    pub extra: HashMap<String, serde_yaml::Value>,
}

impl<'de> Deserialize<'de> for OnConfig {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::Error;

        // First deserialize as a generic YAML value
        let value = serde_yaml::Value::deserialize(deserializer)?;

        match value {
            // Map form: `on: { push: { branches: [main] }, workflow_dispatch: ~ }`
            serde_yaml::Value::Mapping(map) => {
                let mut config = OnConfig::default();

                for (key, val) in map {
                    let key_str = key.as_str().unwrap_or("");
                    match key_str {
                        "push" => {
                            if val.is_mapping() || val.is_null() {
                                config.push = serde_yaml::from_value(val).ok();
                            }
                        }
                        "pull_request" => {
                            if val.is_mapping() || val.is_null() {
                                config.pull_request = serde_yaml::from_value(val).ok();
                            }
                        }
                        "release" => {
                            config.release = serde_yaml::from_value(val).ok();
                        }
                        "workflow_dispatch" => {
                            if val.is_mapping() || val.is_null() {
                                config.workflow_dispatch = serde_yaml::from_value(val).ok();
                            }
                        }
                        "schedule" => {
                            config.schedule = serde_yaml::from_value(val).ok();
                        }
                        _ => {
                            config.extra.insert(key_str.to_string(), val);
                        }
                    }
                }

                Ok(config)
            }
            // String form: `on: push`
            serde_yaml::Value::String(event) => {
                let mut config = OnConfig::default();
                match event.as_str() {
                    "push" => config.push = Some(PushConfig::default()),
                    "pull_request" => config.pull_request = Some(BranchFilter::default()),
                    "workflow_dispatch" => {
                        config.workflow_dispatch = Some(WorkflowDispatchConfig::default())
                    }
                    other => {
                        config
                            .extra
                            .insert(other.to_string(), serde_yaml::Value::Null);
                    }
                }
                Ok(config)
            }
            // Sequence form: `on: [push, pull_request]`
            serde_yaml::Value::Sequence(seq) => {
                let mut config = OnConfig::default();
                for item in seq {
                    if let Some(event) = item.as_str() {
                        match event {
                            "push" => config.push = Some(PushConfig::default()),
                            "pull_request" => config.pull_request = Some(BranchFilter::default()),
                            "workflow_dispatch" => {
                                config.workflow_dispatch = Some(WorkflowDispatchConfig::default());
                            }
                            other => {
                                config
                                    .extra
                                    .insert(other.to_string(), serde_yaml::Value::Null);
                            }
                        }
                    }
                }
                Ok(config)
            }
            _ => Err(Error::custom(format!(
                "expected a string, sequence, or map for `on`, got {:?}",
                value
            ))),
        }
    }
}

/// Configuration for push events.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PushConfig {
    /// Branch patterns to include.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branches: Option<Vec<String>>,

    /// Tag patterns to include.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tags: Option<Vec<String>>,
}

/// Simple branch filter (for pull_request).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct BranchFilter {
    /// Branch patterns to include.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branches: Option<Vec<String>>,
}

/// Configuration for release events.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ReleaseConfig {
    /// Release event types (published, created, etc.)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub types: Option<Vec<String>>,
}

/// Configuration for workflow_dispatch (manual triggers).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct WorkflowDispatchConfig {
    /// Input parameters for manual dispatch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inputs: Option<HashMap<String, DispatchInput>>,
}

/// An input parameter for workflow_dispatch.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DispatchInput {
    /// Human-readable description.
    #[serde(default)]
    pub description: String,

    /// Whether the input is required.
    #[serde(default)]
    pub required: bool,

    /// Default value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<String>,

    /// Input type (string, boolean, choice, etc.)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[serde(rename = "type")]
    pub input_type: Option<String>,
}

/// Configuration for scheduled triggers.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScheduleConfig {
    /// Cron expression.
    pub cron: String,
}

/// Default settings applied to jobs.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Defaults {
    /// Default run configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run: Option<RunDefaults>,
}

/// Default settings for shell steps.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct RunDefaults {
    /// Default shell to use.
    #[serde(default)]
    pub shell: String,

    /// Default working directory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[serde(rename = "working-directory")]
    pub working_directory: Option<String>,
}

/// A single job definition.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Job {
    /// Human-readable job name.
    #[serde(default)]
    pub name: String,

    /// List of job IDs that must complete before this one.
    ///
    /// Accepts both `needs: build` and `needs: [build, test]`.
    #[serde(
        default,
        deserialize_with = "de_needs",
        skip_serializing_if = "Option::is_none"
    )]
    pub needs: Option<Vec<String>>,

    /// Condition expression to determine if this job should run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[serde(rename = "if")]
    pub if_condition: Option<String>,

    /// Job-level environment variables.
    #[serde(default, deserialize_with = "de_string_map")]
    pub env: HashMap<String, String>,

    /// Job-level default settings.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub defaults: Option<Defaults>,

    /// The steps in this job.
    #[serde(default)]
    pub steps: Vec<Step>,

    /// Outputs from this job.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outputs: Option<HashMap<String, String>>,

    /// The runner label, mapped to a real place to run by `.minact/config.yml`.
    ///
    /// GitHub accepts a bare label, a list of labels, or a `group`/`labels`
    /// mapping; all three collapse to the first label here.
    #[serde(
        default,
        rename = "runs-on",
        deserialize_with = "de_runs_on",
        skip_serializing_if = "Option::is_none"
    )]
    pub runs_on: Option<String>,

    /// Timeout in minutes for the whole job.
    ///
    /// The rename matters: without it `timeout-minutes:` as GitHub spells it
    /// never reaches this field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[serde(rename = "timeout-minutes")]
    pub timeout_minutes: Option<f64>,

    /// Keep the workflow passing when this job fails, and let jobs that
    /// `need` it run anyway.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[serde(rename = "continue-on-error")]
    pub continue_on_error: Option<bool>,

    /// Strategy (matrix, etc.)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub strategy: Option<Strategy>,

    /// Run every step of this job inside a container.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub container: Option<JobContainer>,

    /// Service containers the job expects alongside it.
    ///
    /// Kept so the engine can say it is not starting them, rather than
    /// ignoring a database the workflow depends on.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub services: HashMap<String, JobContainer>,
}

/// A container a job runs in, or a service alongside it.
///
/// GitHub accepts either a bare image name or a mapping, and both appear in
/// real workflows.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(from = "ContainerSpecYaml")]
pub struct JobContainer {
    pub image: String,
    #[serde(default, deserialize_with = "de_string_map")]
    pub env: HashMap<String, String>,
    #[serde(default)]
    pub ports: Vec<String>,
    #[serde(default)]
    pub volumes: Vec<String>,
    /// Extra `docker run` arguments, as one string.
    #[serde(default)]
    pub options: Option<String>,
    /// Registry credentials. Recorded so their presence can be reported;
    /// minact does not log in to registries on the user's behalf.
    #[serde(default)]
    pub credentials: Option<serde_yaml::Value>,
}

/// The two YAML shapes a container can take.
#[derive(Deserialize)]
#[serde(untagged)]
enum ContainerSpecYaml {
    /// `container: node:20`
    Image(String),
    /// `container: { image: node:20, ... }`
    Full {
        image: String,
        #[serde(default, deserialize_with = "de_string_map")]
        env: HashMap<String, String>,
        #[serde(default)]
        ports: Vec<String>,
        #[serde(default)]
        volumes: Vec<String>,
        #[serde(default)]
        options: Option<String>,
        #[serde(default)]
        credentials: Option<serde_yaml::Value>,
    },
}

impl From<ContainerSpecYaml> for JobContainer {
    fn from(spec: ContainerSpecYaml) -> Self {
        match spec {
            ContainerSpecYaml::Image(image) => Self {
                image,
                env: HashMap::new(),
                ports: Vec::new(),
                volumes: Vec::new(),
                options: None,
                credentials: None,
            },
            ContainerSpecYaml::Full {
                image,
                env,
                ports,
                volumes,
                options,
                credentials,
            } => Self {
                image,
                env,
                ports,
                volumes,
                options,
                credentials,
            },
        }
    }
}

/// Strategy configuration (matrix, fail-fast, etc.)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Strategy {
    /// Matrix configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub matrix: Option<MatrixConfig>,

    /// Whether a failing matrix instance cancels the ones still to run.
    /// Defaults to `true`, matching GitHub.
    #[serde(default, rename = "fail-fast", skip_serializing_if = "Option::is_none")]
    pub fail_fast: Option<bool>,

    /// Maximum parallelism.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[serde(rename = "max-parallel")]
    pub max_parallel: Option<u64>,
}

impl Strategy {
    /// Whether a failed instance should cancel the remaining ones.
    pub fn is_fail_fast(&self) -> bool {
        self.fail_fast.unwrap_or(true)
    }
}

/// One axis of a build matrix, e.g. `os: [ubuntu-latest, macos-latest]`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MatrixAxis {
    pub name: String,
    pub values: MatrixSource<serde_yaml::Value>,
}

/// Something a matrix needs that may be written out, or computed.
///
/// `os: [linux, macos]` is known while the workflow is being parsed.
/// `os: ${{ fromJSON(needs.plan.outputs.targets) }}` is not known until the
/// job it depends on has run, so it is carried as text and resolved by
/// [`MatrixConfig::resolve`] once there is a context to evaluate it against.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum MatrixSource<T> {
    Literal(Vec<T>),
    Expression(String),
}

impl<T> MatrixSource<T> {
    /// The values, for a source that has already been resolved.
    ///
    /// An unresolved expression reads as empty rather than panicking; the
    /// engine resolves before expanding, so this only bites a caller that
    /// expands a config straight off the parser.
    pub fn values(&self) -> &[T] {
        match self {
            MatrixSource::Literal(values) => values,
            MatrixSource::Expression(_) => &[],
        }
    }

    pub fn expression(&self) -> Option<&str> {
        match self {
            MatrixSource::Expression(source) => Some(source),
            MatrixSource::Literal(_) => None,
        }
    }
}

impl<T> Default for MatrixSource<T> {
    fn default() -> Self {
        MatrixSource::Literal(Vec::new())
    }
}

/// Matrix configuration.
///
/// Axes keep their declaration order — a `HashMap` would make the expansion
/// order (and therefore the run order and log output) vary between runs.
#[derive(Debug, Clone, Default, Serialize)]
pub struct MatrixConfig {
    /// The whole matrix as one expression, for
    /// `matrix: ${{ fromJSON(needs.plan.outputs.matrix) }}`.
    ///
    /// When set, it replaces everything else once resolved.
    pub expression: Option<String>,

    /// Matrix axes, in the order they were written.
    pub axes: Vec<MatrixAxis>,

    /// Combinations to remove from the product.
    pub exclude: MatrixSource<serde_yaml::Mapping>,

    /// Extra values to merge in, or extra combinations to append.
    pub include: MatrixSource<serde_yaml::Mapping>,
}

impl<'de> Deserialize<'de> for MatrixConfig {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::Error;

        let value = serde_yaml::Value::deserialize(deserializer)?;

        // The whole matrix can be one expression, which is how a job fans out
        // over a list another job computed.
        if let serde_yaml::Value::String(source) = &value {
            if is_expression(source) {
                return Ok(MatrixConfig {
                    expression: Some(source.clone()),
                    ..MatrixConfig::default()
                });
            }
        }

        let serde_yaml::Value::Mapping(map) = value else {
            return Err(D::Error::custom(format!(
                "expected a mapping or an expression for `matrix`, got {:?}",
                value
            )));
        };

        let mut config = MatrixConfig::default();

        for (key, val) in map {
            let key = key
                .as_str()
                .ok_or_else(|| D::Error::custom("matrix keys must be strings"))?
                .to_string();

            match key.as_str() {
                "include" | "exclude" => {
                    let source = match val {
                        // `include: ${{ fromJSON(…) }}` — resolved later.
                        serde_yaml::Value::String(source) if is_expression(&source) => {
                            MatrixSource::Expression(source)
                        }
                        serde_yaml::Value::Sequence(entries) => {
                            let mut mappings = Vec::with_capacity(entries.len());
                            for entry in entries {
                                match entry {
                                    serde_yaml::Value::Mapping(m) => mappings.push(m),
                                    other => {
                                        return Err(D::Error::custom(format!(
                                            "`matrix.{}` entries must be mappings, got {:?}",
                                            key, other
                                        )))
                                    }
                                }
                            }
                            MatrixSource::Literal(mappings)
                        }
                        _ => {
                            return Err(D::Error::custom(format!(
                                "`matrix.{}` must be a list of mappings or an expression",
                                key
                            )))
                        }
                    };
                    if key == "include" {
                        config.include = source;
                    } else {
                        config.exclude = source;
                    }
                }
                _ => {
                    let values = match val {
                        serde_yaml::Value::String(source) if is_expression(&source) => {
                            MatrixSource::Expression(source)
                        }
                        serde_yaml::Value::Sequence(values) => MatrixSource::Literal(values),
                        _ => {
                            return Err(D::Error::custom(format!(
                                "matrix axis `{}` must be a list of values or an expression",
                                key
                            )))
                        }
                    };
                    config.axes.push(MatrixAxis { name: key, values });
                }
            }
        }

        Ok(config)
    }
}

impl MatrixConfig {
    /// Whether anything here still has to be evaluated.
    pub fn is_dynamic(&self) -> bool {
        self.expression.is_some()
            || self.include.expression().is_some()
            || self.exclude.expression().is_some()
            || self
                .axes
                .iter()
                .any(|axis| axis.values.expression().is_some())
    }

    /// Resolve every expression against `ctx`, returning a config that is
    /// entirely literal and ready to expand.
    ///
    /// Called once the job's `needs` have finished, which is the earliest
    /// point a matrix computed by another job can be known.
    pub fn resolve(&self, ctx: &crate::types::Context) -> Result<Self, WorkflowError> {
        // `matrix: ${{ … }}` replaces the whole thing.
        if let Some(source) = &self.expression {
            let value = expression_to_yaml("matrix", source, ctx)?;
            let serde_yaml::Value::Mapping(map) = value else {
                return Err(WorkflowError::Other(
                    "`matrix:` must evaluate to an object of axes".to_string(),
                ));
            };
            return Self::from_mapping(map, ctx);
        }

        let mut resolved = MatrixConfig {
            expression: None,
            axes: Vec::with_capacity(self.axes.len()),
            include: MatrixSource::Literal(Vec::new()),
            exclude: MatrixSource::Literal(Vec::new()),
        };

        for axis in &self.axes {
            let values = match &axis.values {
                MatrixSource::Literal(values) => values.clone(),
                MatrixSource::Expression(source) => {
                    sequence(&format!("matrix.{}", axis.name), source, ctx)?
                }
            };
            resolved.axes.push(MatrixAxis {
                name: axis.name.clone(),
                values: MatrixSource::Literal(values),
            });
        }

        resolved.include = MatrixSource::Literal(mappings("matrix.include", &self.include, ctx)?);
        resolved.exclude = MatrixSource::Literal(mappings("matrix.exclude", &self.exclude, ctx)?);
        Ok(resolved)
    }

    /// Build a config from an already-evaluated matrix object.
    fn from_mapping(
        map: serde_yaml::Mapping,
        ctx: &crate::types::Context,
    ) -> Result<Self, WorkflowError> {
        let mut config = MatrixConfig::default();
        // Axis order decides which axis varies slowest, and a computed matrix
        // arrives through JSON, where key order is not preserved. Sorting is
        // what makes instance ids the same from one run to the next.
        let mut entries: Vec<(String, serde_yaml::Value)> = map
            .into_iter()
            .filter_map(|(key, value)| Some((key.as_str()?.to_string(), value)))
            .collect();
        entries.sort_by(|a, b| a.0.cmp(&b.0));

        for (key, value) in entries {
            match key.as_str() {
                "include" | "exclude" => {
                    let source = MatrixSource::Literal(to_mappings(&key, value)?);
                    if key == "include" {
                        config.include = source;
                    } else {
                        config.exclude = source;
                    }
                }
                _ => {
                    let serde_yaml::Value::Sequence(values) = value else {
                        return Err(WorkflowError::Other(format!(
                            "matrix axis `{}` must evaluate to a list",
                            key
                        )));
                    };
                    config.axes.push(MatrixAxis {
                        name: key,
                        values: MatrixSource::Literal(values),
                    });
                }
            }
        }
        // `include`/`exclude` inside a computed matrix are already values, but
        // an axis of one could still hold an expression.
        config.resolve(ctx)
    }
}

/// Evaluate a `${{ … }}` that has to produce a value rather than text.
fn expression_to_yaml(
    what: &str,
    source: &str,
    ctx: &crate::types::Context,
) -> Result<serde_yaml::Value, WorkflowError> {
    let value = crate::expr::evaluate_value(source, ctx)
        .map_err(|e| WorkflowError::Other(format!("`{}`: {}", what, e)))?;
    serde_yaml::to_value(crate::expr::value_to_json(&value))
        .map_err(|e| WorkflowError::Other(format!("`{}`: {}", what, e)))
}

fn sequence(
    what: &str,
    source: &str,
    ctx: &crate::types::Context,
) -> Result<Vec<serde_yaml::Value>, WorkflowError> {
    match expression_to_yaml(what, source, ctx)? {
        serde_yaml::Value::Sequence(values) => Ok(values),
        other => Err(WorkflowError::Other(format!(
            "`{}` must evaluate to a list, got {:?}",
            what, other
        ))),
    }
}

fn mappings(
    what: &str,
    source: &MatrixSource<serde_yaml::Mapping>,
    ctx: &crate::types::Context,
) -> Result<Vec<serde_yaml::Mapping>, WorkflowError> {
    match source {
        MatrixSource::Literal(values) => Ok(values.clone()),
        MatrixSource::Expression(source) => {
            to_mappings(what, expression_to_yaml(what, source, ctx)?)
        }
    }
}

fn to_mappings(
    what: &str,
    value: serde_yaml::Value,
) -> Result<Vec<serde_yaml::Mapping>, WorkflowError> {
    let serde_yaml::Value::Sequence(entries) = value else {
        return Err(WorkflowError::Other(format!(
            "`{}` must evaluate to a list of objects",
            what
        )));
    };
    entries
        .into_iter()
        .map(|entry| match entry {
            serde_yaml::Value::Mapping(map) => Ok(map),
            other => Err(WorkflowError::Other(format!(
                "`{}` entries must be objects, got {:?}",
                what, other
            ))),
        })
        .collect()
}

/// Whether a scalar is a `${{ … }}` expression rather than a plain value.
fn is_expression(value: &str) -> bool {
    let value = value.trim();
    value.starts_with("${{") && value.ends_with("}}")
}

/// A single step within a job.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Step {
    /// Optional step identifier for referencing outputs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,

    /// Human-readable step name.
    #[serde(default)]
    pub name: String,

    /// An action to use (e.g., "actions/checkout@v4").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uses: Option<String>,

    /// A shell command to run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run: Option<String>,

    /// The shell to use for run commands.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shell: Option<String>,

    /// Working directory for this step.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[serde(rename = "working-directory")]
    pub working_directory: Option<String>,

    /// Input parameters for the action.
    #[serde(default, deserialize_with = "de_string_map")]
    pub with: HashMap<String, String>,

    /// Environment variables for this step only.
    #[serde(default, deserialize_with = "de_string_map")]
    pub env: HashMap<String, String>,

    /// Condition expression (`if`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[serde(rename = "if")]
    pub if_condition: Option<String>,

    /// Continue on error.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[serde(rename = "continue-on-error")]
    pub continue_on_error: Option<bool>,

    /// Timeout in minutes for this step.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[serde(rename = "timeout-minutes")]
    pub timeout_minutes: Option<f64>,
}

/// A parsed workflow ready for execution, with all expressions resolved.
#[derive(Debug, Clone)]
pub struct ResolvedWorkflow {
    pub name: String,
    pub env: HashMap<String, String>,
    pub jobs: Vec<ResolvedJob>,
}

/// A resolved job with unmet dependencies tracked.
#[derive(Debug, Clone)]
pub struct ResolvedJob {
    pub id: String,
    pub name: String,
    pub env: HashMap<String, String>,
    pub steps: Vec<Step>,
    pub needs: Vec<String>,
    pub if_condition: Option<String>,
    pub outputs: Option<HashMap<String, String>>,
    pub timeout_minutes: Option<u64>,
    pub matrix: Option<MatrixConfig>,
}

/// Supported event types for workflow dispatch.
#[derive(Debug, Clone, PartialEq)]
pub enum EventType {
    Push,
    PullRequest,
    Release,
    WorkflowDispatch,
    Schedule,
}
