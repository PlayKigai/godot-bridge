//! Zed settings file loading and merging.

use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::{Map, Value};

use crate::godot_bin::validate_extra_args;

const KNOWN_KEYS: [&str; 8] = [
    "godot_path",
    "project_dir",
    "lsp_port",
    "dap_port",
    "startup_timeout_s",
    "project_diagnostics",
    "diagnose_addons",
    "extra_args",
];

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub godot_path: Option<String>,
    pub project_dir: Option<String>,
    pub lsp_port: Option<u16>,
    pub dap_port: u16,
    pub startup_timeout_s: u32,
    pub project_diagnostics: bool,
    pub diagnose_addons: bool,
    pub extra_args: Vec<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            godot_path: None,
            project_dir: None,
            lsp_port: None,
            dap_port: 6006,
            startup_timeout_s: 600,
            project_diagnostics: true,
            diagnose_addons: false,
            extra_args: Vec::new(),
        }
    }
}

pub fn parse_settings(value: &Value) -> Result<Settings, String> {
    if value.is_null() {
        return Ok(Settings::default());
    }
    let Some(object) = value.as_object() else {
        return Err("initializationOptions must be an object".to_string());
    };
    for key in object.keys() {
        if !KNOWN_KEYS.contains(&key.as_str()) {
            tracing::warn!(key = %key, "ignoring unknown key in lsp.godot.settings");
        }
    }
    let settings: Settings = serde_json::from_value(value.clone())
        .map_err(|error| format!("invalid lsp.godot.settings: {error}"))?;
    validate_extra_args(&settings.extra_args)?;
    Ok(settings)
}

pub fn load_zed_settings(worktree: &Path) -> Result<Settings, String> {
    let user_path = user_settings_path();
    let project_path = worktree.join(".zed").join("settings.json");

    let user_section = read_settings_section(&user_path)?;
    let project_section = read_settings_section(&project_path)?;

    let mut merged = Map::new();
    if let Some(section) = user_section {
        validate_section(&user_path, &section)?;
        merged.extend(section);
    }
    if let Some(section) = project_section {
        validate_section(&project_path, &section)?;
        merged.extend(section);
    }
    parse_settings(&Value::Object(merged))
}

fn validate_section(path: &Path, section: &Map<String, Value>) -> Result<(), String> {
    parse_settings(&Value::Object(section.clone()))
        .map_err(|error| format!("{}: {error}", path.display()))?;
    Ok(())
}

fn read_settings_section(path: &Path) -> Result<Option<Map<String, Value>>, String> {
    if !path.is_file() {
        return Ok(None);
    }
    let text =
        std::fs::read_to_string(path).map_err(|error| format!("{}: {error}", path.display()))?;
    let value: Value =
        json5::from_str(&text).map_err(|error| format!("{}: {error}", path.display()))?;
    let Some(settings) = value
        .get("lsp")
        .and_then(|lsp| lsp.get("godot"))
        .and_then(|godot| godot.get("settings"))
    else {
        return Ok(None);
    };
    if settings.is_null() {
        return Ok(None);
    }
    let Some(object) = settings.as_object() else {
        return Err(format!(
            "{}: lsp.godot.settings must be an object",
            path.display()
        ));
    };
    Ok(Some(object.clone()))
}

