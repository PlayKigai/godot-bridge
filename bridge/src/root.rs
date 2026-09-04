use std::collections::VecDeque;
use std::fmt;
use std::path::{Component, Path, PathBuf};

use percent_encoding::percent_decode_str;
use serde_json::Value;
use url::Url;

const ROOT_MESSAGE: &str =
    "godot-bridge: cannot determine a local worktree root from initialize params";

#[derive(Debug)]
pub enum RootError {
    CannotDetermineRoot,
    ProjectDirInvalid(PathBuf),
    NoProject(PathBuf),
    SeveralProjects {
        root: PathBuf,
        projects: Vec<PathBuf>,
    },
}

impl RootError {
    pub fn lsp_code(&self) -> i64 {
        -32002
    }
}

impl fmt::Display for RootError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CannotDetermineRoot => f.write_str(ROOT_MESSAGE),
            Self::ProjectDirInvalid(path) => {
                write!(f, "project_dir {} has no project.godot", path.display())
            }
            Self::NoProject(root) => write!(
                f,
                "No project.godot found under {}. Set lsp.godot.settings.project_dir.",
                root.display()
            ),
            Self::SeveralProjects { root, projects } => {
                let paths = projects
                    .iter()
                    .map(|path| path.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ");
                write!(
                    f,
                    "Several Godot projects under {}: {}. Set lsp.godot.settings.project_dir.",
                    root.display(),
                    paths
                )
            }
        }
    }
}

impl std::error::Error for RootError {}

pub fn worktree_root_from_initialize(params: &Value) -> Result<PathBuf, RootError> {
    if let Some(workspaces) = params.get("workspaceFolders") {
        let Some(workspaces) = workspaces.as_array() else {
            return Err(RootError::CannotDetermineRoot);
        };
        for workspace in workspaces {
            let uri = workspace
                .as_str()
                .or_else(|| workspace.get("uri").and_then(Value::as_str));
            if let Some(uri) = uri {
                if Url::parse(uri)
                    .map(|parsed| parsed.scheme().eq_ignore_ascii_case("file"))
                    .unwrap_or(false)
                {
                    if let Ok(path) = uri_to_path(uri) {
                        return path
                            .canonicalize()
                            .map_err(|_| RootError::CannotDetermineRoot);
                    }
                }
            }
        }
        return Err(RootError::CannotDetermineRoot);
    }

    if let Some(root_uri) = params.get("rootUri").and_then(Value::as_str) {
        return uri_to_path(root_uri).and_then(|path| {
            path.canonicalize()
                .map_err(|_| RootError::CannotDetermineRoot)
        });
    }
    if let Some(root_path) = params.get("rootPath").and_then(Value::as_str) {
        return Path::new(root_path)
            .canonicalize()
            .map_err(|_| RootError::CannotDetermineRoot);
    }
    cwd_root()
}

pub fn cwd_root() -> Result<PathBuf, RootError> {
    std::env::current_dir()
        .and_then(|path| path.canonicalize())
        .map_err(|_| RootError::CannotDetermineRoot)
}

