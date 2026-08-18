//! The `action.yml` an action ships with.
//!
//! Everything minact needs to run an action is declared here: the inputs it
//! takes (with their defaults), the outputs it promises, and the `runs:` block
//! saying whether it is JavaScript, a list of steps, or a container.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::types::WorkflowError;
use crate::workflow::Step;

/// The two spellings GitHub accepts, in the order it looks for them.
const MANIFEST_FILES: &[&str] = &["action.yml", "action.yaml"];

/// A parsed `action.yml`.
#[derive(Debug, Clone)]
pub struct ActionManifest {
    pub name: String,
    pub description: String,
    pub inputs: BTreeMap<String, ActionInput>,
    pub outputs: BTreeMap<String, ActionOutputSpec>,
    pub runs: ActionRuns,
}

/// One entry of the `inputs:` map.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActionInput {
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub required: bool,
    #[serde(default)]
    pub default: Option<String>,
    /// Warned about when the workflow passes this input.
    #[serde(default, rename = "deprecationMessage")]
    pub deprecation_message: Option<String>,
}

/// One entry of the `outputs:` map.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ActionOutputSpec {
    #[serde(default)]
    pub description: String,
    /// Composite actions compute their outputs from an expression; JavaScript
    /// and container actions write theirs to `$GITHUB_OUTPUT` instead.
    #[serde(default)]
    pub value: Option<String>,
}

/// How an action runs.
#[derive(Debug, Clone)]
pub enum ActionRuns {
    /// `using: node16` / `node20` / `node24`.
    Node {
        /// The runtime as written, e.g. `node20`.
        using: String,
        main: String,
        pre: Option<String>,
        pre_if: Option<String>,
        post: Option<String>,
        post_if: Option<String>,
    },
    /// `using: composite` — a list of steps run in the caller's job.
    Composite { steps: Vec<Step> },
    /// `using: docker` — a container, built from a `Dockerfile` or pulled.
    Docker {
        image: DockerImageSource,
        entrypoint: Option<String>,
        args: Vec<String>,
        env: BTreeMap<String, String>,
        pre_entrypoint: Option<String>,
        pre_if: Option<String>,
        post_entrypoint: Option<String>,
        post_if: Option<String>,
    },
}

/// Where a container action's image comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DockerImageSource {
    /// `image: docker://alpine:3.18` — pulled as-is.
    Registry(String),
    /// `image: Dockerfile` — built from a file in the action directory.
    Dockerfile(String),
}

impl ActionManifest {
    /// Find and parse the `action.yml` in an action directory.
    pub fn load(dir: &Path) -> Result<Self, WorkflowError> {
        let path = Self::locate(dir).ok_or_else(|| {
            WorkflowError::Other(format!("no action.yml or action.yaml in {}", dir.display()))
        })?;
        let source = std::fs::read_to_string(&path)?;
        Self::from_yaml(&source)
            .map_err(|e| WorkflowError::Other(format!("{}: {}", path.display(), e)))
    }

    /// The manifest file in an action directory, if there is one.
    pub fn locate(dir: &Path) -> Option<PathBuf> {
        MANIFEST_FILES
            .iter()
            .map(|name| dir.join(name))
            .find(|path| path.is_file())
    }

    /// Parse a manifest from YAML.
    pub fn from_yaml(source: &str) -> Result<Self, WorkflowError> {
        let raw: RawManifest = serde_yaml::from_str(source)
            .map_err(|e| WorkflowError::Other(format!("invalid action manifest: {}", e)))?;
        raw.into_manifest()
    }

    /// The value an input takes when the workflow does not pass it, and an
    /// error when it is `required:` with no default.
    pub fn default_for(&self, name: &str) -> Option<&str> {
        self.inputs.get(name)?.default.as_deref()
    }
}

// ---------------------------------------------------------------------------
// Deserialisation
// ---------------------------------------------------------------------------

