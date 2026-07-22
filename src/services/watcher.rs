//! Non-recursive directory watching. Events are forwarded over a channel the
//! owning pane drains on the UI executor; the watcher stops when dropped.

use std::path::Path;

use futures::channel::mpsc::{UnboundedReceiver, unbounded};
use notify::{RecommendedWatcher, RecursiveMode, Watcher};

pub struct DirWatcher {
    _watcher: RecommendedWatcher,
}

impl DirWatcher {
    /// Watch a directory; each relevant filesystem event pushes one unit onto
    /// the returned channel.
    pub fn watch(dir: &Path) -> anyhow::Result<(Self, UnboundedReceiver<()>)> {
        let (tx, rx) = unbounded::<()>();
        let mut watcher =
            notify::recommended_watcher(move |result: notify::Result<notify::Event>| {
                if let Ok(event) = result {
                    use notify::EventKind;
                    if matches!(
                        event.kind,
                        EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_)
                    ) {
                        let _ = tx.unbounded_send(());
                    }
                }
            })?;
        watcher.watch(dir, RecursiveMode::NonRecursive)?;
        Ok((Self { _watcher: watcher }, rx))
    }
}
