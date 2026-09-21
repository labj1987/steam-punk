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
    /// `find_running_appid`), but reliable cover art requires knowing the
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

fn load_meta() -> HashMap<String, TrainerMeta> {
    let Ok(path) = meta_path() else {
        return HashMap::new();
    };
    let Ok(contents) = std::fs::read_to_string(&path) else {
        return HashMap::new();
    };
    serde_json::from_str(&contents).unwrap_or_default()
}

fn save_meta(meta: &HashMap<String, TrainerMeta>) -> Result<()> {
    let path = meta_path()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(meta)?;
    std::fs::write(&path, json).with_context(|| format!("writing {}", path.display()))
}

/// Associate a Steam AppID with an already-imported trainer, keyed by its
/// filename. Overwrites any existing association for that trainer.
pub fn set_trainer_appid(filename: &str, appid: u32) -> Result<()> {
    let mut meta = load_meta();
    meta.insert(
        filename.to_string(),
        TrainerMeta {
            appid: Some(appid),
        },
    );
    save_meta(&meta)
}

/// `~/.local/share/steampunk/` — the app's data dir (trainers, logs,
/// trainer metadata, cover-art cache).
pub fn data_dir() -> Result<PathBuf> {
    let home = std::env::var("HOME").context("HOME not set")?;
    Ok(PathBuf::from(home).join(".local/share/steampunk"))
}

/// One-time move of the pre-rename data dir (`~/.local/share/proton-trainer`)
/// to the new location, so existing imported trainers survive the rebrand.
/// Must run before anything (applog included) touches the data dir.
pub fn migrate_legacy_data_dir() {
    let Ok(new_dir) = data_dir() else { return };
    let Ok(home) = std::env::var("HOME") else { return };
    let old_dir = PathBuf::from(home).join(".local/share/proton-trainer");
    if old_dir.is_dir() && !new_dir.exists() {
        let _ = std::fs::rename(&old_dir, &new_dir);
    }
}

/// `~/.local/share/steampunk/trainers/` — where imported trainers live,
/// flat, no per-game folders.
pub fn trainers_dir() -> Result<PathBuf> {
    Ok(data_dir()?.join("trainers"))
}

pub fn list_trainers() -> Result<Vec<Trainer>> {
    let dir = trainers_dir()?;
    if !dir.is_dir() {
        return Ok(Vec::new());
    }

    let meta = load_meta();

    let mut trainers: Vec<Trainer> = std::fs::read_dir(&dir)?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_file() && p.extension().and_then(|e| e.to_str()).is_some_and(|e| e.eq_ignore_ascii_case("exe")))
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
    Ok(trainers)
}

/// Copy a dropped/picked .exe into the managed trainers dir, flat, keeping
/// its filename. Overwrites an existing trainer of the same name.
pub fn import_trainer(src: &Path) -> Result<PathBuf> {
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

    // Best-effort: drop the AppID association too, so re-adding the same
    // filename later doesn't inherit a stale one.
    if let Some(filename) = path.file_name().map(|n| n.to_string_lossy().into_owned()) {
        let mut meta = load_meta();
        if meta.remove(&filename).is_some() {
            let _ = save_meta(&meta);
        }
    }
    Ok(())
}
