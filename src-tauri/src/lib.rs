mod fs_ops;
mod settings;
mod state;

use notify::RecursiveMode;
use notify_debouncer_mini::{new_debouncer, DebouncedEvent, Debouncer};
use serde::Serialize;
use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;
use state::Session;
use tauri::{AppHandle, Emitter, Manager, State};

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct SessionInfo {
    /// Monotonic version; the frontend drops stale async snapshots.
    seq: u64,
    root: Option<String>,
    categories: Vec<String>,
    /// 0-based index into the queue.
    index: usize,
    /// Number of unclassified images remaining in the queue.
    remaining: usize,
    /// Absolute path of the current image, if any.
    current: Option<String>,
    /// Upcoming image paths for client-side preloading.
    upcoming: Vec<String>,
    /// Number of undoable operations in history.
    undoable: usize,
}

fn snapshot(session: &mut Session) -> SessionInfo {
    session.seq += 1;
    SessionInfo {
        seq: session.seq,
        root: session
            .root
            .as_ref()
            .map(|r| r.to_string_lossy().into_owned()),
        categories: session.categories.clone(),
        index: session.index,
        remaining: session.queue.len(),
        current: session
            .current()
            .map(|p| p.to_string_lossy().into_owned()),
        upcoming: session
            .queue
            .iter()
            .skip(session.index + 1)
            .take(2)
            .map(|p| p.to_string_lossy().into_owned())
            .collect(),
        undoable: session.history.len(),
    }
}

fn root_of(session: &Session) -> Result<PathBuf, String> {
    session
        .root
        .clone()
        .filter(|r| r.is_dir())
        .ok_or_else(|| "folder is not open or no longer exists".to_string())
}

/// A category name must be a single plain path component so `root.join(name)`
/// can never escape the root folder.
fn resolve_category(root: &Path, name: &str) -> Result<PathBuf, String> {
    let mut components = Path::new(name).components();
    match (components.next(), components.next()) {
        (Some(Component::Normal(_)), None) => {}
        _ => return Err(format!("invalid category name: {name:?}")),
    }
    let dest = root.join(name);
    if dest.is_symlink() || !dest.is_dir() || fs_ops::is_hidden_or_system(&dest) {
        return Err(format!("category folder no longer exists: {name}"));
    }
    Ok(dest)
}

/// Watch `root` non-recursively; reconcile queue/categories with reality and
/// push updated snapshots to the frontend. Only touched paths are re-checked;
/// a full rescan happens only when events were lost.
fn watch_root(app: &AppHandle, root: &Path) -> Option<Debouncer<notify::RecommendedWatcher>> {
    let app = app.clone();
    let mut debouncer = new_debouncer(
        Duration::from_millis(300),
        move |res: Result<Vec<DebouncedEvent>, notify::Error>| {
            let state = app.state::<Mutex<Session>>();
            let Ok(mut s) = state.lock() else { return };
            let Some(root) = s.root.clone() else { return };
            match res {
                Err(_) => {
                    // Events lost (buffer overflow, root deletion, ...) —
                    // re-sync with one bounded scan of the root.
                    if !root.exists() {
                        s.watcher = None;
                        s.clear();
                    } else {
                        if let Ok(images) = fs_ops::list_images(&root) {
                            s.reconcile(images);
                        }
                        s.categories = fs_ops::child_dir_names(&root);
                        if s.index >= s.queue.len() {
                            s.index = s.queue.len().saturating_sub(1);
                        }
                    }
                    let _ = app.emit("session", snapshot(&mut s));
                }
                Ok(events) => {
                    let mut dirty = false;
                    let mut dirs_changed = false;
                    let mut gone: HashSet<PathBuf> = HashSet::new();
                    for ev in events {
                        let p = ev.path;
                        if p.parent() != Some(root.as_path()) {
                            // Covers `p == root` too — deletion of the watched
                            // dir itself surfaces as Err/child events anyway.
                            continue;
                        }
                        if p.is_dir()
                            || p
                                .file_name()
                                .and_then(|n| n.to_str())
                                .map(|n| s.categories.iter().any(|c| c == n))
                                .unwrap_or(false)
                        {
                            dirs_changed = true;
                        }
                        if fs_ops::is_image(&p) {
                            if p.is_file() {
                                if !fs_ops::is_hidden_or_system(&p) && s.push_unique(p) {
                                    dirty = true;
                                }
                            } else {
                                gone.insert(p);
                            }
                        }
                    }
                    if !gone.is_empty() {
                        s.remove_paths(&gone);
                        dirty = true;
                    }
                    if dirs_changed {
                        s.categories = fs_ops::child_dir_names(&root);
                        dirty = true;
                    }
                    if !root.exists() {
                        s.watcher = None;
                        s.clear();
                        dirty = true;
                    }
                    if dirty {
                        if s.index >= s.queue.len() {
                            s.index = s.queue.len().saturating_sub(1);
                        }
                        let _ = app.emit("session", snapshot(&mut s));
                    }
                }
            }
        },
    )
    .ok()?;
    if debouncer
        .watcher()
        .watch(root, RecursiveMode::NonRecursive)
        .is_err()
    {
        return None;
    }
    Some(debouncer)
}

