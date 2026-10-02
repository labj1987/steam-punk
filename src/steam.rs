use std::path::{Path, PathBuf};

/// Locate the Steam client installation directory: the first of the usual
/// install locations that actually has a steamapps/ subdirectory.
pub fn steam_client_dir() -> Option<PathBuf> {
    let home = std::env::var("HOME").ok()?;
    for candidate in [
        format!("{home}/.local/share/Steam"),
        format!("{home}/.steam/steam"),
    ] {
        let dir = PathBuf::from(candidate);
        if dir.join("steamapps").is_dir() {
            return Some(dir);
        }
    }
    None
}

/// Every Steam client install that has local game metadata (appmanifests and
/// the library art cache): native Steam and Flatpak Steam. Used only for
/// looking up names and cover art, never for launching, so a Flatpak install
/// is included here even though trainers are launched against the native one.
/// `~/.steam/steam` is usually a symlink to `~/.local/share/Steam`, so
/// duplicates are dropped by canonical path.
pub fn metadata_roots() -> Vec<PathBuf> {
    match std::env::var("HOME") {
        Ok(home) => metadata_roots_in(Path::new(&home)),
        Err(_) => Vec::new(),
    }
}

fn metadata_roots_in(home: &Path) -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = Vec::new();
    let mut seen: Vec<PathBuf> = Vec::new();
    for rel in [
        ".local/share/Steam",
        ".steam/steam",
        ".var/app/com.valvesoftware.Steam/.local/share/Steam",
    ] {
        let dir = home.join(rel);
        if !dir.join("steamapps").is_dir() {
            continue;
        }
        let canon = std::fs::canonicalize(&dir).unwrap_or_else(|_| dir.clone());
        if seen.contains(&canon) {
            continue;
        }
        seen.push(canon);
        roots.push(dir);
    }
    roots
}

/// All Steam library roots: the client dir itself (always has its own
/// steamapps) plus every "path" value in libraryfolders.vdf. Parsed as
/// quoted key/value pairs rather than split on whitespace, since library
/// paths routinely contain spaces (e.g. external drives).
pub fn library_folders(client_dir: &Path) -> Vec<PathBuf> {
    let mut libs = vec![client_dir.to_path_buf()];

    let vdf_path = client_dir.join("steamapps/libraryfolders.vdf");
    let Ok(contents) = std::fs::read_to_string(&vdf_path) else {
        return libs;
    };

    for path in parse_library_paths(&contents) {
        if !libs.contains(&path) {
            libs.push(path);
        }
    }

    libs
}

/// Every `"path"` value in a libraryfolders.vdf, in file order.
fn parse_library_paths(contents: &str) -> Vec<PathBuf> {
    contents
        .lines()
        .filter_map(|line| {
            let fields = quoted_fields(line);
            if fields.first() == Some(&"path") {
                fields.get(1).map(PathBuf::from)
            } else {
                None
            }
        })
        .collect()
}

/// The quoted fields on a VDF line, e.g. `"path"    "/mnt/Storage/Steam"`
/// -> ["path", "/mnt/Storage/Steam"].
fn quoted_fields(line: &str) -> Vec<&str> {
    line.split('"').skip(1).step_by(2).collect()
}

/// A top-level (depth 1) string field of an appmanifest. Depth matters
/// because nested sections such as `UserConfig` carry their own keys, and
/// the top-level value is the one wanted.
fn acf_field(contents: &str, key: &str) -> Option<String> {
    let mut depth = 0usize;
    for line in contents.lines() {
        let trimmed = line.trim();
        if trimmed == "{" {
            depth += 1;
            continue;
        }
        if trimmed == "}" {
            depth = depth.saturating_sub(1);
            continue;
        }
        if depth != 1 {
            continue;
        }
        let fields = quoted_fields(line);
        if fields.first().is_some_and(|k| k.eq_ignore_ascii_case(key)) {
            if let Some(value) = fields.get(1) {
                if !value.is_empty() {
                    return Some((*value).to_string());
                }
            }
        }
    }
    None
}

fn manifest_path(lib: &Path, appid: &str) -> PathBuf {
    lib.join("steamapps").join(format!("appmanifest_{appid}.acf"))
}

/// The game's display name from its appmanifest, used to tell several running
/// games apart. Returns None if no library has a manifest for this AppId, in
/// which case callers fall back to showing the bare number.
pub fn game_name(libraries: &[PathBuf], appid: &str) -> Option<String> {
    libraries.iter().find_map(|lib| {
        let contents = std::fs::read_to_string(manifest_path(lib, appid)).ok()?;
        acf_field(&contents, "name")
    })
}

/// The game's install directory (`steamapps/common/<installdir>`), if its
/// manifest is in one of the libraries and the folder exists.
pub fn game_install_dir(libraries: &[PathBuf], appid: &str) -> Option<PathBuf> {
    libraries.iter().find_map(|lib| {
        let contents = std::fs::read_to_string(manifest_path(lib, appid)).ok()?;
        let installdir = acf_field(&contents, "installdir")?;
        let dir = lib.join("steamapps/common").join(installdir);
        dir.is_dir().then_some(dir)
    })
}

/// Every installed game as (AppID, name), read from the appmanifests in all
/// given libraries. Unreadable or malformed manifests are skipped.
pub fn installed_games(libraries: &[PathBuf]) -> Vec<(u32, String)> {
    let mut games: Vec<(u32, String)> = Vec::new();
    for lib in libraries {
        let Ok(entries) = std::fs::read_dir(lib.join("steamapps")) else {
            continue;
        };
        for entry in entries.flatten() {
            let file_name = entry.file_name();
            let Some(id) = file_name
                .to_str()
                .and_then(|n| n.strip_prefix("appmanifest_"))
                .and_then(|n| n.strip_suffix(".acf"))
                .and_then(|n| n.parse::<u32>().ok())
            else {
                continue;
            };
            let Ok(contents) = std::fs::read_to_string(entry.path()) else {
                continue;
            };
            if let Some(name) = acf_field(&contents, "name") {
                if !games.iter().any(|(existing, _)| *existing == id) {
                    games.push((id, name));
                }
            }
        }
    }
    games
}

