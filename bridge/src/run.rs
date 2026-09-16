use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use crate::exclude::Exclude;
use crate::godot_bin;
use crate::root;
use crate::scene;
use crate::settings_file;

pub fn run(file: &Path, scene: Option<&str>) -> crate::error::Result<ExitCode> {
    let (project, file, settings) = resolve_project(file)?;
    let godot = godot_bin::resolve_godot(settings.godot_path.as_deref().map(Path::new))?;
    godot_bin::check_version(&godot)?;

    let scene = match scene {
        None | Some("main") => None,
        Some("current") => Some(scene::resolve_scene(
            &project,
            &file,
            &Exclude::new(&settings.exclude),
        )?),
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
    let (project, _, _) = resolve_project(file)?;
    println!("{}", project.display());
    Ok(())
}

fn resolve_project(
    file: &Path,
) -> crate::error::Result<(PathBuf, PathBuf, settings_file::Settings)> {
    let worktree = root::cwd_root()?;
    let settings = settings_file::load_cli(&worktree)?;
    let (project, resolved) = root::resolve_project_and_file(
        &worktree,
        Some(file),
        settings.project_dir.as_deref().map(Path::new),
    )?;
    let file = resolved.unwrap_or_else(|| file.to_path_buf());
    Ok((project, file, settings))
}
