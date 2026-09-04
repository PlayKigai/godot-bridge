use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::json;

use crate::state::{remove_if_stale, runtime_dir, socket_request};

const STATUS_TIMEOUT: Duration = Duration::from_secs(5);

pub async fn run() -> crate::error::Result<()> {
    let stdout = io::stdout();
    run_in(&runtime_dir()?, &mut stdout.lock()).await
}

async fn run_in(dir: &Path, out: &mut impl Write) -> crate::error::Result<()> {
    for state_path in list_state_files(dir)? {
        let sock_path = state_path.with_extension("sock");
        match socket_request(&sock_path, &json!({"cmd": "status"}), STATUS_TIMEOUT).await {
            Ok(response) => writeln!(out, "{response}")?,
            Err(_) => {
                let _ = remove_if_stale(&state_path, &sock_path);
            }
        }
    }
    Ok(())
}

fn list_state_files(dir: &Path) -> io::Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        if entry.file_type()?.is_file() && entry.path().extension().is_some_and(|ext| ext == "json")
        {
            files.push(entry.path());
        }
    }
    files.sort();
    Ok(files)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    use crate::state::{serve_socket, write_state, Mode, State, Status};
    use tempfile::tempdir;

    #[tokio::test]
    async fn detached_gui_state_without_socket_is_kept() {
        let dir = tempdir().unwrap();
        write_state(
            &dir.path().join("gui.json"),
            &crate::state::detached_gui_state(Path::new("/project"), Status::Ready),
        )
        .unwrap();
        let mut output = Vec::new();
        run_in(dir.path(), &mut output).await.unwrap();
        assert!(dir.path().join("gui.json").exists());
        assert!(output.is_empty());
    }

    #[tokio::test]
    async fn stale_state_is_removed_and_live_status_is_printed() {
        let dir = tempdir().unwrap();
        let stale = State {
            version: 1,
            project: "/stale".to_owned(),
            status: Status::Ready,
            mode: Mode::Headless,
            godot_pid: None,
            godot_pgid: None,
            lsp_port: None,
            dap_port: None,
            owner_pid: Some(u32::MAX),
            owner_start_ticks: Some(1),
            godot_start_ticks: None,
            started_at: "2026-09-04T00:00:00Z".to_owned(),
            bridge_version: "0.1.0".to_owned(),
        };
        write_state(&dir.path().join("stale.json"), &stale).unwrap();
        std::fs::write(dir.path().join("stale.sock"), b"dead").unwrap();
        std::fs::write(dir.path().join("live.json"), b"{}").unwrap();
        let _handle = serve_socket(dir.path().join("live.sock"), |_request| async move {
            json!({"status": "ready", "project": "/live"})
        })
        .await
        .unwrap();
        let mut output = Vec::new();
        run_in(dir.path(), &mut output).await.unwrap();
        assert!(!dir.path().join("stale.json").exists());
        assert!(!dir.path().join("stale.sock").exists());
        assert!(dir.path().join("live.json").exists());
        let lines: Vec<&str> = std::str::from_utf8(&output).unwrap().lines().collect();
        assert_eq!(lines.len(), 1);
        let response: Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(response["status"], "ready");
        assert_eq!(response["project"], "/live");
    }
}