/// The manifest as written, before `runs.using` decides its shape.
#[derive(Debug, Deserialize)]
struct RawManifest {
    #[serde(default)]
    name: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    inputs: BTreeMap<String, ActionInput>,
    #[serde(default)]
    outputs: BTreeMap<String, ActionOutputSpec>,
    runs: RawRuns,
}

#[derive(Debug, Deserialize)]
struct RawRuns {
    using: String,

    // node
    #[serde(default)]
    main: Option<String>,
    #[serde(default)]
    pre: Option<String>,
    #[serde(default)]
    post: Option<String>,

    // composite
    #[serde(default)]
    steps: Option<Vec<Step>>,

    // docker
    #[serde(default)]
    image: Option<String>,
    #[serde(default)]
    entrypoint: Option<String>,
    #[serde(default)]
    args: Option<Vec<String>>,
    #[serde(default)]
    env: Option<BTreeMap<String, String>>,
    #[serde(default, rename = "pre-entrypoint")]
    pre_entrypoint: Option<String>,
    #[serde(default, rename = "post-entrypoint")]
    post_entrypoint: Option<String>,

    // shared
    #[serde(default, rename = "pre-if")]
    pre_if: Option<String>,
    #[serde(default, rename = "post-if")]
    post_if: Option<String>,
}

impl RawManifest {
    fn into_manifest(self) -> Result<ActionManifest, WorkflowError> {
        let runs = self.runs.into_runs()?;
        Ok(ActionManifest {
            name: self.name,
            description: self.description,
            inputs: self.inputs,
            outputs: self.outputs,
            runs,
        })
    }
}

