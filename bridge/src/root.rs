use std::collections::VecDeque;
use std::fmt;
use std::path::{Component, Path, PathBuf};

use crate::json::Value;

const ROOT_MESSAGE: &str =
    "godot-bridge: cannot determine a local worktree root from initialize params";

#[derive(Debug)]
pub enum RootError {
    CannotDetermineRoot,
    UncPath(PathBuf),
    ProjectDirInvalid(PathBuf),
    FileOutsideRoot(PathBuf),
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
            Self::UncPath(path) => write!(
                f,
                "network path {} is not supported; use a local drive",
                path.display()
            ),
            Self::ProjectDirInvalid(path) => {
                write!(f, "project_dir {} has no project.godot", path.display())
            }
            Self::FileOutsideRoot(path) => {
                write!(f, "{} is outside the worktree", path.display())
            }
            Self::NoProject(root) => write!(
                f,
                "No project.godot found under {}. Set the project_dir setting.",
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
                    "Several Godot projects under {}: {}. Set the project_dir setting.",
                    root.display(),
                    paths
                )
            }
        }
    }
}

impl std::error::Error for RootError {}

/// Resolve a path against the file system, then remove the `\?\` prefix
/// `canonicalize` adds on Windows so hashes, display and URIs all agree, and
/// refuse a network path the rest of the bridge cannot address.
pub fn canonicalize(path: &Path) -> Result<PathBuf, RootError> {
    let canonical = path
        .canonicalize()
        .map_err(|_| RootError::CannotDetermineRoot)?;
    strip_verbatim(canonical)
}

#[cfg(unix)]
pub fn strip_verbatim(path: PathBuf) -> Result<PathBuf, RootError> {
    Ok(path)
}

#[cfg(windows)]
pub fn strip_verbatim(path: PathBuf) -> Result<PathBuf, RootError> {
    use std::path::Prefix;
    let mut components = path.components();
    let Some(Component::Prefix(prefix)) = components.next() else {
        return Ok(path);
    };
    let letter = match prefix.kind() {
        Prefix::VerbatimDisk(letter) => letter,
        Prefix::Disk(_) => return Ok(path),
        _ => return Err(RootError::UncPath(path)),
    };
    let mut stripped = PathBuf::from(format!("{}:\\", char::from(letter.to_ascii_uppercase())));
    for component in components {
        if component != Component::RootDir {
            stripped.push(component.as_os_str());
        }
    }
    Ok(stripped)
}

#[cfg(unix)]
pub fn paths_equal(left: &Path, right: &Path) -> bool {
    left.as_os_str() == right.as_os_str()
}

#[cfg(windows)]
pub fn paths_equal(left: &Path, right: &Path) -> bool {
    left.as_os_str()
        .to_string_lossy()
        .eq_ignore_ascii_case(&right.as_os_str().to_string_lossy())
}

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
                if let Ok(path) = uri_to_path(uri) {
                    return canonicalize(&path);
                }
            }
        }
        return Err(RootError::CannotDetermineRoot);
    }

    if let Some(root_uri) = params.get("rootUri").and_then(Value::as_str) {
        return uri_to_path(root_uri).and_then(|path| canonicalize(&path));
    }
    if let Some(root_path) = params.get("rootPath").and_then(Value::as_str) {
        return canonicalize(Path::new(root_path));
    }
    cwd_root()
}

pub fn cwd_root() -> Result<PathBuf, RootError> {
    let cwd = std::env::current_dir().map_err(|_| RootError::CannotDetermineRoot)?;
    canonicalize(&cwd)
}

