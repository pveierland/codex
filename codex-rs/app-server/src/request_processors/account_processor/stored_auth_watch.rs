//! Adopt credentials that another process stores while the app server runs.
//!
//! Account switchers such as `codex-auth switch` replace `auth.json` in place. Without this
//! watch the server keeps the account it loaded at startup until it restarts, and its next token
//! refresh fails because the stored account no longer matches.

use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Weak;
use std::time::Duration;

use codex_file_watcher::DebouncedWatchReceiver;
use codex_file_watcher::FileWatcher;
use codex_file_watcher::WatchPath;
use tokio_util::task::AbortOnDropHandle;
use tracing::warn;

use super::AccountRequestProcessor;

/// Lets a writer finish replacing the file before it is read.
const DEBOUNCE_INTERVAL: Duration = Duration::from_millis(250);

/// Watch `codex_home` for a replaced `auth.json` and let `processor` adopt it.
///
/// The directory is watched rather than the file: writers usually rename a new file over the old
/// one, and a watch on the old file would end with it.
pub(super) fn spawn(
    processor: Weak<AccountRequestProcessor>,
    codex_home: &Path,
) -> Option<AbortOnDropHandle<()>> {
    let file_watcher = match FileWatcher::new() {
        Ok(file_watcher) => Arc::new(file_watcher),
        Err(err) => {
            warn!("failed to watch stored credentials: {err}");
            return None;
        }
    };
    let (subscriber, rx) = file_watcher.add_subscriber();
    let registration = subscriber.register_paths(vec![WatchPath {
        path: codex_home.to_path_buf(),
        recursive: false,
    }]);
    let auth_file: PathBuf = codex_home.join("auth.json");
    let mut rx = DebouncedWatchReceiver::new(rx, DEBOUNCE_INTERVAL);
    Some(AbortOnDropHandle::new(tokio::spawn(async move {
        let _watch = (file_watcher, subscriber, registration);
        while let Some(event) = rx.recv().await {
            if !event.paths.contains(&auth_file) {
                continue;
            }
            let Some(processor) = processor.upgrade() else {
                break;
            };
            processor.adopt_stored_auth().await;
        }
    })))
}