impl RawRuns {
    fn into_runs(self) -> Result<ActionRuns, WorkflowError> {
        let using = self.using.trim().to_ascii_lowercase();

        if using == "composite" {
            let steps = self.steps.ok_or_else(|| {
                WorkflowError::Other("a composite action needs `runs.steps`".to_string())
            })?;
            return Ok(ActionRuns::Composite { steps });
        }

        if using == "docker" {
            let image = self.image.ok_or_else(|| {
                WorkflowError::Other("a container action needs `runs.image`".to_string())
            })?;
            let image = match image.strip_prefix("docker://") {
                Some(reference) => DockerImageSource::Registry(reference.to_string()),
                None => DockerImageSource::Dockerfile(image),
            };
            return Ok(ActionRuns::Docker {
                image,
                entrypoint: self.entrypoint,
                args: self.args.unwrap_or_default(),
                env: self.env.unwrap_or_default(),
                pre_entrypoint: self.pre_entrypoint,
                pre_if: self.pre_if,
                post_entrypoint: self.post_entrypoint,
                post_if: self.post_if,
            });
        }

        // Every other `using:` GitHub defines is a JavaScript runtime, and new
        // node majors keep appearing — accept the family rather than a list.
        if using.starts_with("node") {
            let main = self.main.ok_or_else(|| {
                WorkflowError::Other("a JavaScript action needs `runs.main`".to_string())
            })?;
            return Ok(ActionRuns::Node {
                using,
                main,
                pre: self.pre,
                pre_if: self.pre_if,
                post: self.post,
                post_if: self.post_if,
            });
        }

        Err(WorkflowError::Other(format!(
            "unsupported `runs.using: {}` — minact runs node*, composite and docker actions",
            self.using
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_javascript_action() {
        let manifest = ActionManifest::from_yaml(
            r#"
name: Greet
description: Says hello
inputs:
  who-to-greet:
    description: Who
    required: true
    default: World
  legacy:
    description: Old
    deprecationMessage: Use who-to-greet
outputs:
  time:
    description: When
runs:
  using: node20
  main: dist/index.js
  post: dist/cleanup.js
  post-if: always()
"#,
        )
        .unwrap();

        assert_eq!(manifest.name, "Greet");
        assert_eq!(manifest.default_for("who-to-greet"), Some("World"));
        assert!(manifest.inputs["who-to-greet"].required);
        assert_eq!(
            manifest.inputs["legacy"].deprecation_message.as_deref(),
            Some("Use who-to-greet")
        );
        assert!(manifest.outputs.contains_key("time"));

        match manifest.runs {
            ActionRuns::Node {
                using,
                main,
                pre,
                post,
                post_if,
                ..
            } => {
                assert_eq!(using, "node20");
                assert_eq!(main, "dist/index.js");
                assert_eq!(pre, None);
                assert_eq!(post.as_deref(), Some("dist/cleanup.js"));
                assert_eq!(post_if.as_deref(), Some("always()"));
            }
            other => panic!("parsed as {:?}", other),
        }
    }

    #[test]
    fn parses_a_composite_action() {
        let manifest = ActionManifest::from_yaml(
            r#"
name: Build
description: Composite
outputs:
  sha:
    description: The sha
    value: ${{ steps.rev.outputs.sha }}
runs:
  using: composite
  steps:
    - id: rev
      run: echo "sha=abc" >> $GITHUB_OUTPUT
      shell: bash
    - uses: ./nested
"#,
        )
        .unwrap();

        assert_eq!(
            manifest.outputs["sha"].value.as_deref(),
            Some("${{ steps.rev.outputs.sha }}")
        );
        match manifest.runs {
            ActionRuns::Composite { steps } => {
                assert_eq!(steps.len(), 2);
                assert_eq!(steps[0].id.as_deref(), Some("rev"));
                assert_eq!(steps[1].uses.as_deref(), Some("./nested"));
            }
            other => panic!("parsed as {:?}", other),
        }
    }

    #[test]
    fn tells_a_built_image_from_a_pulled_one() {
        let built = ActionManifest::from_yaml(
            "name: x\ndescription: y\nruns:\n  using: docker\n  image: Dockerfile\n  args: ['a']\n",
        )
        .unwrap();
        let pulled = ActionManifest::from_yaml(
            "name: x\ndescription: y\nruns:\n  using: docker\n  image: docker://alpine:3.18\n",
        )
        .unwrap();

        match built.runs {
            ActionRuns::Docker { image, args, .. } => {
                assert_eq!(image, DockerImageSource::Dockerfile("Dockerfile".into()));
                assert_eq!(args, ["a"]);
            }
            other => panic!("parsed as {:?}", other),
        }
        match pulled.runs {
            ActionRuns::Docker { image, .. } => {
                assert_eq!(image, DockerImageSource::Registry("alpine:3.18".into()));
            }
            other => panic!("parsed as {:?}", other),
        }
    }

    #[test]
    fn accepts_a_node_major_it_has_never_heard_of() {
        let manifest = ActionManifest::from_yaml(
            "name: x\ndescription: y\nruns:\n  using: node24\n  main: index.js\n",
        )
        .unwrap();
        assert!(matches!(manifest.runs, ActionRuns::Node { .. }));
    }

    #[test]
    fn rejects_manifests_it_cannot_run() {
        // A runtime minact has no story for.
        assert!(ActionManifest::from_yaml(
            "name: x\ndescription: y\nruns:\n  using: dotnet\n  main: a.dll\n"
        )
        .is_err());
        // Right family, missing the entry point.
        assert!(
            ActionManifest::from_yaml("name: x\ndescription: y\nruns:\n  using: node20\n").is_err()
        );
        assert!(
            ActionManifest::from_yaml("name: x\ndescription: y\nruns:\n  using: composite\n")
                .is_err()
        );
        assert!(
            ActionManifest::from_yaml("name: x\ndescription: y\nruns:\n  using: docker\n").is_err()
        );
    }

    #[test]
    fn locates_either_spelling() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(ActionManifest::locate(dir.path()), None);
        std::fs::write(dir.path().join("action.yaml"), "").unwrap();
        assert_eq!(
            ActionManifest::locate(dir.path()),
            Some(dir.path().join("action.yaml"))
        );
    }
}
