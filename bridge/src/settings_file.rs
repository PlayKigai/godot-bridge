use std::ffi::OsString;
use std::io;
use std::io::Read;
use std::path::{Path, PathBuf};

use crate::json::{Map, Value};

use crate::godot_bin::validate_extra_args;

const SETTINGS_FILE_CAP: u64 = 1024 * 1024;

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
const PROJECT_UNTRUSTED_KEYS: [&str; 5] = [
    "godot_path",
    "project_dir",
    "lsp_port",
    "dap_port",
    "extra_args",
];

#[derive(Debug, Clone, PartialEq)]
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

impl Settings {
    fn from_object(object: &Map) -> Result<Self, String> {
        let mut settings = Self {
            godot_path: optional_string(object, "godot_path")?,
            project_dir: optional_string(object, "project_dir")?,
            ..Self::default()
        };
        if object.contains_key("lsp_port") {
            settings.lsp_port = optional_u64(object, "lsp_port")?
                .map(|value| u16::try_from(value).map_err(|_| "lsp_port must be a 16-bit integer"))
                .transpose()?;
        }
        if object.contains_key("dap_port") {
            settings.dap_port = default_u64(object, "dap_port", u64::from(settings.dap_port))?
                .try_into()
                .map_err(|_| "dap_port must be a 16-bit integer")?;
        }
        if object.contains_key("startup_timeout_s") {
            settings.startup_timeout_s = default_u64(
                object,
                "startup_timeout_s",
                u64::from(settings.startup_timeout_s),
            )?
            .try_into()
            .map_err(|_| "startup_timeout_s must be a 32-bit integer")?;
        }
        settings.project_diagnostics =
            bool_value(object, "project_diagnostics", settings.project_diagnostics)?;
        settings.diagnose_addons = bool_value(object, "diagnose_addons", settings.diagnose_addons)?;
        if let Some(extra_args) = string_array(object, "extra_args")? {
            settings.extra_args = extra_args;
        }
        Ok(settings)
    }
}

fn optional_string(object: &Map, key: &str) -> Result<Option<String>, String> {
    match object.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value
            .as_str()
            .map(str::to_owned)
            .map(Some)
            .ok_or_else(|| format!("{key} must be a string or null")),
    }
}

fn optional_u64(object: &Map, key: &str) -> Result<Option<u64>, String> {
    match object.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value
            .as_u64()
            .ok_or_else(|| format!("{key} must be an unsigned integer"))
            .map(Some),
    }
}

fn default_u64(object: &Map, key: &str, default: u64) -> Result<u64, String> {
    object.get(key).map_or(Ok(default), |value| {
        value
            .as_u64()
            .ok_or_else(|| format!("{key} must be an unsigned integer"))
    })
}

fn bool_value(object: &Map, key: &str, default: bool) -> Result<bool, String> {
    object.get(key).map_or(Ok(default), |value| {
        value
            .as_bool()
            .ok_or_else(|| format!("{key} must be a boolean"))
    })
}

fn string_array(object: &Map, key: &str) -> Result<Option<Vec<String>>, String> {
    let Some(value) = object.get(key) else {
        return Ok(None);
    };
    let Some(values) = value.as_array() else {
        return Err(format!("{key} must be an array of strings"));
    };
    values
        .iter()
        .map(|value| {
            value
                .as_str()
                .map(str::to_owned)
                .ok_or_else(|| format!("{key} must be an array of strings"))
        })
        .collect::<Result<Vec<_>, _>>()
        .map(Some)
}

pub fn parse_settings(value: &Value) -> Result<Settings, String> {
    if value.is_null() {
        return Ok(Settings::default());
    }
    let Some(object) = value.as_object() else {
        return Err("settings must be an object".to_string());
    };
    for key in object.keys() {
        if !KNOWN_KEYS.contains(&key.as_str()) {
            crate::warn!("ignoring unknown setting {key}");
        }
    }
    let settings =
        Settings::from_object(object).map_err(|error| format!("invalid settings: {error}"))?;
    validate_settings(&settings).map_err(|error| format!("invalid settings: {error}"))?;
    Ok(settings)
}