/// Installed games across every known Steam install (native and Flatpak).
pub fn all_installed_games() -> Vec<(u32, String)> {
    let mut libs: Vec<PathBuf> = Vec::new();
    for root in metadata_roots() {
        for lib in library_folders(&root) {
            if !libs.contains(&lib) {
                libs.push(lib);
            }
        }
    }
    installed_games(&libs)
}

/// The first library whose steamapps/compatdata contains this AppId.
pub fn compatdata_dir(libraries: &[PathBuf], appid: &str) -> Option<PathBuf> {
    libraries
        .iter()
        .map(|lib| lib.join("steamapps/compatdata").join(appid))
        .find(|p| p.is_dir())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Scratch(PathBuf);

    impl Scratch {
        fn new(tag: &str) -> Self {
            let p = std::env::temp_dir().join(format!(
                "steam-punk-steam-{tag}-{}-{:?}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos())
                    .unwrap_or_default()
            ));
            std::fs::create_dir_all(&p).expect("scratch dir");
            Self(p)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    const ACF: &str = "\"AppState\"\n{\n\t\"appid\"\t\t\"3240220\"\n\t\"Universe\"\t\t\"1\"\n\t\"name\"\t\t\"Grand Theft Auto V Enhanced\"\n\t\"StateFlags\"\t\t\"4\"\n\t\"installdir\"\t\t\"Grand Theft Auto V Enhanced\"\n\t\"UserConfig\"\n\t{\n\t\t\"name\"\t\t\"Wrong Nested Name\"\n\t\t\"language\"\t\t\"english\"\n\t}\n}\n";

    const VDF: &str = "\"libraryfolders\"\n{\n\t\"0\"\n\t{\n\t\t\"path\"\t\t\"/home/alex/.local/share/Steam\"\n\t\t\"label\"\t\t\"\"\n\t\t\"apps\"\n\t\t{\n\t\t\t\"3240220\"\t\t\"1234\"\n\t\t}\n\t}\n\t\"1\"\n\t{\n\t\t\"path\"\t\t\"/mnt/My Games/SteamLibrary\"\n\t}\n}\n";

    #[test]
    fn acf_name_is_the_top_level_field() {
        assert_eq!(acf_field(ACF, "name").as_deref(), Some("Grand Theft Auto V Enhanced"));
        assert_eq!(acf_field(ACF, "installdir").as_deref(), Some("Grand Theft Auto V Enhanced"));
        assert_eq!(acf_field(ACF, "missing"), None);
    }

    #[test]
    fn acf_nested_only_key_is_ignored() {
        let acf = "\"AppState\"\n{\n\t\"appid\"\t\"1\"\n\t\"UserConfig\"\n\t{\n\t\t\"name\"\t\"Nested\"\n\t}\n}\n";
        assert_eq!(acf_field(acf, "name"), None);
    }

    #[test]
    fn libraryfolders_paths_keep_spaces_and_order() {
        assert_eq!(
            parse_library_paths(VDF),
            vec![
                PathBuf::from("/home/alex/.local/share/Steam"),
                PathBuf::from("/mnt/My Games/SteamLibrary"),
            ]
        );
        assert!(parse_library_paths("garbage").is_empty());
    }

    #[test]
    fn name_and_install_dir_resolve_across_libraries() {
        let s = Scratch::new("libs");
        let lib_a = s.0.join("a");
        let lib_b = s.0.join("b");
        std::fs::create_dir_all(lib_a.join("steamapps")).unwrap();
        std::fs::create_dir_all(lib_b.join("steamapps/common/Grand Theft Auto V Enhanced")).unwrap();
        std::fs::write(lib_b.join("steamapps/appmanifest_3240220.acf"), ACF).unwrap();
        std::fs::write(lib_b.join("steamapps/appmanifest_notanumber.acf"), ACF).unwrap();
        std::fs::write(lib_a.join("steamapps/appmanifest_7.acf"), "not a manifest").unwrap();

        let libs = vec![lib_a, lib_b.clone()];
        assert_eq!(game_name(&libs, "3240220").as_deref(), Some("Grand Theft Auto V Enhanced"));
        assert_eq!(game_name(&libs, "999"), None);
        assert_eq!(
            game_install_dir(&libs, "3240220"),
            Some(lib_b.join("steamapps/common/Grand Theft Auto V Enhanced"))
        );
        assert_eq!(
            installed_games(&libs),
            vec![(3240220, "Grand Theft Auto V Enhanced".to_string())]
        );
    }

    #[test]
    fn metadata_roots_include_flatpak_and_dedupe_symlinks() {
        let s = Scratch::new("roots");
        let native = s.0.join(".local/share/Steam");
        std::fs::create_dir_all(native.join("steamapps")).unwrap();
        std::fs::create_dir_all(s.0.join(".steam")).unwrap();
        std::os::unix::fs::symlink(&native, s.0.join(".steam/steam")).unwrap();
        let flatpak = s.0.join(".var/app/com.valvesoftware.Steam/.local/share/Steam");
        std::fs::create_dir_all(flatpak.join("steamapps")).unwrap();

        assert_eq!(metadata_roots_in(&s.0), vec![native, flatpak]);
    }
}
