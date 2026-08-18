//! Workflow discovery that keeps hold of parse failures.
//!
//! [`minact_core::WorkflowParser::discover_workflows`] drops unparseable files
//! with a `tracing::warn!`, which is the right call for the CLI but wrong for
//! Studio — a broken workflow is exactly what the user opened the UI to look
//! at. This walks the same locations and returns the errors alongside the
//! workflows.
//!
//! The locations come from [`WorkflowParser::default_search_paths`] rather
//! than being restated here, so the two cannot drift. Extra directories come
//! from the caller, which is how `--workflows examples/` reaches a directory
//! minact would never search on its own.

use std::path::{Path, PathBuf};

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use minact_core::workflow::Workflow;
use minact_core::{SearchPath, WorkflowParser};

/// A workflow file found in the workspace, parsed or not.
#[derive(Debug)]
pub struct Discovered {
    /// Opaque, URL-safe identifier derived from [`Discovered::rel_path`].
    pub id: String,
    /// Path relative to the workspace root where possible, always with `/`
    /// separators. An extra directory outside the workspace keeps its
    /// absolute path.
    pub rel_path: String,
    pub abs_path: PathBuf,
    /// Where it came from, as the UI labels it: `.github`, `examples/`, …
    pub source: String,
    pub parsed: Result<Workflow, String>,
}

/// Encode a workspace-relative path as an opaque URL path segment.
///
/// Paths contain `/`, which no router will hand back intact as a single
/// parameter, so ids are base64url rather than the path itself.
pub fn encode_id(rel_path: &str) -> String {
    URL_SAFE_NO_PAD.encode(rel_path.as_bytes())
}

/// Inverse of [`encode_id`]. Rejects anything that is not valid UTF-8.
pub fn decode_id(id: &str) -> Option<String> {
    let bytes = URL_SAFE_NO_PAD.decode(id.as_bytes()).ok()?;
    String::from_utf8(bytes).ok()
}

/// Find every workflow file in `workspace`, in the same order core searches,
/// followed by any `extra_dirs` the caller asked for.
pub fn discover(workspace: &Path, extra_dirs: &[PathBuf]) -> Vec<Discovered> {
    let mut found = Vec::new();

    // Walking core's list keeps both the set of locations and their order in
    // one place.
    for search_path in WorkflowParser::default_search_paths() {
        match &search_path {
            SearchPath::Directory(dir) => {
                collect_dir(workspace, &workspace.join(dir), label(dir), &mut found);
            }
            SearchPath::File(file) => {
                let path = workspace.join(file);
                if path.is_file() {
                    found.push(load(workspace, &path, "root".to_string()));
                }
            }
        }
    }

    for dir in extra_dirs {
        let resolved = if dir.is_absolute() {
            dir.clone()
        } else {
            workspace.join(dir)
        };
        let name = relative_to(workspace, &resolved);
        collect_dir(workspace, &resolved, format!("{}/", name), &mut found);
    }

    // The same directory can be reached twice — `--workflows .github/workflows`
    // on top of the default, say. First hit wins, so the built-in label stays.
    let mut seen = std::collections::HashSet::new();
    found.retain(|item| seen.insert(item.abs_path.clone()));

    found
}

/// Find a single workflow by its [`encode_id`] identifier.
pub fn find(workspace: &Path, extra_dirs: &[PathBuf], id: &str) -> Option<Discovered> {
    let rel = decode_id(id)?;
    // Resolve through discovery rather than joining the decoded path onto the
    // workspace: an id is only valid if it names a file we would have found
    // anyway, which keeps `../` out of the lookup entirely.
    discover(workspace, extra_dirs)
        .into_iter()
        .find(|d| d.rel_path == rel)
}

/// `.github/workflows` reads as `.github` in the UI — the parent is what
/// identifies the convention.
fn label(dir: &str) -> String {
    dir.split('/').next().unwrap_or(dir).to_string()
}

fn collect_dir(workspace: &Path, dir: &Path, source: String, out: &mut Vec<Discovered>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };

    let mut paths: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.is_file()
                && matches!(
                    path.extension().and_then(|ext| ext.to_str()),
                    Some("yml") | Some("yaml")
                )
        })
        .collect();

    // read_dir order is filesystem-defined; the UI wants a stable list.
    paths.sort();

    out.extend(
        paths
            .iter()
            .map(|path| load(workspace, path, source.clone())),
    );
}