fn user_settings_path() -> PathBuf {
    let config = match std::env::var_os("XDG_CONFIG_HOME") {
        Some(dir) => PathBuf::from(dir),
        None => std::env::var_os("HOME").map_or_else(
            || PathBuf::from(".config"),
            |home| PathBuf::from(home).join(".config"),
        ),
    };
    config.join("zed").join("settings.json")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;
    use std::fs;
    use tempfile::tempdir;

    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn with_config_dir(dir: &Path, run: impl FnOnce()) {
        let old = env::var_os("XDG_CONFIG_HOME");
        env::set_var("XDG_CONFIG_HOME", dir);
        run();
        match old {
            Some(value) => env::set_var("XDG_CONFIG_HOME", value),
            None => env::remove_var("XDG_CONFIG_HOME"),
        }
    }

    fn write_user_settings(config_dir: &Path, contents: &str) {
        let dir = config_dir.join("zed");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("settings.json"), contents).unwrap();
    }

    fn write_project_settings(worktree: &Path, contents: &str) {
        let dir = worktree.join(".zed");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("settings.json"), contents).unwrap();
    }

    #[test]
    fn absent_settings_parse_to_defaults() {
        let settings = parse_settings(&Value::Null).unwrap();
        assert_eq!(settings, Settings::default());
        assert_eq!(settings.dap_port, 6006);
        assert_eq!(settings.startup_timeout_s, 600);
        assert!(settings.project_diagnostics);
        assert!(!settings.diagnose_addons);
        assert!(settings.extra_args.is_empty());
    }

    #[test]
    fn unknown_key_does_not_fail() {
        let value: Value =
            serde_json::from_str(r#"{"project_diagnostics": false, "made_up_key": 42}"#).unwrap();
        let settings = parse_settings(&value).unwrap();
        assert!(!settings.project_diagnostics);
        assert_eq!(settings.dap_port, 6006);
    }

    #[test]
    fn non_object_is_an_error() {
        for value in [
            serde_json::from_str(r#"["lsp"]"#).unwrap(),
            serde_json::from_str(r#""string""#).unwrap(),
            serde_json::from_str("42").unwrap(),
        ] {
            assert_eq!(
                parse_settings(&value).unwrap_err(),
                "initializationOptions must be an object"
            );
        }
    }

    #[test]
    fn project_settings_override_user_per_key() {
        let _guard = ENV_LOCK.lock().unwrap();
        let config_dir = tempdir().unwrap();
        write_user_settings(
            config_dir.path(),
            r#"{
                "lsp": {
                    "godot": {
                        "settings": {
                            "godot_path": "user-godot",
                            "project_dir": "user-project",
                            "dap_port": 5555,
                            "diagnose_addons": true
                        }
                    }
                }
            }"#,
        );
        let worktree = tempdir().unwrap();
        write_project_settings(
            worktree.path(),
            r#"{
                "lsp": {
                    "godot": {
                        "settings": {
                            "godot_path": "project-godot",
                            "project_diagnostics": false,
                            "extra_args": ["--verbose"],
                            "startup_timeout_s": 30
                        }
                    }
                }
            }"#,
        );
        with_config_dir(config_dir.path(), || {
            let settings = load_zed_settings(worktree.path()).unwrap();
            assert_eq!(settings.godot_path.as_deref(), Some("project-godot"));
            assert_eq!(settings.project_dir.as_deref(), Some("user-project"));
            assert_eq!(settings.lsp_port, None);
            assert_eq!(settings.dap_port, 5555);
            assert_eq!(settings.startup_timeout_s, 30);
            assert!(!settings.project_diagnostics);
            assert!(settings.diagnose_addons);
            assert_eq!(settings.extra_args, ["--verbose"]);
        });
    }

    #[test]
    fn comment_bearing_settings_file_parses() {
        let _guard = ENV_LOCK.lock().unwrap();
        let config_dir = tempdir().unwrap();
        let worktree = tempdir().unwrap();
        write_project_settings(
            worktree.path(),
            r#"{
                // comment
                "lsp": {
                    "godot": {
                        "settings": {
                            "godot_path": "godot4", // inline
                            "extra_args": ["--verbose",],
                        },
                    },
                },
            }"#,
        );
        with_config_dir(config_dir.path(), || {
            let settings = load_zed_settings(worktree.path()).unwrap();
            assert_eq!(settings.godot_path.as_deref(), Some("godot4"));
            assert_eq!(settings.extra_args, ["--verbose"]);
        });
    }

    #[test]
    fn validation_error_names_the_file() {
        let _guard = ENV_LOCK.lock().unwrap();
        let config_dir = tempdir().unwrap();
        let worktree = tempdir().unwrap();
        write_project_settings(
            worktree.path(),
            r#"{"lsp": {"godot": {"settings": {"extra_args": ["--editor"]}}}}"#,
        );
        with_config_dir(config_dir.path(), || {
            let error = load_zed_settings(worktree.path()).unwrap_err();
            assert!(error.contains(".zed/settings.json"), "{error}");
            assert!(error.contains("--editor"), "{error}");
        });
    }
}
