//! Browsing what `actions/upload-artifact` left behind.
//!
//! Artifacts land in `<workspace>/.minact-artifacts/<name>/`, keyed by name
//! rather than by run — uploading the same name twice overwrites, exactly as
//! the action behaves. So this is the current contents of that directory, not
//! a per-run archive.

use std::path::{Component, Path, PathBuf};

use serde::Serialize;

use crate::error::ApiError;

pub const DIRECTORY: &str = ".minact-artifacts";

#[derive(Debug, Serialize)]
pub struct ArtifactDto {
    pub name: String,
    pub file_count: usize,
    pub total_bytes: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub modified: Option<chrono::DateTime<chrono::Utc>>,
    pub files: Vec<ArtifactFileDto>,
}

#[derive(Debug, Serialize)]
pub struct ArtifactFileDto {
    /// Path relative to the artifact's own directory, always `/`-separated.
    pub path: String,
    pub bytes: u64,
    /// True when the contents are worth showing inline.
    pub previewable: bool,
}

/// Every artifact in the workspace, newest first.
pub fn list(workspace: &Path) -> Vec<ArtifactDto> {
    let root = workspace.join(DIRECTORY);
    let Ok(entries) = std::fs::read_dir(&root) else {
        return Vec::new();
    };

    let mut artifacts: Vec<ArtifactDto> = entries
        .flatten()
        .filter(|entry| entry.path().is_dir())
        .filter_map(|entry| {
            let name = entry.file_name().to_str()?.to_string();
            let mut files = Vec::new();
            collect(&entry.path(), &entry.path(), &mut files);
            files.sort_by(|a, b| a.path.cmp(&b.path));

            Some(ArtifactDto {
                name,
                file_count: files.len(),
                total_bytes: files.iter().map(|file| file.bytes).sum(),
                modified: entry
                    .metadata()
                    .and_then(|meta| meta.modified())
                    .ok()
                    .map(chrono::DateTime::<chrono::Utc>::from),
                files,
            })
        })
        .collect();

    artifacts.sort_by(|a, b| {
        b.modified
            .cmp(&a.modified)
            .then_with(|| a.name.cmp(&b.name))
    });
    artifacts
}

/// Resolve one file inside an artifact, refusing to leave its directory.
pub fn resolve(workspace: &Path, name: &str, rel_path: &str) -> Result<PathBuf, ApiError> {
    let root = workspace.join(DIRECTORY);

    // The name is a single directory, never a path. `..` and separators here
    // would escape the artifact store before the canonical check even runs.
    if name.is_empty() || name.contains('/') || name.contains('\\') || name.contains("..") {
        return Err(ApiError::BadRequest(format!(
            "'{}' is not an artifact name",
            name
        )));
    }

    let artifact_root = root.join(name);
    let requested = artifact_root.join(rel_path);

    // Symlinks inside an artifact can point anywhere, so compare the resolved
    // paths rather than the requested ones.
    let canonical_root = artifact_root
        .canonicalize()
        .map_err(|_| ApiError::NotFound(format!("No artifact named '{}'", name)))?;
    let canonical = requested
        .canonicalize()
        .map_err(|_| ApiError::NotFound(format!("'{}' is not in artifact '{}'", rel_path, name)))?;

    if !canonical.starts_with(&canonical_root) || !canonical.is_file() {
        return Err(ApiError::NotFound(format!(
            "'{}' is not in artifact '{}'",
            rel_path, name
        )));
    }

    Ok(canonical)
}

fn collect(root: &Path, dir: &Path, out: &mut Vec<ArtifactFileDto>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect(root, &path, out);
            continue;
        }

        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        let Some(relative) = relative_to(root, &path) else {
            continue;
        };

        out.push(ArtifactFileDto {
            previewable: is_previewable(&path, metadata.len()),
            path: relative,
            bytes: metadata.len(),
        });
    }
}

fn relative_to(root: &Path, path: &Path) -> Option<String> {
    let relative = path.strip_prefix(root).ok()?;
    Some(
        relative
            .components()
            .filter_map(|component| match component {
                Component::Normal(part) => Some(part.to_string_lossy().into_owned()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("/"),
    )
}

/// Text small enough to render in the browser without downloading it.
fn is_previewable(path: &Path, bytes: u64) -> bool {
    const PREVIEW_LIMIT: u64 = 256 * 1024;

    if bytes > PREVIEW_LIMIT {
        return false;
    }
    let mime = mime_guess::from_path(path).first_or_octet_stream();
    mime.type_() == "text"
        || matches!(
            mime.essence_str(),
            "application/json" | "application/xml" | "application/javascript"
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn workspace_with_artifacts() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        let build = tmp.path().join(DIRECTORY).join("build-output");
        std::fs::create_dir_all(build.join("nested")).unwrap();
        std::fs::write(build.join("app.js"), "console.log(1)").unwrap();
        std::fs::write(build.join("nested/readme.txt"), "hello").unwrap();

        let results = tmp.path().join(DIRECTORY).join("test-results");
        std::fs::create_dir_all(&results).unwrap();
        std::fs::write(results.join("junit.xml"), "<testsuite/>").unwrap();
        tmp
    }

    #[test]
    fn lists_artifacts_with_their_files_and_sizes() {
        let tmp = workspace_with_artifacts();
        let artifacts = list(tmp.path());

        assert_eq!(artifacts.len(), 2);

        let build = artifacts
            .iter()
            .find(|artifact| artifact.name == "build-output")
            .expect("build-output");
        assert_eq!(build.file_count, 2);
        assert_eq!(build.total_bytes, "console.log(1)".len() as u64 + 5);

        let paths: Vec<&str> = build.files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(paths, vec!["app.js", "nested/readme.txt"]);
        assert!(build.files.iter().all(|file| file.previewable));
    }

    #[test]
    fn resolves_a_file_inside_the_artifact() {
        let tmp = workspace_with_artifacts();
        let path = resolve(tmp.path(), "build-output", "nested/readme.txt").unwrap();
        assert_eq!(std::fs::read_to_string(path).unwrap(), "hello");
    }

    #[test]
    fn refuses_to_escape_the_artifact_directory() {
        let tmp = workspace_with_artifacts();
        std::fs::write(tmp.path().join("secret.txt"), "nope").unwrap();

        for (name, path) in [
            ("build-output", "../../secret.txt"),
            ("build-output", "../test-results/junit.xml"),
            ("..", "secret.txt"),
            ("../..", "etc/passwd"),
        ] {
            assert!(
                resolve(tmp.path(), name, path).is_err(),
                "{}/{} should have been refused",
                name,
                path,
            );
        }
    }

    #[test]
    fn refuses_a_symlink_that_points_outside() {
        let tmp = workspace_with_artifacts();
        std::fs::write(tmp.path().join("secret.txt"), "nope").unwrap();

        #[cfg(unix)]
        {
            let link = tmp.path().join(DIRECTORY).join("build-output").join("out");
            std::os::unix::fs::symlink(tmp.path().join("secret.txt"), &link).unwrap();
            assert!(resolve(tmp.path(), "build-output", "out").is_err());
        }
    }

    #[test]
    fn an_empty_workspace_has_no_artifacts() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(list(tmp.path()).is_empty());
    }
}