fn load(workspace: &Path, path: &Path, source: String) -> Discovered {
    let rel_path = relative_to(workspace, path);
    Discovered {
        id: encode_id(&rel_path),
        rel_path,
        abs_path: path.to_path_buf(),
        source,
        parsed: WorkflowParser::parse_file(path).map_err(|err| err.to_string()),
    }
}

fn relative_to(workspace: &Path, path: &Path) -> String {
    let rel = path.strip_prefix(workspace).unwrap_or(path);
    rel.components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("/")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id_round_trips_a_nested_path() {
        let id = encode_id(".github/workflows/ci.yml");
        assert!(!id.contains('/'));
        assert_eq!(decode_id(&id).as_deref(), Some(".github/workflows/ci.yml"));
    }

    #[test]
    fn decode_rejects_garbage() {
        assert_eq!(decode_id("not/valid/base64"), None);
    }

    fn valid(name: &str) -> String {
        format!(
            "name: {}\non: push\njobs:\n  build:\n    steps:\n      - run: echo hi\n",
            name
        )
    }

    #[test]
    fn discovers_across_all_locations_and_keeps_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();

        let minact = root.join(".minact/workflows");
        std::fs::create_dir_all(&minact).unwrap();
        std::fs::write(minact.join("ci.yml"), valid("CI")).unwrap();
        std::fs::write(minact.join("broken.yml"), "name: Broken\njobs: [\n").unwrap();

        let github = root.join(".github/workflows");
        std::fs::create_dir_all(&github).unwrap();
        std::fs::write(github.join("release.yaml"), valid("Release")).unwrap();

        let found = discover(root, &[]);
        let paths: Vec<&str> = found.iter().map(|d| d.rel_path.as_str()).collect();
        assert_eq!(
            paths,
            vec![
                ".minact/workflows/broken.yml",
                ".minact/workflows/ci.yml",
                ".github/workflows/release.yaml",
            ]
        );

        let broken = &found[0];
        assert!(broken.parsed.is_err());
        assert_eq!(broken.source, ".minact");

        let ci = &found[1];
        assert_eq!(ci.parsed.as_ref().unwrap().name, "CI");

        // A round-tripped id resolves back to the same file.
        let again = find(root, &[], &ci.id).unwrap();
        assert_eq!(again.rel_path, ci.rel_path);
    }

    #[test]
    fn extra_directories_are_searched_after_the_defaults() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();

        let minact = root.join(".minact/workflows");
        std::fs::create_dir_all(&minact).unwrap();
        std::fs::write(minact.join("ci.yml"), valid("CI")).unwrap();

        let examples = root.join("examples");
        std::fs::create_dir_all(&examples).unwrap();
        std::fs::write(examples.join("matrix.yml"), valid("Matrix")).unwrap();
        std::fs::write(examples.join("outputs.yml"), valid("Outputs")).unwrap();
        // Not a workflow; must not be picked up.
        std::fs::write(examples.join("notes.md"), "hello").unwrap();

        let found = discover(root, &[PathBuf::from("examples")]);
        let paths: Vec<&str> = found.iter().map(|d| d.rel_path.as_str()).collect();
        assert_eq!(
            paths,
            vec![
                ".minact/workflows/ci.yml",
                "examples/matrix.yml",
                "examples/outputs.yml",
            ]
        );

        // The label says where it came from, so the list stays readable when
        // several directories are mounted.
        assert_eq!(found[1].source, "examples/");

        // And an id from an extra directory resolves like any other.
        let matrix = find(root, &[PathBuf::from("examples")], &found[1].id).unwrap();
        assert_eq!(matrix.parsed.unwrap().name, "Matrix");
    }

    #[test]
    fn a_directory_mounted_twice_appears_once() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();

        let github = root.join(".github/workflows");
        std::fs::create_dir_all(&github).unwrap();
        std::fs::write(github.join("ci.yml"), valid("CI")).unwrap();

        let found = discover(root, &[PathBuf::from(".github/workflows")]);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].source, ".github", "the built-in label wins");
    }

    #[test]
    fn an_absolute_extra_directory_outside_the_workspace_works() {
        let workspace = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        std::fs::write(elsewhere.path().join("shared.yml"), valid("Shared")).unwrap();

        let found = discover(workspace.path(), &[elsewhere.path().to_path_buf()]);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].parsed.as_ref().unwrap().name, "Shared");
        // Outside the workspace, so the path stays absolute rather than
        // pretending to be relative to it.
        assert!(found[0].rel_path.ends_with("shared.yml"));
        assert!(found[0].abs_path.is_absolute());
    }

    #[test]
    fn find_rejects_traversal() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(find(tmp.path(), &[], &encode_id("../../etc/passwd")).is_none());
    }
}