pub fn find_project_dir(
    root: &Path,
    file: Option<&Path>,
    configured: Option<&Path>,
) -> Result<PathBuf, RootError> {
    let root = root
        .canonicalize()
        .map_err(|_| RootError::CannotDetermineRoot)?;

    if let Some(file) = file {
        let file = normalize_absolute(file);
        let mut candidate = if file.is_dir() {
            file
        } else {
            file.parent()
                .unwrap_or_else(|| Path::new("/"))
                .to_path_buf()
        };
        loop {
            if has_project_file(&candidate) {
                return canonical_dir(&candidate);
            }
            let Some(parent) = candidate.parent() else {
                break;
            };
            if parent == candidate {
                break;
            }
            candidate = parent.to_path_buf();
        }
    }

    if let Some(configured) = configured {
        let candidate = if configured.is_absolute() {
            configured.to_path_buf()
        } else {
            root.join(configured)
        };
        if has_project_file(&candidate) {
            return canonical_dir(&candidate);
        }
        return Err(RootError::ProjectDirInvalid(candidate));
    }

    if has_project_file(&root) {
        return Ok(root.clone());
    }

    let mut queue = VecDeque::from([(root.clone(), 0usize)]);
    let mut projects = Vec::new();
    while let Some((dir, depth)) = queue.pop_front() {
        if depth == 3 {
            continue;
        }
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(error) => {
                tracing::warn!(path = %dir.display(), %error, "skipping unreadable directory");
                continue;
            }
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let file_type = match entry.file_type() {
                Ok(file_type) => file_type,
                Err(error) => {
                    tracing::warn!(path = %path.display(), %error, "skipping unreadable directory entry");
                    continue;
                }
            };
            if !file_type.is_dir() || file_type.is_symlink() || should_skip(&path) {
                continue;
            }
            if has_project_file(&path) {
                if let Ok(path) = canonical_dir(&path) {
                    projects.push(path);
                }
            }
            queue.push_back((path, depth + 1));
        }
    }
    projects.sort();
    projects.dedup();
    match projects.len() {
        0 => Err(RootError::NoProject(root)),
        1 => Ok(projects.remove(0)),
        _ => Err(RootError::SeveralProjects { root, projects }),
    }
}

pub fn path_to_uri(path: &Path) -> String {
    let path = canonical_or_normalized(path);
    Url::from_file_path(path)
        .expect("absolute paths can be represented as file URIs")
        .to_string()
}

pub fn uri_to_path(uri: &str) -> Result<PathBuf, RootError> {
    let parsed = Url::parse(uri).map_err(|_| RootError::CannotDetermineRoot)?;
    if !parsed.scheme().eq_ignore_ascii_case("file")
        || parsed
            .host_str()
            .is_some_and(|host| !host.eq_ignore_ascii_case("localhost"))
    {
        return Err(RootError::CannotDetermineRoot);
    }
    let decoded = percent_decode_str(parsed.path())
        .decode_utf8()
        .map_err(|_| RootError::CannotDetermineRoot)?;
    let path = normalize_absolute(Path::new(decoded.as_ref()));
    if path.is_absolute() {
        Ok(path)
    } else {
        Err(RootError::CannotDetermineRoot)
    }
}

pub fn doc_key(uri_or_path: &str) -> PathBuf {
    let path = if uri_or_path.to_ascii_lowercase().starts_with("file:") {
        uri_to_path(uri_or_path).unwrap_or_else(|_| PathBuf::from(uri_or_path))
    } else {
        PathBuf::from(uri_or_path)
    };
    canonical_or_normalized(&path)
}

fn has_project_file(path: &Path) -> bool {
    path.is_dir() && path.join("project.godot").is_file()
}

fn canonical_dir(path: &Path) -> Result<PathBuf, RootError> {
    path.canonicalize()
        .map_err(|_| RootError::CannotDetermineRoot)
}

fn should_skip(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return true;
    };
    matches!(
        name,
        ".git" | ".godot" | "addons" | "node_modules" | "target"
    ) || name.starts_with('.')
}

pub fn normalize_absolute(path: &Path) -> PathBuf {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("/"))
            .join(path)
    };
    let mut normalized = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            component => normalized.push(component.as_os_str()),
        }
    }
    normalized
}

pub fn canonical_or_normalized(path: &Path) -> PathBuf {
    path.canonicalize()
        .unwrap_or_else(|_| normalize_absolute(path))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_fixture_projects() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../fixtures");
        let root = root.canonicalize().unwrap();
        assert!(find_project_dir(&root.join("nested"), None, None)
            .unwrap()
            .ends_with("nested/repo/game"));
        assert!(find_project_dir(&root.join("minimal-project"), None, None)
            .unwrap()
            .ends_with("minimal-project"));
    }

    #[test]
    fn encodes_and_decodes_spaces() {
        let path = PathBuf::from("/tmp/a space/project.godot");
        let uri = path_to_uri(&path);
        assert!(uri.contains("a%20space"));
        assert_eq!(uri_to_path(&uri).unwrap(), path);
    }
}
