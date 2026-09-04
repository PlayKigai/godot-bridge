pub(crate) use crate::sys::{watch_project_into, ProjectWatcher};

#[cfg(test)]
pub(crate) mod test_support {
    use crate::docs_state::{WatcherChange, WatcherChangeKind};
    use crate::lsp::ProxyEvent;
    use std::path::PathBuf;
    use std::sync::mpsc::Receiver;
    use std::time::Duration;

    pub(crate) fn receive_change(receiver: &Receiver<ProxyEvent>) -> WatcherChange {
        let Ok(ProxyEvent::Watcher(change)) = receiver.recv_timeout(Duration::from_secs(2)) else {
            panic!("no watcher event within two seconds");
        };
        change.unwrap()
    }

    /// Windows reports a rename as two changes and may repeat a write, so the
    /// wanted changes are matched against a stream that may hold extras.
    pub(crate) fn wait_for(
        receiver: &Receiver<ProxyEvent>,
        mut wanted: Vec<(WatcherChangeKind, PathBuf)>,
    ) {
        while !wanted.is_empty() {
            let change = receive_change(receiver);
            wanted.retain(|(kind, path)| *kind != change.kind || *path != change.path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::docs_state::WatcherChangeKind;
    use crate::temp::TempDir;
    use crate::watch::test_support::wait_for;
    use std::fs;
    use std::sync::mpsc;

    #[test]
    fn watches_file_lifecycle() {
        let directory = TempDir::new().unwrap();
        let (sender, receiver) = mpsc::sync_channel(4096);
        let _watcher = watch_project_into(directory.path(), false, sender).unwrap();
        let path = directory.path().join("file.gd");
        fs::write(&path, "one").unwrap();
        wait_for(
            &receiver,
            vec![
                (WatcherChangeKind::Created, path.clone()),
                (WatcherChangeKind::Modified, path.clone()),
            ],
        );

        fs::write(&path, "two").unwrap();
        wait_for(&receiver, vec![(WatcherChangeKind::Modified, path.clone())]);

        let renamed = directory.path().join("renamed.gd");
        fs::rename(&path, &renamed).unwrap();
        wait_for(
            &receiver,
            vec![
                (WatcherChangeKind::Removed, path.clone()),
                (WatcherChangeKind::Created, renamed.clone()),
            ],
        );

        fs::remove_file(&renamed).unwrap();
        wait_for(&receiver, vec![(WatcherChangeKind::Removed, renamed)]);
    }

    #[test]
    fn watches_directories_created_after_start() {
        let directory = TempDir::new().unwrap();
        let (sender, receiver) = mpsc::sync_channel(4096);
        let _watcher = watch_project_into(directory.path(), false, sender).unwrap();
        let nested = directory.path().join("nested");
        fs::create_dir(&nested).unwrap();
        wait_for(
            &receiver,
            vec![(WatcherChangeKind::Rescan, directory.path().to_owned())],
        );
        let path = nested.join("file.gd");
        fs::write(&path, "one").unwrap();
        wait_for(&receiver, vec![(WatcherChangeKind::Created, path)]);
    }

    #[test]
    fn moved_directories_trigger_rescan() {
        let outside = TempDir::new().unwrap();
        let directory = TempDir::new().unwrap();
        let (sender, receiver) = mpsc::sync_channel(4096);
        let _watcher = watch_project_into(directory.path(), false, sender).unwrap();
        let source = outside.path().join("scripts");
        fs::create_dir(&source).unwrap();
        fs::write(source.join("a.gd"), "one").unwrap();
        let moved = directory.path().join("scripts");
        fs::rename(&source, &moved).unwrap();
        wait_for(
            &receiver,
            vec![(WatcherChangeKind::Rescan, directory.path().to_owned())],
        );
        fs::write(moved.join("b.gd"), "two").unwrap();
        wait_for(
            &receiver,
            vec![(WatcherChangeKind::Created, moved.join("b.gd"))],
        );
        fs::rename(&moved, outside.path().join("gone")).unwrap();
        wait_for(
            &receiver,
            vec![(WatcherChangeKind::Rescan, directory.path().to_owned())],
        );
    }
}
