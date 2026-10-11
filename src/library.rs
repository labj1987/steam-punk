use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug)]
pub struct Trainer {
    /// File stem (e.g. "CrimsonDesert_1_13") — the display name when no
    /// AppID is set (see `display_name`). Once a resolved game name is
    /// available it takes over the row entirely; this is no longer shown
    /// anywhere in that case.
    pub name: String,
    pub path: PathBuf,
    /// Optional per-trainer Steam AppID association, set at import time.
    /// This is new: trainers were originally deliberately unassociated with
    /// any game (matched to whatever's running only at launch, via
    /// `launcher::find_running_appids`), but reliable cover art requires knowing the
    /// AppID up front, so this reverses that decision for trainers that
    /// opt in. Fully optional — see `display_name`.
    pub appid: Option<u32>,
    /// Resolved from the Steam Store API and cached locally under
    /// `data_dir()/cache/`. `None` until a fetch has succeeded (or if no
    /// AppID is set), in which case the UI falls back to `name`.
    pub game_name: Option<String>,
    /// Cached cover-art image path, if a fetch has succeeded.
    pub cover_path: Option<PathBuf>,
}

impl Trainer {
    /// The resolved game name if available, else the filename-derived
    /// title — today's behavior for trainers with no AppID association.
    pub fn display_name(&self) -> &str {
        self.game_name.as_deref().unwrap_or(&self.name)
    }
}

/// Per-trainer metadata, keyed by filename, persisted as JSON alongside the
/// trainers themselves. Only holds the AppID association today — the
/// resolved name/art live in the separate `gamedata` cache, keyed by AppID
/// instead of filename, since multiple trainers can share one AppID.
#[derive(Serialize, Deserialize, Default, Clone)]
struct TrainerMeta {
    appid: Option<u32>,
}

fn meta_path() -> Result<PathBuf> {
    Ok(data_dir()?.join("trainers.json"))
}

/// The AppID associations. A missing file is simply "none yet"; a file that
/// exists but can't be read or parsed is an error, never an empty map, so
/// that nothing goes on to save over it and wipe every association.
fn load_meta_from(path: &Path) -> Result<HashMap<String, TrainerMeta>> {
    let contents = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(HashMap::new()),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    serde_json::from_str(&contents).with_context(|| {
        format!(
            "{} is damaged, so the trainers' Steam AppIDs can't be read; it was left as it is",
            path.display()
        )
    })
}

/// Read-modify-write of the associations. Refuses (with the parse error)
/// when the existing file is unreadable, and replaces it atomically
/// otherwise, so an interrupted save can't leave a torn file behind.
fn update_meta_at(path: &Path, change: impl FnOnce(&mut HashMap<String, TrainerMeta>) -> bool) -> Result<()> {
    let mut meta = load_meta_from(path)?;
    if !change(&mut meta) {
        return Ok(());
    }
    let json = serde_json::to_string_pretty(&meta)?;
    crate::fsutil::write_atomic(path, json.as_bytes())
}

/// Associate a Steam AppID with an already-imported trainer, keyed by its
/// filename. Overwrites any existing association for that trainer.
pub fn set_trainer_appid(filename: &str, appid: u32) -> Result<()> {
    update_meta_at(&meta_path()?, |meta| {
        meta.insert(filename.to_string(), TrainerMeta { appid: Some(appid) });
        true
    })
}

/// `~/.local/share/steam-punk/` — the app's data dir (trainers, logs,
/// trainer metadata, cover-art cache).
pub fn data_dir() -> Result<PathBuf> {
    let home = std::env::var("HOME").context("HOME not set")?;
    Ok(PathBuf::from(home).join(".local/share/steam-punk"))
}

/// `~/.local/share/steam-punk/trainers/` — where imported trainers live,
/// flat, no per-game folders.
pub fn trainers_dir() -> Result<PathBuf> {
    Ok(data_dir()?.join("trainers"))
}

/// The trainer list, plus the reason the AppID associations couldn't be
/// read, if they couldn't: the trainers are still listed (by filename) so
/// they stay launchable, and the caller reports the problem.
pub struct Listing {
    pub trainers: Vec<Trainer>,
    pub meta_error: Option<String>,
}

