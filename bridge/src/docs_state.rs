use crate::file_uri::path_to_uri;
use crate::root::{canonical_or_normalized, doc_key, normalize_absolute};
use std::collections::HashMap;
use std::ffi::OsStr;
use std::io::Read;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Component, Path, PathBuf};

pub const MAX_DOCUMENT_BYTES: usize = 2 * 1024 * 1024;
pub const BULK_DOCUMENTS: usize = 100;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DocumentOwner {
    Zed,
    Bridge,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpenDoc {
    pub uri: String,
    pub version: i64,
    pub generation: u64,
    pub text: Option<String>,
    text_hash: u64,
    pub owner: DocumentOwner,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DocumentAction {
    Open {
        uri: String,
        version: i64,
        text: String,
    },
    Change {
        uri: String,
        version: i64,
        text: String,
    },
}

pub struct DocumentState {
    pub(crate) open_docs: HashMap<PathBuf, OpenDoc>,
    pub(crate) uri_keys: HashMap<String, PathBuf>,
    pub(crate) watcher_keys: HashMap<PathBuf, PathBuf>,
    generation: u64,
}

impl DocumentState {
    pub fn new() -> Self {
        Self {
            open_docs: HashMap::new(),
            uri_keys: HashMap::new(),
            watcher_keys: HashMap::new(),
            generation: 0,
        }
    }

    pub fn zed_open(&mut self, incoming_uri: &str, action: DocumentAction) -> (String, i64) {
        let key = self.key_for_uri(incoming_uri);
        self.uri_keys.insert(incoming_uri.to_owned(), key.clone());
        self.register_watcher_path(&key, key.clone());
        match action {
            DocumentAction::Change { uri, version, text } => {
                let doc = self
                    .open_docs
                    .get_mut(&key)
                    .expect("planned document exists");
                doc.text = Some(text);
                doc.version = version;
                doc.owner = DocumentOwner::Zed;
                (uri, version)
            }
            DocumentAction::Open { uri, version, text } => {
                let generation = self.next_generation();
                self.open_docs.insert(
                    key.clone(),
                    OpenDoc {
                        uri: uri.clone(),
                        version,
                        generation,
                        text: Some(text),
                        text_hash: 0,
                        owner: DocumentOwner::Zed,
                    },
                );
                self.uri_keys.insert(uri.clone(), key.clone());
                (uri, version)
            }
        }
    }

    pub fn zed_change(
        &mut self,
        incoming_uri: &str,
        action: DocumentAction,
    ) -> Option<(String, i64)> {
        let key = self.key_for_uri(incoming_uri);
        let DocumentAction::Change { uri, version, text } = action else {
            unreachable!();
        };
        let doc = self.open_docs.get_mut(&key)?;
        doc.text = Some(text);
        doc.version = version;
        doc.owner = DocumentOwner::Zed;
        Some((uri, version))
    }

    pub fn zed_close(&mut self, incoming_uri: &str) -> Option<(PathBuf, String)> {
        let key = self.key_for_uri(incoming_uri);
        let doc = self.open_docs.remove(&key)?;
        self.remove_mappings(&key);
        Some((key, doc.uri))
    }

    pub fn bridge_open_path(&mut self, path: &Path, text: String) -> Option<DocumentAction> {
        let key = canonical_or_normalized(path);
        self.register_watcher_path(path, key.clone());
        self.bridge_open_key(key, text)
    }

    pub fn bridge_open_key(&mut self, key: PathBuf, text: String) -> Option<DocumentAction> {
        if let Some(uri) = self.open_docs.get(&key).map(|doc| doc.uri.clone()) {
            self.uri_keys.insert(uri, key);
            return None;
        }
        let uri = path_to_uri(&key);
        let version = 1;
        let generation = self.next_generation();
        self.open_docs.insert(
            key.clone(),
            OpenDoc {
                uri: uri.clone(),
                version,
                generation,
                text: None,
                text_hash: crate::fnv::hash(text.as_bytes()),
                owner: DocumentOwner::Bridge,
            },
        );
        self.uri_keys.insert(uri.clone(), key.clone());
        Some(DocumentAction::Open { uri, version, text })
    }

    pub fn bridge_change_path(&mut self, path: &Path, text: String) -> Option<DocumentAction> {
        let key = canonical_or_normalized(path);
        self.register_watcher_path(path, key.clone());
        let hash = crate::fnv::hash(text.as_bytes());
        let doc = self.open_docs.get_mut(&key)?;
        if doc.owner != DocumentOwner::Bridge || doc.text_hash == hash {
            return None;
        }
        doc.text_hash = hash;
        doc.version += 1;
        let uri = doc.uri.clone();
        let version = doc.version;
        Some(DocumentAction::Change { uri, version, text })
    }

    pub fn bridge_remove_key(&mut self, key: &Path) -> Option<String> {
        let key = key.to_path_buf();
        if self
            .open_docs
            .get(&key)
            .is_none_or(|doc| doc.owner != DocumentOwner::Bridge)
        {
            return None;
        }
        let doc = self.open_docs.remove(&key)?;
        self.remove_mappings(&key);
        Some(doc.uri)
    }

    pub fn forget_closed(&mut self, key: &Path) {
        self.remove_mappings(key);
    }

    pub fn register_watcher_path(&mut self, path: &Path, key: PathBuf) {
        let path = normalize_absolute(path);
        if path == key {
            self.watcher_keys.remove(&path);
        } else {
            self.watcher_keys.insert(path, key);
        }
    }

    pub fn watcher_key(&self, path: &Path) -> Option<PathBuf> {
        let path = normalize_absolute(path);
        self.open_docs
            .contains_key(&path)
            .then_some(path.clone())
            .or_else(|| self.watcher_keys.get(&path).cloned())
    }

    pub fn owner(&self, key: &Path) -> Option<DocumentOwner> {
        self.open_docs.get(key).map(|doc| doc.owner)
    }

    pub fn key_for_uri(&self, uri: &str) -> PathBuf {
        self.uri_keys
            .get(uri)
            .cloned()
            .unwrap_or_else(|| doc_key(uri))
    }

    pub fn generation_for_uri(&self, uri: &str) -> Option<u64> {
        let key = self.key_for_uri(uri);
        self.open_docs.get(&key).map(|doc| doc.generation)
    }

    fn remove_mappings(&mut self, key: &Path) {
        self.uri_keys.retain(|_, value| value != key);
        self.watcher_keys.retain(|_, value| value != key);
    }

    fn next_generation(&mut self) -> u64 {
        self.generation += 1;
        self.generation
    }

    pub(crate) fn plan_zed_open_key(&self, key: &Path, text: &str) -> DocumentAction {
        if let Some(doc) = self.open_docs.get(key) {
            DocumentAction::Change {
                uri: doc.uri.clone(),
                version: doc.version + 1,
                text: text.to_owned(),
            }
        } else {
            DocumentAction::Open {
                uri: path_to_uri(key),
                version: 1,
                text: text.to_owned(),
            }
        }
    }

    pub(crate) fn plan_zed_change_key(&self, key: &Path, text: &str) -> Option<DocumentAction> {
        let doc = self.open_docs.get(key)?;
        Some(DocumentAction::Change {
            uri: doc.uri.clone(),
            version: doc.version + 1,
            text: text.to_owned(),
        })
    }
}

impl Default for DocumentState {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScannedDocument {
    pub path: PathBuf,
    pub key: PathBuf,
    pub text: Option<String>,
}

pub fn scan_project_stream(
    project: &Path,
    diagnose_addons: bool,
    mut send: impl FnMut(ScannedDocument) -> bool,
) {
    let project = canonical_or_normalized(project);
    let mut directories = vec![project.clone()];
    while let Some(directory) = directories.pop() {
        let mut entries = match std::fs::read_dir(&directory) {
            Ok(entries) => entries.flatten().collect::<Vec<_>>(),
            Err(error) => {
                crate::warn!(
                    "skipping unreadable diagnostics directory {}: {error}",
                    directory.display()
                );
                continue;
            }
        };
        entries.sort_by_key(|entry| entry.path());
        for entry in entries {
            let path = entry.path();
            let file_type = match entry.file_type() {
                Ok(file_type) => file_type,
                Err(error) => {
                    crate::warn!(
                        "skipping unreadable diagnostics entry {}: {error}",
                        path.display()
                    );
                    continue;
                }
            };
            if file_type.is_symlink() {
                continue;
            }
            if file_type.is_dir() {
                if !directory_is_skipped(&path, &project, diagnose_addons) {
                    directories.push(path);
                }
                continue;
            }
            if !file_type.is_file() || path.extension() != Some(OsStr::new("gd")) {
                continue;
            }
            let Some(text) = read_document(&path) else {
                continue;
            };
            let key = canonical_or_normalized(&path);
            if !send(ScannedDocument {
                path,
                key,
                text: Some(text),
            }) {
                return;
            }
        }
    }
}

pub fn read_document(path: &Path) -> Option<String> {
    let file = match std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
    {
        Ok(file) => file,
        Err(error) => {
            crate::warn!(
                "skipping unreadable diagnostics file {}: {error}",
                path.display()
            );
            return None;
        }
    };
    let mut bytes = Vec::new();
    if file
        .take((MAX_DOCUMENT_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .is_err()
    {
        crate::warn!("skipping unreadable diagnostics file {}", path.display());
        return None;
    }
    if bytes.len() > MAX_DOCUMENT_BYTES {
        crate::warn!("skipping diagnostics file over 2 MiB {}", path.display());
        return None;
    }
    match String::from_utf8(bytes) {
        Ok(text) => Some(text),
        Err(_) => {
            crate::warn!(
                "skipping diagnostics file with invalid UTF-8 {}",
                path.display()
            );
            None
        }
    }
}

pub fn eligible_path(project: &Path, path: &Path, diagnose_addons: bool) -> bool {
    if path.extension() != Some(OsStr::new("gd")) {
        return false;
    }
    let project = canonical_or_normalized(project);
    let path = canonical_or_normalized(path);
    let Ok(relative) = path.strip_prefix(&project) else {
        return false;
    };
    if relative.components().next().is_none() {
        return false;
    }
    path.parent()
        .is_some_and(|parent| !directory_is_skipped(parent, &project, diagnose_addons))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum WatcherChangeKind {
    Created,
    Modified,
    Removed,
    Rescan,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WatcherChange {
    pub kind: WatcherChangeKind,
    pub path: PathBuf,
}

pub(crate) fn directory_is_skipped(path: &Path, project: &Path, diagnose_addons: bool) -> bool {
    let Ok(relative) = path.strip_prefix(project) else {
        return true;
    };
    relative.components().any(|component| {
        let Component::Normal(name) = component else {
            return true;
        };
        name == OsStr::new(".godot")
            || (!diagnose_addons && name == OsStr::new("addons"))
            || name.to_string_lossy().starts_with('.')
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::temp::TempDir;
    use std::fs;

    #[test]
    fn scan_filters_project_diagnostics_files() {
        let directory = TempDir::new().unwrap();
        fs::write(directory.path().join("valid.gd"), "extends Node\n").unwrap();
        fs::create_dir(directory.path().join(".godot")).unwrap();
        fs::write(directory.path().join(".godot/hidden.gd"), "x").unwrap();
        fs::create_dir(directory.path().join("addons")).unwrap();
        fs::write(directory.path().join("addons/addon.gd"), "x").unwrap();
        fs::create_dir(directory.path().join(".hidden")).unwrap();
        fs::write(directory.path().join(".hidden/hidden.gd"), "x").unwrap();
        fs::write(
            directory.path().join("large.gd"),
            vec![b'x'; MAX_DOCUMENT_BYTES + 1],
        )
        .unwrap();
        fs::write(directory.path().join("invalid.gd"), [0xff, 0xfe]).unwrap();

        let mut normal = Vec::new();
        scan_project_stream(directory.path(), false, |document| {
            normal.push(document);
            true
        });
        normal.sort_by(|left, right| left.path.cmp(&right.path));
        assert_eq!(
            normal
                .iter()
                .map(|doc| doc.path.clone())
                .collect::<Vec<_>>(),
            vec![directory.path().join("valid.gd")]
        );
        let mut with_addons = Vec::new();
        scan_project_stream(directory.path(), true, |document| {
            with_addons.push(document);
            true
        });
        assert!(with_addons
            .iter()
            .any(|doc| doc.path.ends_with("addons/addon.gd")));
    }
}