const ENV_SETTINGS: &str = "GODOT_BRIDGE_SETTINGS";

/// Already merged by the client, so every key is kept.
fn env_settings() -> Result<Option<Settings>, String> {
    match std::env::var(ENV_SETTINGS) {
        Ok(contents) if contents.is_empty() => Ok(None),
        Ok(contents) if contents.len() <= SETTINGS_FILE_CAP as usize => {
            let value = crate::json::from_str(&contents)
                .map_err(|error| format!("invalid {ENV_SETTINGS}: {error}"))?;
            parse_settings(&value).map(Some)
        }
        Ok(_) => Err(format!("{ENV_SETTINGS} exceeds 1 MiB")),
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(error) => Err(format!("cannot read {ENV_SETTINGS}: {error}")),
    }
}

fn load_with(fallback: impl FnOnce() -> Result<Settings, String>) -> Result<Settings, String> {
    match env_settings()? {
        Some(settings) => Ok(settings),
        None => fallback(),
    }
}

pub fn load_cli(worktree: &Path) -> Result<Settings, String> {
    load_with(|| load_file_settings(worktree, user_settings_path().as_deref()))
}

pub fn load_lsp(options: &Value, worktree: &Path) -> Result<Settings, String> {
    load_with(|| merge_lsp_options(options, worktree))
}

fn load_file_settings(worktree: &Path, user_path: Option<&Path>) -> Result<Settings, String> {
    let project_path = worktree.join(".zed").join("settings.json");

    let project_section = read_settings_section(&project_path)?;

    let mut merged = Map::new();
    if let Some(section) = read_user_section(user_path)? {
        merged.extend(section);
    }
    if let Some(mut section) = project_section {
        remove_untrusted_project_keys(&project_path, &mut section);
        validate_section(&project_path, &section)?;
        merged.extend(section);
    }
    parse_settings(&Value::Object(merged))
}

fn merge_lsp_options(value: &Value, worktree: &Path) -> Result<Settings, String> {
    let empty = Map::new();
    let object = match value.as_object() {
        Some(object) => object,
        None if value.is_null() => &empty,
        None => return parse_settings(value),
    };
    let user_section = read_user_section(user_settings_path().as_deref())?;

    let project_path = worktree.join(".zed").join("settings.json");
    if let Some(project_section) = read_settings_section(&project_path)? {
        warn_untrusted_project_keys(&project_path, &project_section);
    }

    let mut merged = user_section.unwrap_or_default();
    for (key, value) in object {
        if !PROJECT_UNTRUSTED_KEYS.contains(&key.as_str()) {
            merged.insert(key.clone(), value.clone());
        }
    }
    parse_settings(&Value::Object(merged))
}

fn warn_untrusted_project_keys(path: &Path, section: &Map) {
    for key in PROJECT_UNTRUSTED_KEYS {
        if section.contains_key(key) {
            crate::warn!("ignoring project setting {key} in {}", path.display());
        }
    }
}

fn remove_untrusted_project_keys(path: &Path, section: &mut Map) {
    for key in PROJECT_UNTRUSTED_KEYS {
        if section.remove(key).is_some() {
            crate::warn!("ignoring project setting {key} in {}", path.display());
        }
    }
}

fn validate_section(path: &Path, section: &Map) -> Result<(), String> {
    let settings =
        Settings::from_object(section).map_err(|error| format!("{}: {error}", path.display()))?;
    validate_settings(&settings).map_err(|error| format!("{}: {error}", path.display()))?;
    Ok(())
}

fn validate_settings(settings: &Settings) -> Result<(), String> {
    if let Some(path) = settings.godot_path.as_deref() {
        crate::godot_bin::validate_godot_path(Path::new(path))?;
    }
    validate_extra_args(&settings.extra_args)?;
    Ok(())
}

