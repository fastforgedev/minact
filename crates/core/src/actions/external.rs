//! Turning a `uses:` value into something the engine can run.
//!
//! Registered actions — minact's built-ins and whatever an embedding tool
//! added — are answered from the registry. Everything else is *external*: it
//! has a directory on disk and an `action.yml` describing how to run it, and
//! this module is what produces both.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use crate::logging::LogLevel;
use crate::types::WorkflowError;

use super::manifest::{ActionManifest, ActionRuns, DockerImageSource};
use super::reference::ActionRef;
use super::store::ActionStore;

/// An action reference resolved to something runnable.
#[derive(Debug, Clone)]
pub struct ResolvedAction {
    /// The reference it came from, for logs and `github.action_repository`.
    pub reference: ActionRef,
    /// The directory holding `action.yml`. For a bare `docker://` image there
    /// is no such directory, and the workspace stands in so that relative
    /// paths still resolve somewhere sensible.
    pub dir: PathBuf,
    pub manifest: ActionManifest,
}

impl ResolvedAction {
    /// What `github.action_path` should read as: empty for a bare image,
    /// which has no checkout.
    pub fn action_path(&self) -> String {
        match self.reference {
            ActionRef::DockerImage { .. } => String::new(),
            _ => self.dir.to_string_lossy().to_string(),
        }
    }
}

/// Resolve a reference, fetching it if it does not exist locally yet.
pub async fn resolve(
    reference: &ActionRef,
    workspace: &Path,
    store: &ActionStore,
    report: &mut (dyn FnMut(LogLevel, String) + Send),
) -> Result<ResolvedAction, WorkflowError> {
    let (dir, manifest) = match reference {
        ActionRef::Repository {
            owner,
            repo,
            path,
            git_ref,
        } => {
            let dir = store
                .fetch(owner, repo, path.as_deref(), git_ref, report)
                .await?;
            let manifest = ActionManifest::load(&dir)?;
            (dir, manifest)
        }

        ActionRef::Local { path } => {
            let dir = workspace.join(path.trim_start_matches("./"));
            if !dir.is_dir() {
                return Err(WorkflowError::Other(format!(
                    "local action `{}` is not a directory ({})",
                    path,
                    dir.display()
                )));
            }
            // A symlink could still lead out of the workspace even though the
            // reference itself contains no `..`.
            let dir = dir.canonicalize()?;
            if !dir.starts_with(workspace.canonicalize()?) {
                return Err(WorkflowError::Other(format!(
                    "local action `{}` resolves outside the workspace",
                    path
                )));
            }
            let manifest = ActionManifest::load(&dir)?;
            (dir, manifest)
        }

        // `uses: docker://image` has no repository and no manifest. GitHub
        // treats it as a container action whose `entrypoint` and `args` come
        // from the step's `with:`, so that is the manifest to synthesise.
        ActionRef::DockerImage { image } => (
            workspace.to_path_buf(),
            ActionManifest {
                name: image.clone(),
                description: String::new(),
                inputs: BTreeMap::new(),
                outputs: BTreeMap::new(),
                runs: ActionRuns::Docker {
                    image: DockerImageSource::Registry(image.clone()),
                    entrypoint: None,
                    args: Vec::new(),
                    env: BTreeMap::new(),
                    pre_entrypoint: None,
                    pre_if: None,
                    post_entrypoint: None,
                    post_if: None,
                },
            },
        ),
    };

    Ok(ResolvedAction {
        reference: reference.clone(),
        dir,
        manifest,
    })
}

/// The inputs an action sees, and anything worth telling the user about them.
#[derive(Debug, Default, Clone)]
pub struct ActionInputs {
    /// `INPUT_*` variables, ready to merge into the step environment.
    pub env: HashMap<String, String>,
    /// The resolved values, keyed by input name as written.
    pub values: HashMap<String, String>,
    pub warnings: Vec<String>,
}

/// Build the `INPUT_*` environment from a step's `with:` and the manifest's
/// declared defaults.
///
/// Values arrive already expression-evaluated; this only decides which inputs
/// exist and what they are called.
pub fn action_inputs(manifest: &ActionManifest, with: &HashMap<String, String>) -> ActionInputs {
    let mut values: HashMap<String, String> = HashMap::new();
    let mut warnings = Vec::new();

    // Declared defaults first, so an explicit `with:` overrides them.
    for (name, input) in &manifest.inputs {
        if let Some(default) = &input.default {
            values.insert(name.clone(), default.clone());
        }
    }
    values.extend(with.iter().map(|(k, v)| (k.clone(), v.clone())));

    for (name, input) in &manifest.inputs {
        if let Some(message) = &input.deprecation_message {
            if with.contains_key(name) {
                warnings.push(format!("input `{}` is deprecated: {}", name, message));
            }
        }
        // GitHub's runner does not enforce `required:` either, but saying so
        // locally is cheaper than finding out from a failing action.
        if input.required && !values.contains_key(name) {
            warnings.push(format!("required input `{}` was not provided", name));
        }
    }

    let env = values
        .iter()
        .map(|(name, value)| (input_var_name(name), value.clone()))
        .collect();

    ActionInputs {
        env,
        values,
        warnings,
    }
}

