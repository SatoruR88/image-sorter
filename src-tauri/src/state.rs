use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// A completed file move that can be undone.
pub struct Operation {
    /// Where the file originally was.
    pub from: PathBuf,
    /// Where it ended up (after collision resolution).
    pub to: PathBuf,
}

/// Cap on undo history so long sessions don't grow memory without bound.
const HISTORY_CAP: usize = 1000;

/// In-memory classification session. The filesystem is the source of truth;
/// `queue` is a snapshot of unclassified images taken at folder-open time.
#[derive(Default)]
pub struct Session {
    pub root: Option<PathBuf>,
    pub queue: Vec<PathBuf>,
    pub index: usize,
    pub history: Vec<Operation>,
    /// Cached category names; refreshed on watcher events and category
    /// creation so per-keypress snapshots do no directory I/O.
    pub categories: Vec<String>,
    /// O(1) membership index for `queue` (kept in sync by mutating methods).
    members: HashSet<PathBuf>,
    /// Monotonic snapshot version so stale async emissions can be dropped.
    pub seq: u64,
    /// Keeps the filesystem watcher alive for the current root.
    pub watcher:
        Option<notify_debouncer_mini::Debouncer<notify::RecommendedWatcher>>,
}

impl Session {
    pub fn reset(
        &mut self,
        root: PathBuf,
        queue: Vec<PathBuf>,
        categories: Vec<String>,
    ) {
        self.root = Some(root);
        self.members = queue.iter().cloned().collect();
        self.queue = queue;
        self.categories = categories;
        self.index = 0;
        self.history.clear();
    }

    /// Close the session entirely (e.g. the root folder was deleted).
    pub fn clear(&mut self) {
        self.root = None;
        self.queue.clear();
        self.members.clear();
        self.categories.clear();
        self.index = 0;
        self.history.clear();
    }

    pub fn current(&self) -> Option<&PathBuf> {
        self.queue.get(self.index)
    }

    pub fn contains(&self, path: &Path) -> bool {
        self.members.contains(path)
    }

    /// Remove the file at `index` from the queue after it left the root folder.
    /// Index then points at the next image automatically.
    pub fn remove_current(&mut self) {
        if self.index < self.queue.len() {
            let p = self.queue.remove(self.index);
            self.members.remove(&p);
        }
        if self.index >= self.queue.len() && self.index > 0 {
            self.index = self.queue.len().saturating_sub(1);
        }
    }

    /// Batch-remove paths known to be gone (watcher events, reconciliation).
    /// Preserves order; adjusts `index` for removals before it.
    pub fn remove_paths(&mut self, paths: &HashSet<PathBuf>) {
        if paths.is_empty() {
            return;
        }
        let before = self
            .queue
            .iter()
            .take(self.index)
            .filter(|p| paths.contains(*p))
            .count();
        self.queue.retain(|p| !paths.contains(p));
        for p in paths {
            self.members.remove(p);
        }
        self.index = self.index.saturating_sub(before);
        if self.index >= self.queue.len() {
            self.index = self.queue.len().saturating_sub(1);
        }
    }

    /// Add a path to the end of the queue if not already present.
    pub fn push_unique(&mut self, path: PathBuf) -> bool {
        if self.members.insert(path.clone()) {
            self.queue.push(path);
            true
        } else {
            false
        }
    }

    /// Rebuild queue membership against a fresh scan while preserving the
    /// order of surviving entries and keeping `index` pointing sensibly.
    /// Used on watcher error (event loss) to re-sync with the filesystem.
    pub fn reconcile(&mut self, scanned: Vec<PathBuf>) {
        let scanned_set: HashSet<PathBuf> = scanned.iter().cloned().collect();
        self.remove_paths(
            &self
                .queue
                .iter()
                .filter(|p| !scanned_set.contains(*p))
                .cloned()
                .collect(),
        );
        for p in scanned {
            self.push_unique(p);
        }
    }

    /// Move `index` by `delta` without touching files.
    pub fn navigate(&mut self, delta: i64) {
        if self.queue.is_empty() {
            self.index = 0;
            return;
        }
        let len = self.queue.len() as i64;
        self.index = (self.index as i64 + delta).clamp(0, len - 1) as usize;
    }

    /// Reinsert a restored path just before the current index so it becomes
    /// current. If the watcher already re-added it elsewhere, it is moved.
    pub fn reinsert(&mut self, path: PathBuf) {
        if self.contains(&path) {
            if let Some(pos) = self.queue.iter().position(|p| *p == path) {
                self.queue.remove(pos);
                if pos < self.index {
                    self.index -= 1;
                }
            }
        }
        let pos = self.index.min(self.queue.len());
        self.queue.insert(pos, path.clone());
        self.members.insert(path);
        self.index = pos;
    }

    /// Record an undoable operation, capping history size.
    pub fn push_history(&mut self, op: Operation) {
        self.history.push(op);
        if self.history.len() > HISTORY_CAP {
            self.history.drain(..self.history.len() - HISTORY_CAP);
        }
    }
}