pub fn list_trainers() -> Result<Listing> {
    let dir = trainers_dir()?;
    let (meta, meta_error) = match meta_path().and_then(|p| load_meta_from(&p)) {
        Ok(meta) => (meta, None),
        Err(e) => {
            crate::applog::log(&format!("list_trainers: {e:#}"));
            (HashMap::new(), Some(format!("{e:#}")))
        }
    };
    if !dir.is_dir() {
        return Ok(Listing { trainers: Vec::new(), meta_error });
    }

    let mut trainers: Vec<Trainer> = std::fs::read_dir(&dir)?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_file() && is_exe(p))
        .map(|path| {
            let filename = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            let name = path
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.to_string_lossy().into_owned());
            let appid = meta.get(&filename).and_then(|m| m.appid);
            // Cache reads only — no network access here, so this runs on
            // every list refresh (including app startup) without ever
            // re-fetching. A cache miss (fetch never ran, or failed) just
            // means falling back to the filename, matching trainers that
            // never had an AppID at all.
            let (game_name, cover_path) = match appid {
                Some(id) => (
                    crate::gamedata::cached_name(id),
                    crate::gamedata::cached_cover(id),
                ),
                None => (None, None),
            };
            Trainer {
                name,
                path,
                appid,
                game_name,
                cover_path,
            }
        })
        .collect();

    trainers.sort_by(|a, b| a.display_name().cmp(b.display_name()));
    Ok(Listing { trainers, meta_error })
}

/// Trainers are Windows executables; the list only ever shows `.exe` files.
pub fn is_exe(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("exe"))
}

/// Copy a dropped/picked .exe into the managed trainers dir, flat, keeping
/// its filename. Overwrites an existing trainer of the same name.
pub fn import_trainer(src: &Path) -> Result<PathBuf> {
    // Anything else would be copied in and then never listed.
    if !is_exe(src) {
        anyhow::bail!("{} is not a trainer .exe", src.display());
    }
    let dir = trainers_dir()?;
    std::fs::create_dir_all(&dir)?;

    let filename = src
        .file_name()
        .context("dropped file has no filename")?;
    let dest = dir.join(filename);

    // Re-importing a file that already is the library copy: copying onto
    // itself would truncate it to zero bytes, so do nothing.
    if let (Ok(a), Ok(b)) = (std::fs::canonicalize(src), std::fs::canonicalize(&dest)) {
        if a == b {
            return Ok(dest);
        }
    }

    // Copy to a temp name and rename into place so an interrupted copy
    // never leaves a corrupt trainer behind.
    let mut tmp_name = dest.as_os_str().to_owned();
    tmp_name.push(".tmp");
    let tmp = PathBuf::from(tmp_name);
    if let Err(e) = std::fs::copy(src, &tmp).and_then(|_| std::fs::rename(&tmp, &dest)) {
        let _ = std::fs::remove_file(&tmp);
        return Err(anyhow::Error::new(e)
            .context(format!("copying {} to {}", src.display(), dest.display())));
    }
    Ok(dest)
}

pub fn remove_trainer(path: &Path) -> Result<()> {
    std::fs::remove_file(path).with_context(|| format!("removing {}", path.display()))?;

    // Drop the AppID association too, so re-adding the same filename later
    // doesn't inherit a stale one. The trainer itself is already gone, so a
    // failure here is only logged (an unreadable trainers.json is kept as is).
    if let Some(filename) = path.file_name().map(|n| n.to_string_lossy().into_owned()) {
        if let Err(e) = meta_path().and_then(|p| update_meta_at(&p, |meta| meta.remove(&filename).is_some())) {
            crate::applog::log(&format!("remove_trainer: could not drop the AppID for {filename}: {e:#}"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "steam-punk-library-{tag}-{}-{:?}",
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
    fn a_missing_meta_file_is_an_empty_map_and_is_created_on_save() {
        let dir = scratch("missing");
        let path = dir.join("trainers.json");
        assert!(load_meta_from(&path).unwrap().is_empty());
        update_meta_at(&path, |m| {
            m.insert("A.exe".into(), TrainerMeta { appid: Some(7) });
            true
        })
        .unwrap();
        assert_eq!(load_meta_from(&path).unwrap()["A.exe"].appid, Some(7));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_damaged_meta_file_is_an_error_and_is_never_overwritten() {
        let dir = scratch("torn");
        let path = dir.join("trainers.json");
        let torn = b"{\n  \"A.exe\": {\n    \"appid\": 7\n  },\n  \"B.ex";
        std::fs::write(&path, torn).unwrap();

        assert!(load_meta_from(&path).is_err());
        let err = update_meta_at(&path, |m| {
            m.insert("C.exe".into(), TrainerMeta { appid: Some(9) });
            true
        })
        .unwrap_err();
        assert!(format!("{err:#}").contains("left as it is"), "{err:#}");
        assert_eq!(std::fs::read(&path).unwrap(), torn);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn only_exe_files_count_as_trainers() {
        assert!(is_exe(Path::new("/x/Game Trainer.EXE")));
        assert!(is_exe(Path::new("/x/t.exe")));
        assert!(!is_exe(Path::new("/x/readme.txt")));
        assert!(!is_exe(Path::new("/x/noext")));
    }

    #[test]
    fn importing_a_non_exe_is_refused() {
        let err = import_trainer(Path::new("/nonexistent/readme.txt")).unwrap_err();
        assert!(err.to_string().contains("not a trainer .exe"), "{err}");
    }
}
