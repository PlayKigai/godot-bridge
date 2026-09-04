//! Project and scene execution.

use std::path::Path;
use std::process::{Command, ExitCode};

use crate::godot_bin;
use crate::root;
use crate::scene;
use crate::settings_file;

pub fn run(file: &Path, scene: Option<&str>) -> anyhow::Result<ExitCode> {
    let worktree = root::cwd_root()?;
    let settings = settings_file::load_zed_settings(&worktree).map_err(anyhow::Error::msg)?;
    let project = root::find_project_dir(
        &worktree,
        Some(file),
        settings.project_dir.as_deref().map(Path::new),
    )?;
    let godot = godot_bin::resolve_godot(settings.godot_path.as_deref().map(Path::new))
        .map_err(anyhow::Error::msg)?;

    let scene = match scene {
        None | Some("main") => None,
        Some("current") => Some(scene::resolve_scene(&project, file)?),
        Some(passed) => Some(passed.to_owned()),
    };

    let mut command = Command::new(&godot);
    command
        .args(&settings.extra_args)
        .arg("--path")
        .arg(&project);
    if let Some(scene) = scene {
        command.arg(scene);
    }

    let status = command.status()?;
    Ok(ExitCode::from(status.code().unwrap_or(1) as u8))
}

pub fn project_dir(file: &Path) -> anyhow::Result<()> {
    let worktree = root::cwd_root()?;
    let settings = settings_file::load_zed_settings(&worktree).map_err(anyhow::Error::msg)?;
    let project = root::find_project_dir(
        &worktree,
        Some(file),
        settings.project_dir.as_deref().map(Path::new),
    )?;
    println!("{}", project.display());
    Ok(())
}
