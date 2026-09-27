use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};

pub const IMAGE_EXTENSIONS: [&str; 6] = ["jpg", "jpeg", "png", "webp", "bmp", "gif"];

fn is_hidden_name(path: &Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .map(|name| name.starts_with('.') || name.eq_ignore_ascii_case("$RECYCLE.BIN"))
        .unwrap_or(false)
}

#[cfg(windows)]
fn has_hidden_attrs(meta: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    const FILE_ATTRIBUTE_HIDDEN: u32 = 0x2;
    const FILE_ATTRIBUTE_SYSTEM: u32 = 0x4;
    meta.file_attributes() & (FILE_ATTRIBUTE_HIDDEN | FILE_ATTRIBUTE_SYSTEM) != 0
}

#[cfg(not(windows))]
fn has_hidden_attrs(_meta: &fs::Metadata) -> bool {
    false
}

/// Hidden/system check usable outside of a directory scan (e.g. the watcher,
/// where no DirEntry is available). Does one metadata syscall.
pub(crate) fn is_hidden_or_system(path: &Path) -> bool {
    is_hidden_name(path)
        || path.metadata().map(|m| has_hidden_attrs(&m)).unwrap_or(false)
}

/// Sort key approximating Explorer's case-insensitive ordering.
fn sort_key(path: &Path) -> String {
    path.to_string_lossy().to_lowercase()
}

pub fn is_image(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| IMAGE_EXTENSIONS.iter().any(|x| x.eq(&e.to_ascii_lowercase())))
        .unwrap_or(false)
}

/// List image files directly inside `dir` (non-recursive), sorted by name.
/// `DirEntry::file_type`/`metadata` are free on Windows (no extra syscalls).
pub fn list_images(dir: &Path) -> Result<Vec<PathBuf>, String> {
    let entries =
        fs::read_dir(dir).map_err(|e| format!("cannot read folder {}: {e}", dir.display()))?;
    let mut images = Vec::new();
    for entry in entries.flatten() {
        let (Ok(file_type), Ok(meta)) = (entry.file_type(), entry.metadata()) else {
            continue;
        };
        let path = entry.path();
        if file_type.is_file()
            && is_image(&path)
            && !is_hidden_name(&path)
            && !has_hidden_attrs(&meta)
        {
            images.push(path);
        }
    }
    images.sort_by_key(|p| sort_key(p));
    Ok(images)
}

/// List direct child directories of `dir` (category destinations), sorted by
/// name. Symlinks/junctions are skipped so classification can never move a
/// file outside the selected root via a reparse point.
pub fn list_child_dirs(dir: &Path) -> Result<Vec<PathBuf>, String> {
    let entries =
        fs::read_dir(dir).map_err(|e| format!("cannot read folder {}: {e}", dir.display()))?;
    let mut dirs = Vec::new();
    for entry in entries.flatten() {
        let (Ok(file_type), Ok(meta)) = (entry.file_type(), entry.metadata()) else {
            continue;
        };
        if file_type.is_symlink() {
            continue;
        }
        let path = entry.path();
        if file_type.is_dir() && !is_hidden_name(&path) && !has_hidden_attrs(&meta) {
            dirs.push(path);
        }
    }
    dirs.sort_by_key(|p| sort_key(p));
    Ok(dirs)
}