fn read_settings_section(path: &Path) -> Result<Option<Map>, String> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
            ) =>
        {
            return Ok(None)
        }
        Err(error) => return Err(format!("{}: {error}", path.display())),
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(format!(
            "{}: settings file is not a regular file",
            path.display()
        ));
    }
    if metadata.len() > SETTINGS_FILE_CAP {
        return Err(format!("{}: settings file exceeds 1 MiB", path.display()));
    }
    let file = crate::sys::open_nofollow_read(path)
        .map_err(|error| format!("{}: {error}", path.display()))?;
    let mut text = String::new();
    file.take(SETTINGS_FILE_CAP + 1)
        .read_to_string(&mut text)
        .map_err(|error| format!("{}: {error}", path.display()))?;
    if text.len() > SETTINGS_FILE_CAP as usize {
        return Err(format!("{}: settings file exceeds 1 MiB", path.display()));
    }
    let value: Value = crate::json::from_str_relaxed(&text)
        .map_err(|error| format!("{}: {error}", path.display()))?;
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

fn read_user_section(path: Option<&Path>) -> Result<Option<Map>, String> {
    let Some(path) = path else {
        return Ok(None);
    };
    let section = read_settings_section(path)?;
    if let Some(section) = &section {
        validate_section(path, section)?;
    }
    Ok(section)
}

#[cfg(unix)]
const CONFIG_HOME_FALLBACK: &str = "HOME";
#[cfg(windows)]
const CONFIG_HOME_FALLBACK: &str = "APPDATA";

fn user_settings_path() -> Option<PathBuf> {
    let directory = zed_config_dir(
        std::env::var_os("XDG_CONFIG_HOME"),
        std::env::var_os(CONFIG_HOME_FALLBACK),
    )?;
    Some(directory.join("settings.json"))
}

