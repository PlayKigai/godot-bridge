use std::collections::HashMap;
use std::ffi::OsStr;
use std::io::Read;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use notify::event::{ModifyKind, RenameMode};
use notify::{Config, Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use tokio::sync::mpsc::{self, Receiver};

use crate::root::{canonical_or_normalized, doc_key, path_to_uri};

pub use crate::root::normalize_absolute as normalize_path;

pub const MAX_DOCUMENT_BYTES: usize = 2 * 1024 * 1024;
pub const BULK_DOCUMENTS: usize = 20;
pub const BULK_INTERVAL_MS: u64 = 50;

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
    pub text: String,
    pub owner: DocumentOwner,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DocumentEvent {
    Open {
        uri: String,
        generation: u64,
        version: i64,
    },
    Change {
        uri: String,
        generation: u64,
        version: i64,
    },
    Close {
        uri: String,
        generation: u64,
    },
    Remove {
        uri: String,
        generation: u64,
    },
}

pub type DocumentEventHook = Arc<dyn Fn(DocumentEvent) + Send + Sync>;

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
    generations: HashMap<PathBuf, u64>,
    event_hook: Option<DocumentEventHook>,
    emit_open_change_events: bool,
}

impl DocumentState {
    pub fn new() -> Self {
        Self {
            open_docs: HashMap::new(),
            uri_keys: HashMap::new(),
            watcher_keys: HashMap::new(),
            generations: HashMap::new(),
            event_hook: None,
            emit_open_change_events: true,
        }
    }

    pub fn set_event_hook(&mut self, hook: DocumentEventHook) {
        self.event_hook = Some(hook);
    }

    pub fn set_open_change_events(&mut self, enabled: bool) {
        self.emit_open_change_events = enabled;
    }

    pub fn planned_zed_open(&self, incoming_uri: &str, text: String) -> DocumentAction {
        let key = self.key_for_uri(incoming_uri);
        match self.open_docs.get(&key) {
            Some(doc) => DocumentAction::Change {
                uri: doc.uri.clone(),
                version: doc.version + 1,
                text,
            },
            None => DocumentAction::Open {
                uri: path_to_uri(&key),
                version: 1,
                text,
            },
        }
    }

    pub fn planned_zed_change(&self, incoming_uri: &str, text: String) -> Option<DocumentAction> {
        let key = self.key_for_uri(incoming_uri);
        let doc = self.open_docs.get(&key)?;
        Some(DocumentAction::Change {
            uri: doc.uri.clone(),
            version: doc.version + 1,
            text,
        })
    }

    pub fn zed_open(&mut self, incoming_uri: &str, text: String) -> DocumentAction {
        let key = self.key_for_uri(incoming_uri);
        self.uri_keys.insert(incoming_uri.to_owned(), key.clone());
        self.watcher_keys.insert(normalize_path(&key), key.clone());
        if let Some(doc) = self.open_docs.get_mut(&key) {
            doc.text = text.clone();
            doc.version += 1;
            doc.owner = DocumentOwner::Zed;
            let uri = doc.uri.clone();
            let version = doc.version;
            let generation = doc.generation;
            let action = DocumentAction::Change {
                uri: uri.clone(),
                version,
                text,
            };
            if self.emit_open_change_events {
                self.emit(DocumentEvent::Change {
                    uri,
                    generation,
                    version,
                });
            }
            return action;
        }

        let uri = path_to_uri(&key);
        let generation = self.next_generation(&key);
        let version = 1;
        self.open_docs.insert(
            key.clone(),
            OpenDoc {
                uri: uri.clone(),
                version,
                generation,
                text: text.clone(),
                owner: DocumentOwner::Zed,
            },
        );
        self.uri_keys.insert(uri.clone(), key.clone());
        if self.emit_open_change_events {
            self.emit(DocumentEvent::Open {
                uri: uri.clone(),
                generation,
                version,
            });
        }
        DocumentAction::Open { uri, version, text }
    }

    pub fn zed_change(&mut self, incoming_uri: &str, text: String) -> Option<DocumentAction> {
        let key = self.key_for_uri(incoming_uri);
        let doc = self.open_docs.get_mut(&key)?;
        doc.text = text.clone();
        doc.version += 1;
        doc.owner = DocumentOwner::Zed;
        let uri = doc.uri.clone();
        let version = doc.version;
        let generation = doc.generation;
        if self.emit_open_change_events {
            self.emit(DocumentEvent::Change {
                uri: uri.clone(),
                generation,
                version,
            });
        }
        Some(DocumentAction::Change { uri, version, text })
    }

    pub fn zed_close(&mut self, incoming_uri: &str) -> Option<(PathBuf, String)> {
        let key = self.key_for_uri(incoming_uri);
        let doc = self.open_docs.remove(&key)?;
        self.remove_mappings(&key);
        self.emit(DocumentEvent::Close {
            uri: doc.uri.clone(),
            generation: doc.generation,
        });
        Some((key, doc.uri))
    }