/// The environment variable an input arrives in.
///
/// GitHub uppercases the name and replaces spaces with underscores, and
/// deliberately leaves dashes alone — `who-to-greet` is read from
/// `INPUT_WHO-TO-GREET`.
pub fn input_var_name(name: &str) -> String {
    format!("INPUT_{}", name.to_uppercase().replace(' ', "_"))
}

/// Split a command-line string into arguments, honouring quotes.
///
/// A manifest declares `runs.args` as a list, but a bare
/// `uses: docker://image` has no manifest and the step's `with.args` stands in
/// as one string. Splitting that on whitespace would break
/// `-c "echo hello world"` into three arguments, so quoting is respected the
/// way a shell would — without any of the expansion a shell would also do.
pub fn split_arguments(input: &str) -> Vec<String> {
    let mut args = Vec::new();
    let mut current = String::new();
    let mut has_current = false;
    let mut chars = input.chars().peekable();

    while let Some(c) = chars.next() {
        match c {
            c if c.is_whitespace() => {
                if has_current {
                    args.push(std::mem::take(&mut current));
                    has_current = false;
                }
            }
            '\'' => {
                has_current = true;
                for c in chars.by_ref() {
                    if c == '\'' {
                        break;
                    }
                    current.push(c);
                }
            }
            '"' => {
                has_current = true;
                while let Some(c) = chars.next() {
                    match c {
                        '"' => break,
                        // Inside double quotes a backslash only escapes a
                        // quote or another backslash; everything else is
                        // literal, as in a shell.
                        '\\' if matches!(chars.peek(), Some('"') | Some('\\')) => {
                            current.push(chars.next().unwrap_or('\\'));
                        }
                        c => current.push(c),
                    }
                }
            }
            '\\' => {
                has_current = true;
                if let Some(next) = chars.next() {
                    current.push(next);
                }
            }
            c => {
                has_current = true;
                current.push(c);
            }
        }
    }

    if has_current {
        args.push(current);
    }
    args
}