/// Zed reads its user settings from `$XDG_CONFIG_HOME/zed`, falling back to
/// `~/.config/zed` and, on Windows, to `%APPDATA%\Zed`.
fn zed_config_dir(config_home: Option<OsString>, fallback: Option<OsString>) -> Option<PathBuf> {
    if let Some(directory) = config_home {
        return Some(PathBuf::from(directory).join("zed"));
    }
    let fallback = PathBuf::from(fallback?);
    #[cfg(unix)]
    let directory = fallback.join(".config").join("zed");
    #[cfg(windows)]
    let directory = fallback.join("Zed");
    Some(directory)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::temp::TempDir;
    use std::fs;

    #[test]
    fn user_settings_live_where_zed_keeps_them() {
        assert_eq!(
            zed_config_dir(Some(OsString::from("/config")), None),
            Some(PathBuf::from("/config").join("zed"))
        );
        assert_eq!(zed_config_dir(None, None), None);
        #[cfg(unix)]
        assert_eq!(
            zed_config_dir(None, Some(OsString::from("/home/user"))),
            Some(PathBuf::from("/home/user/.config/zed"))
        );
        #[cfg(windows)]
        assert_eq!(
            zed_config_dir(None, Some(OsString::from(r"C:\Users\u\AppData\Roaming"))),
            Some(PathBuf::from(r"C:\Users\u\AppData\Roaming\Zed"))
        );
    }

    fn user_settings_path_in(config_dir: &Path) -> PathBuf {
        config_dir.join("zed").join("settings.json")
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
    fn single_file_worktree_has_no_project_settings() {
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("main.gd");
        fs::write(&file, "").unwrap();
        assert!(load_file_settings(&file, None).is_ok());
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
            crate::json::from_str(r#"{"project_diagnostics": false, "made_up_key": 42}"#).unwrap();
        let settings = parse_settings(&value).unwrap();
        assert!(!settings.project_diagnostics);
        assert_eq!(settings.dap_port, 6006);
    }

    #[test]
    fn non_object_is_an_error() {
        for value in [
            crate::json::from_str(r#"["lsp"]"#).unwrap(),
            crate::json::from_str(r#""string""#).unwrap(),
            crate::json::from_str("42").unwrap(),
        ] {
            assert_eq!(
                parse_settings(&value).unwrap_err(),
                "settings must be an object"
            );
        }
    }

    /// An absolute path to a real executable the settings loader will accept.
    #[cfg(unix)]
    const TRUSTED_GODOT_PATH: &str = "/bin/sh";
    #[cfg(windows)]
    const TRUSTED_GODOT_PATH: &str = "C:/Windows/System32/cmd.exe";

    #[test]
    fn project_settings_override_user_per_key() {
        let config_dir = TempDir::new().unwrap();
        write_user_settings(
            config_dir.path(),
            &format!(
                r#"{{
                "lsp": {{
                    "godot": {{
                        "settings": {{
                            "godot_path": "{TRUSTED_GODOT_PATH}",
                            "project_dir": "user-project",
                            "dap_port": 5555,
                            "diagnose_addons": true
                        }}
                    }}
                }}
            }}"#
            ),
        );
        let worktree = TempDir::new().unwrap();
        write_project_settings(
            worktree.path(),
            r#"{
                "lsp": {
                    "godot": {
                        "settings": {
                             "godot_path": "/not-an-executable",
                             "project_dir": "/not-a-project",
                             "project_diagnostics": false,
                             "extra_args": ["--editor"],
                            "startup_timeout_s": 30
                        }
                    }
                }
            }"#,
        );
        let settings = load_file_settings(
            worktree.path(),
            Some(&user_settings_path_in(config_dir.path())),
        )
        .unwrap();
        assert_eq!(settings.godot_path.as_deref(), Some(TRUSTED_GODOT_PATH));
        assert_eq!(settings.project_dir.as_deref(), Some("user-project"));
        assert_eq!(settings.lsp_port, None);
        assert_eq!(settings.dap_port, 5555);
        assert_eq!(settings.startup_timeout_s, 30);
        assert!(!settings.project_diagnostics);
        assert!(settings.diagnose_addons);
        assert!(settings.extra_args.is_empty());
    }

    #[test]
    fn duplicate_project_untrusted_setting_is_removed() {
        let config_dir = TempDir::new().unwrap();
        let worktree = TempDir::new().unwrap();
        write_project_settings(
            worktree.path(),
            r#"{"lsp":{"godot":{"settings":{"godot_path":"/bin/true","godot_path":"/bin/sh"}}}}"#,
        );
        let settings = load_file_settings(
            worktree.path(),
            Some(&user_settings_path_in(config_dir.path())),
        )
        .unwrap();
        assert_eq!(settings.godot_path, None);
    }

    #[test]
    fn comment_bearing_settings_file_parses() {
        let config_dir = TempDir::new().unwrap();
        let worktree = TempDir::new().unwrap();
        write_project_settings(
            worktree.path(),
            r#"{
                // comment
                "lsp": {
                    "godot": {
                        "settings": {
                            "godot_path": "/bin/true", // inline
                            "extra_args": ["--verbose",],
                        },
                    },
                },
            }"#,
        );
        let settings = load_file_settings(
            worktree.path(),
            Some(&user_settings_path_in(config_dir.path())),
        )
        .unwrap();
        assert_eq!(settings.godot_path, None);
        assert!(settings.extra_args.is_empty());
    }

    #[test]
    fn validation_error_names_the_file() {
        let config_dir = TempDir::new().unwrap();
        let worktree = TempDir::new().unwrap();
        write_project_settings(
            worktree.path(),
            r#"{"lsp": {"godot": {"settings": {"startup_timeout_s": "bad"}}}}"#,
        );
        let error = load_file_settings(
            worktree.path(),
            Some(&user_settings_path_in(config_dir.path())),
        )
        .unwrap_err();
        let expected = Path::new(".zed").join("settings.json");
        assert!(
            error.contains(&expected.to_string_lossy().into_owned()),
            "{error}"
        );
        assert!(error.contains("startup_timeout_s"), "{error}");
    }
}