    pub fn bridge_open_path(&mut self, path: &Path, text: String) -> Option<DocumentAction> {
        let key = canonical_or_normalized(path);
        if let Some(uri) = self.open_docs.get(&key).map(|doc| doc.uri.clone()) {
            self.register_watcher_path(path, key.clone());
            self.uri_keys.insert(uri, key);
            return None;
        }
        let uri = path_to_uri(&key);
        let version = 1;
        let generation = self.next_generation(&key);
        self.open_docs.insert(
            key.clone(),
            OpenDoc {
                uri: uri.clone(),
                version,
                generation,
                text: text.clone(),
                owner: DocumentOwner::Bridge,
            },
        );
        self.uri_keys.insert(uri.clone(), key.clone());
        self.register_watcher_path(path, key.clone());
        if self.emit_open_change_events {
            self.emit(DocumentEvent::Open {
                uri: uri.clone(),
                generation,
                version,
            });
        }
        Some(DocumentAction::Open { uri, version, text })
    }

    pub fn bridge_change_path(&mut self, path: &Path, text: String) -> Option<DocumentAction> {
        let key = canonical_or_normalized(path);
        self.register_watcher_path(path, key.clone());
        let doc = self.open_docs.get_mut(&key)?;
        if doc.owner != DocumentOwner::Bridge || doc.text == text {
            return None;
        }
        doc.text = text.clone();
        doc.version += 1;
        let uri = doc.uri.clone();
        let version = doc.version;
        let generation = doc.generation;
        if self.emit_open_change_events {
            self.emit(DocumentEvent::Change {
                uri: uri.clone(),
                generation,
                version,
            });
        }
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
        self.emit(DocumentEvent::Close {
            uri: doc.uri.clone(),
            generation: doc.generation,
        });
        self.emit(DocumentEvent::Remove {
            uri: doc.uri.clone(),
            generation: doc.generation,
        });
        Some(doc.uri)
    }

    pub fn forget_closed(&mut self, key: &Path, uri: &str) {
        self.remove_mappings(key);
        self.emit(DocumentEvent::Remove {
            uri: uri.to_owned(),
            generation: self.generation_for_key(key),
        });
    }

    pub fn register_watcher_path(&mut self, path: &Path, key: PathBuf) {
        self.watcher_keys.insert(normalize_path(path), key);
    }