/// Names of direct child directories (UTF-8 names only; non-UTF-8 names are
/// skipped since they cannot be addressed through the IPC boundary anyway).
pub fn child_dir_names(dir: &Path) -> Vec<String> {
    list_child_dirs(dir)
        .map(|dirs| {
            dirs.iter()
                .filter_map(|d| d.file_name().and_then(|n| n.to_str()).map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

/// Atomically claim a non-colliding destination path inside `dest_dir` for a
/// file named `file_name` by exclusively creating an empty placeholder there.
/// `IMG_001.jpg` -> `IMG_001_1.jpg`, `IMG_001_2.jpg`, ... Unlike an `exists()`
/// check followed by a separate `rename`, this leaves no window in which a
/// concurrently created file at the same name would be silently clobbered by
/// `rename`'s overwrite-on-existing behavior. Works on OsStr so non-UTF-8
/// file names are handled too.
fn claim_dest_path(dest_dir: &Path, file_name: &OsStr) -> Result<PathBuf, String> {
    let name = Path::new(file_name);
    let stem = name.file_stem().unwrap_or(file_name);
    let ext = name.extension();

    let mut candidate = dest_dir.join(file_name);
    let mut suffix = 0u32;
    loop {
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            Ok(_) => return Ok(candidate),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                suffix += 1;
                let mut new_name = stem.to_os_string();
                new_name.push(format!("_{suffix}"));
                if let Some(e) = ext {
                    new_name.push(".");
                    new_name.push(e);
                }
                candidate = dest_dir.join(&new_name);
            }
            Err(e) => {
                return Err(format!(
                    "failed to reserve destination name in {}: {e}",
                    dest_dir.display()
                ))
            }
        }
    }
}

/// Move `src` into `dest_dir`, auto-renaming on collision. Returns the final path.
pub fn move_file(src: &Path, dest_dir: &Path) -> Result<PathBuf, String> {
    if !src.is_file() {
        return Err(format!("source file no longer exists: {}", src.display()));
    }
    if !dest_dir.is_dir() {
        return Err(format!(
            "destination folder no longer exists: {}",
            dest_dir.display()
        ));
    }
    let file_name = src
        .file_name()
        .ok_or_else(|| "source file has no file name".to_string())?;
    let dest = claim_dest_path(dest_dir, file_name)?;
    fs::rename(src, &dest).map_err(|e| {
        // Best-effort cleanup of the empty placeholder we just claimed, so a
        // failed move doesn't leave a stray zero-byte file behind.
        let _ = fs::remove_file(&dest);
        format!(
            "failed to move {} -> {}: {e}",
            src.display(),
            dest.display()
        )
    })?;
    Ok(dest)
}

/// Reserved characters forbidden in Windows file names.
fn is_valid_dir_name(name: &str) -> bool {
    if name.is_empty() || name.chars().count() > 100 {
        return false;
    }
    if name != name.trim() || name.ends_with('.') || name.starts_with('.') {
        return false;
    }
    // Would be filtered from category listings anyway — reject up front.
    if name.eq_ignore_ascii_case("$RECYCLE.BIN") {
        return false;
    }
    const INVALID: [char; 9] = ['<', '>', ':', '"', '/', '\\', '|', '?', '*'];
    if name.chars().any(|c| INVALID.contains(&c) || (c as u32) < 0x20) {
        return false;
    }
    const RESERVED: [&str; 22] = [
        "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
        "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
    ];
    // Windows reserves the base name: "CON" and "CON.txt" are both invalid.
    let stem = name.split('.').next().unwrap_or("").to_uppercase();
    !RESERVED.contains(&stem.as_str())
}

/// Create a category folder directly inside `root`. Returns its path.
pub fn create_category(root: &Path, name: &str) -> Result<PathBuf, String> {
    if !is_valid_dir_name(name) {
        return Err(format!("invalid folder name: {name:?}"));
    }
    let path = root.join(name);
    // Defense in depth: the created path must stay directly under root.
    if path.parent() != Some(root) {
        return Err("invalid folder name".to_string());
    }
    if path.exists() {
        return Err(format!("folder already exists: {name}"));
    }
    fs::create_dir(&path).map_err(|e| format!("failed to create folder {name}: {e}"))?;
    Ok(path)
}

/// Send a file to the Windows Recycle Bin. Never falls back to permanent deletion.
#[cfg(windows)]
pub fn recycle_file(path: &Path) -> Result<(), String> {
    trash::delete(path).map_err(|e| format!("failed to recycle {}: {e}", path.display()))
}

#[cfg(not(windows))]
pub fn recycle_file(path: &Path) -> Result<(), String> {
    trash::delete(path).map_err(|e| format!("failed to trash {}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "image_sorter_test_{}_{}_{}",
            tag,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn lists_only_direct_images() {
        let root = temp_dir("list");
        File::create(root.join("a.jpg")).unwrap();
        File::create(root.join("b.PNG")).unwrap();
        File::create(root.join("c.txt")).unwrap();
        fs::create_dir(root.join("sub")).unwrap();
        File::create(root.join("sub").join("d.jpg")).unwrap();

        let images = list_images(&root).unwrap();
        assert_eq!(images.len(), 2);
        assert!(images[0].ends_with("a.jpg"));

        let dirs = list_child_dirs(&root).unwrap();
        assert_eq!(dirs, vec![root.join("sub")]);
    }

    #[test]
    fn move_renames_on_collision() {
        let root = temp_dir("coll");
        let dest_dir = root.join("dest");
        fs::create_dir(&dest_dir).unwrap();
        File::create(dest_dir.join("a.jpg")).unwrap();
        File::create(dest_dir.join("a_1.jpg")).unwrap();
        let src = root.join("a.jpg");
        File::create(&src).unwrap();

        let dest = move_file(&src, &dest_dir).unwrap();
        assert!(dest.ends_with("a_2.jpg"));
        assert!(dest.exists());
        assert!(!src.exists());
    }

    #[test]
    fn rejects_bad_category_names() {
        let root = temp_dir("cat");
        for bad in ["", "a/b", "..", "x\\y", "CON", "name.", " a"] {
            assert!(create_category(&root, bad).is_err(), "should reject {bad:?}");
        }
        let ok = create_category(&root, "Screenshots").unwrap();
        assert!(ok.is_dir());
        assert_eq!(ok.parent(), Some(root.as_path()));
        assert!(create_category(&root, "Screenshots").is_err());
    }

    #[test]
    fn move_fails_when_source_gone() {
        let root = temp_dir("gone");
        let dest_dir = root.join("dest");
        fs::create_dir(&dest_dir).unwrap();
        let src = root.join("missing.jpg");
        assert!(move_file(&src, &dest_dir).is_err());
    }
}