pub fn find_project_dir(
    root: &Path,
    file: Option<&Path>,
    configured: Option<&Path>,
) -> Result<PathBuf, RootError> {
    let root = canonicalize(root)?;

    if let Some(file) = file {
        let file = canonical_or_normalized(file);
        if !file.starts_with(&root) {
            return Err(RootError::FileOutsideRoot(file));
        }
        let mut candidate = if file.is_dir() {
            file
        } else {
            file.parent()
                .unwrap_or_else(|| Path::new("/"))
                .to_path_buf()
        };
        loop {
            if has_project_file(&candidate) {
                let dir = canonical_dir(&candidate)?;
                if dir.starts_with(&root) {
                    return Ok(dir);
                }
                break;
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
                crate::warn!("skipping unreadable directory {}: {error}", dir.display());
                continue;
            }
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let file_type = match entry.file_type() {
                Ok(file_type) => file_type,
                Err(error) => {
                    crate::warn!(
                        "skipping unreadable directory entry {}: {error}",
                        path.display()
                    );
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

pub fn canonical_path_to_uri(path: &Path) -> String {
    crate::file_uri::path_to_uri(&canonical_or_normalized(path))
}

pub fn uri_to_path(uri: &str) -> Result<PathBuf, RootError> {
    Ok(normalize_absolute(
        &crate::file_uri::uri_to_path(uri).ok_or(RootError::CannotDetermineRoot)?,
    ))
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
    canonicalize(path)
}

fn should_skip(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return true;
    };
    matches!(name, "addons" | "node_modules" | "target") || name.starts_with('.')
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
    canonicalize(path).unwrap_or_else(|_| normalize_absolute(path))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_fixture_projects() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../fixtures");
        let root = canonicalize(&root).unwrap();
        assert!(find_project_dir(&root.join("nested"), None, None)
            .unwrap()
            .ends_with(Path::new("nested").join("repo").join("game")));
        assert!(find_project_dir(&root.join("minimal-project"), None, None)
            .unwrap()
            .ends_with("minimal-project"));
    }

    fn scratch_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("godot-bridge-root-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn file_outside_root_is_an_error() {
        let base = scratch_dir("outside");
        let root = base.join("root");
        let other = base.join("other");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&other).unwrap();
        std::fs::write(other.join("project.godot"), "").unwrap();
        std::fs::write(other.join("main.gd"), "").unwrap();
        let error = find_project_dir(&root, Some(&other.join("main.gd")), None).unwrap_err();
        assert!(matches!(error, RootError::FileOutsideRoot(_)), "{error}");
    }

    #[cfg(unix)]
    #[test]
    fn symlink_to_project_outside_root_is_an_error() {
        let base = scratch_dir("symlink");
        let root = base.join("root");
        let other = base.join("other");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&other).unwrap();
        std::fs::write(other.join("project.godot"), "").unwrap();
        std::fs::write(other.join("main.gd"), "").unwrap();
        std::os::unix::fs::symlink(&other, root.join("link")).unwrap();
        let error =
            find_project_dir(&root, Some(&root.join("link").join("main.gd")), None).unwrap_err();
        assert!(matches!(error, RootError::FileOutsideRoot(_)), "{error}");
    }

    #[cfg(unix)]
    #[test]
    fn paths_compare_byte_for_byte() {
        assert!(paths_equal(Path::new("/project"), Path::new("/project")));
        assert!(!paths_equal(Path::new("/Project"), Path::new("/project")));
        assert!(!paths_equal(Path::new("/project/"), Path::new("/project")));
    }

    #[cfg(unix)]
    #[test]
    fn encodes_and_decodes_spaces() {
        let path = PathBuf::from("/tmp/a space/project.godot");
        let uri = canonical_path_to_uri(&path);
        assert!(uri.contains("a%20space"));
        assert_eq!(uri_to_path(&uri).unwrap(), path);
    }

    #[cfg(windows)]
    #[test]
    fn encodes_and_decodes_spaces() {
        let path = PathBuf::from(r"C:\tmp\a space\project.godot");
        let uri = canonical_path_to_uri(&path);
        assert!(uri.starts_with("file:///C:/"), "{uri}");
        assert!(uri.contains("a%20space"), "{uri}");
        assert_eq!(uri_to_path(&uri).unwrap(), path);
    }

    #[cfg(windows)]
    #[test]
    fn verbatim_prefix_is_stripped_and_unc_is_refused() {
        assert_eq!(
            strip_verbatim(PathBuf::from(r"\\?\C:\Users\me\proj")).unwrap(),
            PathBuf::from(r"C:\Users\me\proj")
        );
        assert_eq!(
            strip_verbatim(PathBuf::from(r"\\?\c:\")).unwrap(),
            PathBuf::from(r"C:\")
        );
        assert_eq!(
            strip_verbatim(PathBuf::from(r"C:\Users\me")).unwrap(),
            PathBuf::from(r"C:\Users\me")
        );
        assert!(matches!(
            strip_verbatim(PathBuf::from(r"\\server\share\proj")),
            Err(RootError::UncPath(_))
        ));
        assert!(matches!(
            strip_verbatim(PathBuf::from(r"\\?\UNC\server\share")),
            Err(RootError::UncPath(_))
        ));
    }

    #[cfg(windows)]
    #[test]
    fn canonicalize_returns_a_drive_letter_path() {
        let root = canonicalize(Path::new(env!("CARGO_MANIFEST_DIR"))).unwrap();
        let text = root.to_string_lossy().into_owned();
        assert!(!text.starts_with(r"\\?\"), "{text}");
        assert!(text.as_bytes()[1] == b':', "{text}");
    }

    #[cfg(windows)]
    #[test]
    fn paths_compare_without_regard_to_case() {
        assert!(paths_equal(
            Path::new(r"C:\Users\Me\Proj"),
            Path::new(r"c:\users\me\proj")
        ));
        assert!(!paths_equal(
            Path::new(r"C:\Users\Me\Proj"),
            Path::new(r"C:\Users\Me\Other")
        ));
    }
}