    pub fn watcher_key(&self, path: &Path) -> Option<PathBuf> {
        self.watcher_keys.get(&normalize_path(path)).cloned()
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

    fn next_generation(&mut self, key: &Path) -> u64 {
        let generation = self.generations.entry(key.to_path_buf()).or_insert(0);
        *generation += 1;
        *generation
    }

    fn generation_for_key(&self, key: &Path) -> u64 {
        self.generations.get(key).copied().unwrap_or_default()
    }

    fn emit(&self, event: DocumentEvent) {
        if let Some(hook) = &self.event_hook {
            hook(event);
        }
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
    pub text: String,
}

pub fn scan_project(project: &Path, diagnose_addons: bool) -> Vec<ScannedDocument> {
    let project = canonical_or_normalized(project);
    let mut documents = Vec::new();
    scan_directory(&project, &project, diagnose_addons, &mut documents);
    documents.sort_by(|left, right| left.path.cmp(&right.path));
    documents
}

fn scan_directory(
    directory: &Path,
    project: &Path,
    diagnose_addons: bool,
    documents: &mut Vec<ScannedDocument>,
) {
    let mut entries = match std::fs::read_dir(directory) {
        Ok(entries) => entries.flatten().collect::<Vec<_>>(),
        Err(error) => {
            crate::warn!(
                "skipping unreadable diagnostics directory {}: {error}",
                directory.display()
            );
            return;
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
            if directory_is_skipped(&path, project, diagnose_addons) {
                continue;
            }
            scan_directory(&path, project, diagnose_addons, documents);
            continue;
        }
        if !file_type.is_file() || path.extension() != Some(OsStr::new("gd")) {
            continue;
        }
        let Some(text) = read_document(&path) else {
            continue;
        };
        let key = canonical_or_normalized(&path);
        documents.push(ScannedDocument { path, key, text });
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
    let components = relative.components().collect::<Vec<_>>();
    if components.is_empty() {
        return false;
    }
    components[..components.len() - 1].iter().all(|component| {
        let Component::Normal(name) = component else {
            return false;
        };
        *name != OsStr::new(".godot")
            && (diagnose_addons || *name != OsStr::new("addons"))
            && !name.to_string_lossy().starts_with('.')
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum WatcherChangeKind {
    Created,
    Modified,
    Removed,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WatcherChange {
    pub kind: WatcherChangeKind,
    pub path: PathBuf,
}

pub fn watcher_changes(event: Event) -> Vec<WatcherChange> {
    match event.kind {
        EventKind::Create(_) => event
            .paths
            .into_iter()
            .map(|path| WatcherChange {
                kind: WatcherChangeKind::Created,
                path,
            })
            .collect(),
        EventKind::Remove(_) => event
            .paths
            .into_iter()
            .map(|path| WatcherChange {
                kind: WatcherChangeKind::Removed,
                path,
            })
            .collect(),
        EventKind::Modify(ModifyKind::Name(RenameMode::From)) => event
            .paths
            .into_iter()
            .map(|path| WatcherChange {
                kind: WatcherChangeKind::Removed,
                path,
            })
            .collect(),
        EventKind::Modify(ModifyKind::Name(RenameMode::To)) => event
            .paths
            .into_iter()
            .map(|path| WatcherChange {
                kind: WatcherChangeKind::Created,
                path,
            })
            .collect(),
        EventKind::Modify(ModifyKind::Name(RenameMode::Both)) => {
            let mut paths = event.paths.into_iter();
            let Some(old) = paths.next() else {
                return Vec::new();
            };
            let Some(new) = paths.next() else {
                return vec![WatcherChange {
                    kind: WatcherChangeKind::Removed,
                    path: old,
                }];
            };
            vec![
                WatcherChange {
                    kind: WatcherChangeKind::Removed,
                    path: old,
                },
                WatcherChange {
                    kind: WatcherChangeKind::Created,
                    path: new,
                },
            ]
        }
        EventKind::Modify(_) => event
            .paths
            .into_iter()
            .map(|path| WatcherChange {
                kind: WatcherChangeKind::Modified,
                path,
            })
            .collect(),
        _ => Vec::new(),
    }
}

pub struct ProjectWatcher {
    pub(crate) _watcher: RecommendedWatcher,
    pub(crate) receiver: Receiver<notify::Result<Event>>,
}

pub fn watch_project(project: &Path) -> notify::Result<ProjectWatcher> {
    let (sender, receiver) = mpsc::channel(1024);
    let mut watcher = RecommendedWatcher::new(
        move |result| {
            let _ = sender.try_send(result);
        },
        Config::default(),
    )?;
    watcher.watch(project, RecursiveMode::Recursive)?;
    Ok(ProjectWatcher {
        _watcher: watcher,
        receiver,
    })
}

fn directory_is_skipped(path: &Path, project: &Path, diagnose_addons: bool) -> bool {
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
    use std::fs;
    use std::sync::Mutex;
    use tempfile::tempdir;

    #[test]
    fn scan_filters_project_diagnostics_files() {
        let directory = tempdir().unwrap();
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

        let normal = scan_project(directory.path(), false);
        assert_eq!(
            normal
                .iter()
                .map(|doc| doc.path.clone())
                .collect::<Vec<_>>(),
            vec![directory.path().join("valid.gd")]
        );
        let with_addons = scan_project(directory.path(), true);
        assert!(with_addons
            .iter()
            .any(|doc| doc.path.ends_with("addons/addon.gd")));
    }

    #[test]
    fn ownership_transitions_emit_document_events() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("document.gd");
        fs::write(&path, "disk").unwrap();
        let events = Arc::new(Mutex::new(Vec::new()));
        let events_for_hook = Arc::clone(&events);
        let mut state = DocumentState::new();
        state.set_event_hook(Arc::new(move |event| {
            events_for_hook.lock().unwrap().push(event);
        }));

        let opened = state.bridge_open_path(&path, "disk".to_owned()).unwrap();
        let key = crate::root::canonical_or_normalized(&path);
        assert_eq!(state.owner(&key), Some(DocumentOwner::Bridge));
        let uri = match opened {
            DocumentAction::Open { uri, .. } => uri,
            _ => unreachable!(),
        };
        assert!(matches!(
            state.zed_open(&uri, "zed".to_owned()),
            DocumentAction::Change { .. }
        ));
        assert_eq!(state.owner(&key), Some(DocumentOwner::Zed));
        assert!(state.zed_close(&uri).is_some());
        assert!(matches!(
            state.bridge_open_path(&path, "disk".to_owned()),
            Some(DocumentAction::Open { .. })
        ));
        assert!(state.bridge_remove_key(&key).is_some());
        assert!(state.open_docs.is_empty());
        let events = events.lock().unwrap();
        assert!(events
            .iter()
            .any(|event| matches!(event, DocumentEvent::Open { .. })));
        assert!(events
            .iter()
            .any(|event| matches!(event, DocumentEvent::Change { version: 2, .. })));
        assert!(events
            .iter()
            .any(|event| matches!(event, DocumentEvent::Close { .. })));
        assert!(events
            .iter()
            .any(|event| matches!(event, DocumentEvent::Remove { .. })));
    }
}
