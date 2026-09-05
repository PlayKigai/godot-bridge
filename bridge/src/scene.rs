use std::fmt;
use std::io::Read;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use crate::root::normalize_absolute;

const MAX_SCENE_BYTES: u64 = 4 * 1024 * 1024;

#[derive(Debug, PartialEq, Eq)]
pub enum SceneError {
    NoScene(PathBuf),
    OutsideProject(PathBuf),
}

impl fmt::Display for SceneError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoScene(file) => write!(formatter, "No scene uses {}", file.display()),
            Self::OutsideProject(file) => {
                write!(formatter, "File {} is outside project", file.display())
            }
        }
    }
}

impl std::error::Error for SceneError {}

pub fn resolve_scene(project: &Path, file: &Path) -> Result<String, SceneError> {
    let project = normalize_absolute(project);
    let file = normalize_absolute(file);
    let relative_file = file
        .strip_prefix(&project)
        .map_err(|_| SceneError::OutsideProject(file.clone()))?;
    let relative_file = relative_file.to_path_buf();

    if relative_file
        .extension()
        .is_some_and(|extension| extension == "tscn")
    {
        return Ok(res_path(&relative_file));
    }

    let adjacent = project.join(relative_file.with_extension("tscn"));
    if adjacent.is_file() {
        return Ok(res_path(&relative_file.with_extension("tscn")));
    }

    let expected_script = res_path(&relative_file);
    let mut scenes = Vec::new();
    collect_scenes(&project, &project, &mut scenes);
    scenes.sort_by(|left, right| left.0.cmp(&right.0));

    for (relative_scene, scene) in scenes {
        let Some(contents) = read_scene(&scene) else {
            continue;
        };
        if contents.lines().any(|line| {
            line.trim_start().starts_with("[ext_resource")
                && ext_resource_path(line)
                    .is_some_and(|path| normalize_res_path(&path) == expected_script)
        }) {
            return Ok(res_path(&relative_scene));
        }
    }

    Err(SceneError::NoScene(file))
}

fn read_scene(path: &Path) -> Option<String> {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .ok()?;
    let mut bytes = Vec::new();
    file.take(MAX_SCENE_BYTES)
        .read_to_end(&mut bytes)
        .ok()
        .and_then(|_| String::from_utf8(bytes).ok())
}

fn collect_scenes(root: &Path, directory: &Path, scenes: &mut Vec<(PathBuf, PathBuf)>) {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_symlink() {
            continue;
        }
        if file_type.is_dir() {
            if !should_skip_directory(&path) {
                collect_scenes(root, &path, scenes);
            }
        } else if file_type.is_file()
            && path
                .extension()
                .is_some_and(|extension| extension == "tscn")
        {
            if let Ok(relative) = path.strip_prefix(root) {
                scenes.push((relative.to_path_buf(), path));
            }
        }
    }
}

fn should_skip_directory(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return true;
    };
    name == ".godot" || name == "addons" || name.starts_with('.')
}

fn ext_resource_path(line: &str) -> Option<String> {
    let start = line.find("path=\"")? + 6;
    let end = line[start..].find('"')? + start;
    Some(line[start..end].to_owned())
}

fn res_path(path: &Path) -> String {
    let path = path.to_string_lossy().replace('\\', "/");
    normalize_res_path(&format!("res://{path}"))
}

fn normalize_res_path(path: &str) -> String {
    let mut parts = Vec::new();
    for part in path.strip_prefix("res://").unwrap_or(path).split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            part => parts.push(part),
        }
    }
    format!("res://{}", parts.join("/"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::temp::tempdir;
    use std::fs;

    fn script(project: &Path, relative: &str) -> PathBuf {
        let file = project.join(relative);
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(&file, "").unwrap();
        file
    }

    #[test]
    fn resolves_adjacent_stem() {
        let directory = tempdir().unwrap();
        let file = script(directory.path(), "scripts/player.gd");
        fs::write(directory.path().join("scripts/player.tscn"), "").unwrap();
        assert_eq!(
            resolve_scene(directory.path(), &file).unwrap(),
            "res://scripts/player.tscn"
        );
    }

    #[test]
    fn resolves_ext_resource_reference() {
        let directory = tempdir().unwrap();
        let file = script(directory.path(), "scripts/player.gd");
        fs::create_dir_all(directory.path().join("levels")).unwrap();
        fs::write(
            directory.path().join("levels/level.tscn"),
            "[ext_resource type=\"Script\" path=\"res://scripts/player.gd\" id=\"1\"]",
        )
        .unwrap();
        assert_eq!(
            resolve_scene(directory.path(), &file).unwrap(),
            "res://levels/level.tscn"
        );
    }

    #[test]
    fn resolves_lexicographically_first_reference() {
        let directory = tempdir().unwrap();
        let file = script(directory.path(), "scripts/player.gd");
        fs::create_dir_all(directory.path().join("z")).unwrap();
        fs::create_dir_all(directory.path().join("a")).unwrap();
        let contents = "[ext_resource path=\"res://scripts/player.gd\"]";
        fs::write(directory.path().join("z/scene.tscn"), contents).unwrap();
        fs::write(directory.path().join("a/scene.tscn"), contents).unwrap();
        assert_eq!(
            resolve_scene(directory.path(), &file).unwrap(),
            "res://a/scene.tscn"
        );
    }

    #[test]
    fn reports_no_scene() {
        let directory = tempdir().unwrap();
        let file = script(directory.path(), "scripts/player.gd");
        assert_eq!(
            resolve_scene(directory.path(), &file),
            Err(SceneError::NoScene(file))
        );
    }

    #[test]
    fn passes_through_tscn_input() {
        let directory = tempdir().unwrap();
        let file = directory.path().join("scenes/current.tscn");
        assert_eq!(
            resolve_scene(directory.path(), &file).unwrap(),
            "res://scenes/current.tscn"
        );
    }
}
