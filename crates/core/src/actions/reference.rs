//! What a `uses:` value points at.
//!
//! GitHub accepts three shapes and minact accepts a fourth. In precedence
//! order, as [`resolve`](super::resolve_reference) applies them:
//!
//! * a name registered in the [`ActionRegistry`](super::ActionRegistry) —
//!   minact's built-ins and whatever an embedding tool added,
//! * `./path/to/action`, a directory inside the workspace,
//! * `docker://image:tag`, a container image run as-is,
//! * `owner/repo[/subdir]@ref`, fetched from a git host.

use crate::types::WorkflowError;

/// A parsed `uses:` value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActionRef {
    /// `owner/repo@v4`, optionally with a sub-directory: the action lives in
    /// a repository that has to be fetched.
    Repository {
        owner: String,
        repo: String,
        /// Sub-directory holding `action.yml`, for `owner/repo/sub/dir@ref`.
        path: Option<String>,
        /// Whatever followed `@`: a tag, a branch or a commit SHA.
        git_ref: String,
    },
    /// `./path/to/action`, relative to the workspace root.
    Local { path: String },
    /// `docker://alpine:3.18`, run without a manifest.
    DockerImage { image: String },
}

impl ActionRef {
    /// Parse a `uses:` value.
    pub fn parse(uses: &str) -> Result<Self, WorkflowError> {
        let uses = uses.trim();
        if uses.is_empty() {
            return Err(invalid(uses, "it is empty"));
        }

        if let Some(image) = uses.strip_prefix("docker://") {
            if image.is_empty() {
                return Err(invalid(uses, "it names no image"));
            }
            return Ok(ActionRef::DockerImage {
                image: image.to_string(),
            });
        }

        // GitHub only recognises `./` and `../` as local; a bare relative path
        // is a repository reference missing its owner.
        if uses.starts_with("./") || uses.starts_with(".\\") {
            let path = uses.replace('\\', "/");
            if path.contains('@') {
                return Err(invalid(uses, "a local action cannot carry a `@ref`"));
            }
            if has_parent_segment(&path) {
                return Err(invalid(uses, "a local action cannot escape the workspace"));
            }
            return Ok(ActionRef::Local { path });
        }

        let Some((repo_path, git_ref)) = uses.rsplit_once('@') else {
            return Err(invalid(
                uses,
                "it needs a `@ref` — write `owner/repo@v4`, `./local-action` or `docker://image`",
            ));
        };
        if git_ref.is_empty() {
            return Err(invalid(uses, "the `@ref` is empty"));
        }
        if has_parent_segment(repo_path) {
            return Err(invalid(uses, "it contains a `..` path segment"));
        }

        let mut segments = repo_path.split('/');
        let owner = segments.next().unwrap_or_default();
        let repo = segments.next().unwrap_or_default();
        if owner.is_empty() || repo.is_empty() {
            return Err(invalid(uses, "it is not of the form `owner/repo@ref`"));
        }

        let rest = segments.collect::<Vec<_>>().join("/");
        Ok(ActionRef::Repository {
            owner: owner.to_string(),
            repo: repo.to_string(),
            path: (!rest.is_empty()).then_some(rest),
            git_ref: git_ref.to_string(),
        })
    }

    /// How the reference reads in a log line.
    pub fn describe(&self) -> String {
        match self {
            ActionRef::Repository {
                owner,
                repo,
                path,
                git_ref,
            } => match path {
                Some(path) => format!("{}/{}/{}@{}", owner, repo, path, git_ref),
                None => format!("{}/{}@{}", owner, repo, git_ref),
            },
            ActionRef::Local { path } => path.clone(),
            ActionRef::DockerImage { image } => format!("docker://{}", image),
        }
    }

    /// The `github.action_repository` value: `owner/repo`, or empty for
    /// references that do not come from one.
    pub fn repository(&self) -> String {
        match self {
            ActionRef::Repository { owner, repo, .. } => format!("{}/{}", owner, repo),
            _ => String::new(),
        }
    }
}