/// Shared setup for opening a root folder: start the watcher (before scanning
/// so the gap self-heals), scan, reset session, adjust the asset scope.
fn activate_root(app: &AppHandle, session: &mut Session, root: PathBuf) -> Result<(), String> {
    let watcher = watch_root(app, &root);
    let queue = fs_ops::list_images(&root)?;
    let categories = fs_ops::child_dir_names(&root);
    if let Some(old) = session.root.take() {
        if old != root {
            let _ = app.asset_protocol_scope().forbid_directory(&old, false);
        }
    }
    session.reset(root.clone(), queue, categories);
    session.watcher = watcher;
    // Only direct children of root are ever displayed — non-recursive scope.
    let _ = app.asset_protocol_scope().allow_directory(&root, false);
    Ok(())
}

#[tauri::command]
fn open_folder(
    app: AppHandle,
    session: State<Mutex<Session>>,
    path: String,
) -> Result<SessionInfo, String> {
    let raw = PathBuf::from(&path);
    // Canonicalize (without the \\?\ prefix) so watcher event paths, which are
    // compared by component equality, always share the stored prefix.
    let root = dunce::canonicalize(&raw).unwrap_or(raw);
    if !root.is_dir() {
        return Err(format!("not a folder: {path}"));
    }
    let mut s = session.lock().map_err(|e| e.to_string())?;
    activate_root(&app, &mut s, root.clone())?;
    settings::save_last_folder(&app, &root.to_string_lossy());
    Ok(snapshot(&mut s))
}

#[tauri::command]
fn classify(
    session: State<Mutex<Session>>,
    category: String,
    expected: String,
) -> Result<SessionInfo, String> {
    let mut s = session.lock().map_err(|e| e.to_string())?;
    let root = root_of(&s)?;
    let dest_dir = resolve_category(&root, &category)?;
    let src = s
        .current()
        .cloned()
        .ok_or_else(|| "no current image".to_string())?;
    if src.to_string_lossy() != expected {
        // The queue shifted under the user (watcher activity) — resync
        // instead of moving a file they never saw.
        return Ok(snapshot(&mut s));
    }
    if !src.is_file() {
        // File vanished outside the app — drop it and move on.
        s.remove_current();
        return Ok(snapshot(&mut s));
    }
    let dest = fs_ops::move_file(&src, &dest_dir)?;
    s.push_history(state::Operation {
        from: src,
        to: dest,
    });
    s.remove_current();
    Ok(snapshot(&mut s))
}

#[tauri::command]
fn navigate(session: State<Mutex<Session>>, delta: i64) -> Result<SessionInfo, String> {
    let mut s = session.lock().map_err(|e| e.to_string())?;
    s.navigate(delta);
    Ok(snapshot(&mut s))
}

#[tauri::command]
fn undo(session: State<Mutex<Session>>) -> Result<SessionInfo, String> {
    let mut s = session.lock().map_err(|e| e.to_string())?;
    let op = s
        .history
        .pop()
        .ok_or_else(|| "nothing to undo".to_string())?;
    if !op.to.is_file() {
        return Err("the moved file no longer exists".to_string());
    }
    let Some(parent) = op.from.parent().map(Path::to_path_buf) else {
        s.history.push(op);
        return Err("original folder is gone".to_string());
    };
    match fs_ops::move_file(&op.to, &parent) {
        Ok(restored) => s.reinsert(restored),
        Err(e) => {
            s.history.push(op);
            return Err(e);
        }
    }
    Ok(snapshot(&mut s))
}

#[tauri::command]
fn delete_current(
    session: State<Mutex<Session>>,
    expected: String,
) -> Result<SessionInfo, String> {
    let mut s = session.lock().map_err(|e| e.to_string())?;
    let src = s
        .current()
        .cloned()
        .ok_or_else(|| "no current image".to_string())?;
    if src.to_string_lossy() != expected {
        return Ok(snapshot(&mut s));
    }
    if !src.is_file() {
        // Already gone — drop it from the queue so the user can continue.
        s.remove_current();
        return Ok(snapshot(&mut s));
    }
    fs_ops::recycle_file(&src)?;
    s.remove_current();
    Ok(snapshot(&mut s))
}

#[tauri::command]
fn create_category(
    session: State<Mutex<Session>>,
    name: String,
) -> Result<SessionInfo, String> {
    let mut s = session.lock().map_err(|e| e.to_string())?;
    let root = root_of(&s)?;
    fs_ops::create_category(&root, &name)?;
    s.categories = fs_ops::child_dir_names(&root);
    Ok(snapshot(&mut s))
}

#[tauri::command]
fn get_state(session: State<Mutex<Session>>) -> Result<SessionInfo, String> {
    let mut s = session.lock().map_err(|e| e.to_string())?;
    Ok(snapshot(&mut s))
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_window_state::Builder::default().build())
        .manage(Mutex::new(Session::default()))
        .setup(|app| {
            // Reopen the previously used folder if it still exists.
            let last = settings::load(&app.handle()).last_folder;
            if let Some(path) = last {
                let raw = PathBuf::from(&path);
                let root = dunce::canonicalize(&raw).unwrap_or(raw);
                if root.is_dir() {
                    let session = app.state::<Mutex<Session>>();
                    if let Ok(mut s) = session.lock() {
                        let _ = activate_root(&app.handle(), &mut s, root);
                    };
                }
            }
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            open_folder,
            classify,
            navigate,
            undo,
            delete_current,
            create_category,
            get_state,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
