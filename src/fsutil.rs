//! fsutil.rs — crash-safe file replacement for the app's own data files
//! (`trainers.json` and the cover/name cache).

use anyhow::{Context, Result};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// Replace `path` with `contents` so that a reader (or the next app start
/// after a crash or power loss) sees either the old file or the complete new
/// one, never a torn mix: the bytes go to a temp file in the same directory,
/// which is flushed to disk and then renamed over the target, and the
/// directory itself is synced so the rename is durable too.
pub fn write_atomic(path: &Path, contents: &[u8]) -> Result<()> {
    let dir = path
        .parent()
        .with_context(|| format!("{} has no parent directory", path.display()))?;
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;

    let tmp = temp_path(path);
    let result = (|| -> std::io::Result<()> {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)?;
        f.write_all(contents)?;
        f.sync_all()?;
        std::fs::rename(&tmp, path)?;
        // The rename is only durable once the directory entry is on disk.
        std::fs::File::open(dir)?.sync_all()
    })();
    if let Err(e) = result {
        let _ = std::fs::remove_file(&tmp);
        return Err(e).with_context(|| format!("writing {}", path.display()));
    }
    Ok(())
}

/// A temp name next to `path` that no other writer uses: several cache
/// writes for the same AppID can run at once (a retry racing an import).
fn temp_path(path: &Path) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    path.with_file_name(format!(".{name}.{}.{n}.tmp", std::process::id()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "steam-punk-fsutil-{tag}-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default()
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn replaces_the_file_and_leaves_no_temp_behind() {
        let dir = scratch("replace");
        let path = dir.join("trainers.json");
        std::fs::write(&path, b"old").unwrap();
        write_atomic(&path, b"new contents").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"new contents");
        let names: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name())
            .collect();
        assert_eq!(names, vec![std::ffi::OsString::from("trainers.json")]);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn creates_missing_parent_directories() {
        let dir = scratch("parent");
        let path = dir.join("cache/42.name.txt");
        write_atomic(&path, b"Game").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"Game");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