/// The name a `uses:` value is looked up under in the action registry:
/// everything before the `@`.
///
/// `actions/checkout@v4` and `actions/checkout@v3` are the same registered
/// action, which is why the version is dropped rather than matched.
pub fn registry_name(uses: &str) -> &str {
    let uses = uses.trim();
    match uses.rsplit_once('@') {
        Some((name, _)) if !name.is_empty() => name,
        _ => uses,
    }
}

/// True when any path segment is `..`, which would let a reference reach
/// outside the workspace or the fetched repository.
fn has_parent_segment(path: &str) -> bool {
    path.split(['/', '\\']).any(|segment| segment == "..")
}

fn invalid(uses: &str, why: &str) -> WorkflowError {
    WorkflowError::Other(format!("invalid `uses: {}` — {}", uses, why))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo(uses: &str) -> (String, String, Option<String>, String) {
        match ActionRef::parse(uses).unwrap() {
            ActionRef::Repository {
                owner,
                repo,
                path,
                git_ref,
            } => (owner, repo, path, git_ref),
            other => panic!("{} parsed as {:?}", uses, other),
        }
    }

    #[test]
    fn parses_owner_repo_and_ref() {
        assert_eq!(
            repo("actions/checkout@v4"),
            ("actions".into(), "checkout".into(), None, "v4".into())
        );
    }

    #[test]
    fn parses_a_subdirectory() {
        assert_eq!(
            repo("owner/repo/sub/dir@main"),
            (
                "owner".into(),
                "repo".into(),
                Some("sub/dir".into()),
                "main".into()
            )
        );
    }

    #[test]
    fn keeps_a_ref_that_looks_like_a_path() {
        // Branch names contain slashes; only the last `@` separates the ref.
        assert_eq!(
            repo("owner/repo@refs/tags/v1"),
            ("owner".into(), "repo".into(), None, "refs/tags/v1".into())
        );
    }

    #[test]
    fn parses_a_full_sha() {
        let sha = "8f4b7f84864484a7bf31766abe9204da3cbe65b3";
        assert_eq!(repo(&format!("actions/checkout@{}", sha)).3, sha);
    }

    #[test]
    fn parses_local_and_docker_forms() {
        assert_eq!(
            ActionRef::parse("./.github/actions/build").unwrap(),
            ActionRef::Local {
                path: "./.github/actions/build".into()
            }
        );
        assert_eq!(
            ActionRef::parse("docker://alpine:3.18").unwrap(),
            ActionRef::DockerImage {
                image: "alpine:3.18".into()
            }
        );
    }

    #[test]
    fn a_digest_pinned_image_keeps_its_digest() {
        // The `@` here belongs to the image, not to a git ref.
        assert_eq!(
            ActionRef::parse("docker://ghcr.io/owner/image@sha256:abc123").unwrap(),
            ActionRef::DockerImage {
                image: "ghcr.io/owner/image@sha256:abc123".into()
            }
        );
        assert_eq!(
            registry_name("docker://ghcr.io/owner/image@sha256:abc123"),
            "docker://ghcr.io/owner/image"
        );
    }

    #[test]
    fn rejects_references_that_cannot_be_resolved() {
        // No ref at all: this is the shape that used to reach the registry and
        // fail with a confusing "action not found".
        assert!(ActionRef::parse("actions/checkout").is_err());
        assert!(ActionRef::parse("actions/checkout@").is_err());
        assert!(ActionRef::parse("checkout@v4").is_err());
        assert!(ActionRef::parse("@v4").is_err());
        assert!(ActionRef::parse("").is_err());
        assert!(ActionRef::parse("docker://").is_err());
    }

    #[test]
    fn rejects_paths_that_escape() {
        assert!(ActionRef::parse("./../evil").is_err());
        assert!(ActionRef::parse("owner/repo/../../etc@v1").is_err());
        assert!(ActionRef::parse("./local@v1").is_err());
    }

    #[test]
    fn registry_name_drops_the_version() {
        assert_eq!(registry_name("actions/checkout@v4"), "actions/checkout");
        assert_eq!(registry_name("actions/checkout"), "actions/checkout");
        assert_eq!(registry_name(" my/action@v1 "), "my/action");
    }
}
