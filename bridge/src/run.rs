use std::path::Path;
use std::process::{Command, ExitCode};

use crate::godot_bin;
use crate::root;
use crate::scene;
use crate::settings_file;

pub fn run(file: &Path, scene: Option<&str>) -> crate::error::Result<ExitCode> {
    let worktree = root::cwd_root()?;
    let settings = settings_file::load_zed_settings(&worktree).map_err(crate::error::Error::new)?;
    let project = root::find_project_dir(
        &worktree,
        Some(file),
        settings.project_dir.as_deref().map(Path::new),
    )?;
    let godot = godot_bin::resolve_godot(settings.godot_path.as_deref().map(Path::new))
        .map_err(crate::error::Error::new)?;
    godot_bin::check_version(&godot).map_err(crate::error::Error::new)?;

    let scene = match scene {
        None | Some("main") => None,
        Some("current") => Some(scene::resolve_scene(&project, file)?),
        Some(passed) if !passed.starts_with('-') => Some(passed.to_owned()),
        Some(passed) => crate::bail!("scene value cannot start with '-': {passed}"),
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

pub fn project_dir(file: &Path) -> crate::error::Result<()> {
    let worktree = root::cwd_root()?;
    let settings = settings_file::load_zed_settings(&worktree).map_err(crate::error::Error::new)?;
    let project = root::find_project_dir(
        &worktree,
        Some(file),
        settings.project_dir.as_deref().map(Path::new),
    )?;
    println!("{}", project.display());
    Ok(())
}