/// The `github.action` value: a slug identifying the step's action, the way
/// GitHub names it when a step has no `id`.
pub fn action_slug(reference: &ActionRef) -> String {
    let raw = match reference {
        ActionRef::Repository { owner, repo, .. } => format!("{}{}", owner, repo),
        ActionRef::Local { path } => path.trim_start_matches("./").to_string(),
        ActionRef::DockerImage { image } => image.clone(),
    };
    let slug: String = raw
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    format!("__{}", slug)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actions::manifest::ActionInput;

    fn manifest_with(inputs: &[(&str, ActionInput)]) -> ActionManifest {
        ActionManifest {
            name: "test".into(),
            description: String::new(),
            inputs: inputs
                .iter()
                .map(|(name, input)| (name.to_string(), input.clone()))
                .collect(),
            outputs: BTreeMap::new(),
            runs: ActionRuns::Node {
                using: "node20".into(),
                main: "index.js".into(),
                pre: None,
                pre_if: None,
                post: None,
                post_if: None,
            },
        }
    }

    #[test]
    fn splits_arguments_without_breaking_quoted_ones() {
        assert_eq!(
            split_arguments("-c \"echo hello world\""),
            ["-c", "echo hello world"]
        );
        assert_eq!(split_arguments("a b   c"), ["a", "b", "c"]);
        assert_eq!(split_arguments("  "), Vec::<String>::new());
        assert_eq!(split_arguments("'single quoted'"), ["single quoted"]);
        // An empty argument is a real argument.
        assert_eq!(split_arguments("a '' b"), ["a", "", "b"]);
        // Quotes join rather than separate, as in a shell.
        assert_eq!(
            split_arguments("pre\"in quotes\"post"),
            ["prein quotespost"]
        );
        assert_eq!(
            split_arguments(r#"say \"hi\""#),
            ["say", r#""hi""#].map(String::from)
        );
        assert_eq!(split_arguments(r#""a \"b\" c""#), [r#"a "b" c"#]);
        // Unterminated quotes take the rest rather than dropping it.
        assert_eq!(split_arguments("-c \"unterminated"), ["-c", "unterminated"]);
    }

    #[test]
    fn names_inputs_the_way_the_toolkit_reads_them() {
        assert_eq!(input_var_name("who-to-greet"), "INPUT_WHO-TO-GREET");
        assert_eq!(input_var_name("node version"), "INPUT_NODE_VERSION");
        assert_eq!(input_var_name("path"), "INPUT_PATH");
    }

    #[test]
    fn defaults_apply_and_with_overrides_them() {
        let manifest = manifest_with(&[
            (
                "greeting",
                ActionInput {
                    default: Some("hello".into()),
                    ..Default::default()
                },
            ),
            (
                "target",
                ActionInput {
                    default: Some("World".into()),
                    ..Default::default()
                },
            ),
        ]);
        let with = HashMap::from([("target".to_string(), "minact".to_string())]);

        let inputs = action_inputs(&manifest, &with);
        assert_eq!(inputs.env["INPUT_GREETING"], "hello");
        assert_eq!(inputs.env["INPUT_TARGET"], "minact");
        assert!(inputs.warnings.is_empty());
    }

    #[test]
    fn passes_through_inputs_the_manifest_never_declared() {
        // Undeclared inputs still reach the action; GitHub does not filter
        // them, and some actions read them straight from the environment.
        let inputs = action_inputs(
            &manifest_with(&[]),
            &HashMap::from([("extra".to_string(), "1".to_string())]),
        );
        assert_eq!(inputs.env["INPUT_EXTRA"], "1");
    }

    #[test]
    fn warns_about_deprecated_and_missing_inputs() {
        let manifest = manifest_with(&[
            (
                "old",
                ActionInput {
                    deprecation_message: Some("use `new`".into()),
                    ..Default::default()
                },
            ),
            (
                "needed",
                ActionInput {
                    required: true,
                    ..Default::default()
                },
            ),
        ]);

        let inputs = action_inputs(
            &manifest,
            &HashMap::from([("old".to_string(), "x".to_string())]),
        );
        assert_eq!(inputs.warnings.len(), 2);
        assert!(inputs.warnings.iter().any(|w| w.contains("deprecated")));
        assert!(inputs.warnings.iter().any(|w| w.contains("required")));

        // A deprecated input nobody passes is not worth a warning.
        let quiet = action_inputs(
            &manifest,
            &HashMap::from([("needed".to_string(), "y".to_string())]),
        );
        assert!(quiet.warnings.is_empty());
    }

    #[tokio::test]
    async fn resolves_a_local_action() {
        let workspace = tempfile::tempdir().unwrap();
        let action = workspace.path().join("tools/greet");
        std::fs::create_dir_all(&action).unwrap();
        std::fs::write(
            action.join("action.yml"),
            "name: Greet\ndescription: x\nruns:\n  using: node20\n  main: index.js\n",
        )
        .unwrap();

        let store = ActionStore::with_root(workspace.path().join(".cache"));
        let resolved = resolve(
            &ActionRef::Local {
                path: "./tools/greet".into(),
            },
            workspace.path(),
            &store,
            &mut |_, _| {},
        )
        .await
        .unwrap();

        assert_eq!(resolved.manifest.name, "Greet");
        assert!(resolved.action_path().ends_with("tools/greet"));
    }

    #[tokio::test]
    async fn a_local_action_that_is_not_there_says_so() {
        let workspace = tempfile::tempdir().unwrap();
        let store = ActionStore::with_root(workspace.path().join(".cache"));
        let error = resolve(
            &ActionRef::Local {
                path: "./missing".into(),
            },
            workspace.path(),
            &store,
            &mut |_, _| {},
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("missing"));
    }

    #[tokio::test]
    async fn a_bare_image_gets_a_synthesised_manifest() {
        let workspace = tempfile::tempdir().unwrap();
        let store = ActionStore::with_root(workspace.path().join(".cache"));
        let resolved = resolve(
            &ActionRef::DockerImage {
                image: "alpine:3.18".into(),
            },
            workspace.path(),
            &store,
            &mut |_, _| {},
        )
        .await
        .unwrap();

        assert!(resolved.action_path().is_empty());
        match resolved.manifest.runs {
            ActionRuns::Docker { image, .. } => {
                assert_eq!(image, DockerImageSource::Registry("alpine:3.18".into()));
            }
            other => panic!("synthesised {:?}", other),
        }
    }
}
