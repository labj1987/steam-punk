use crate::steam;
use anyhow::{anyhow, Context, Result};
use std::path::{Path, PathBuf};

/// Everything needed to launch a trainer against the currently running game.
pub struct LaunchTarget {
    pub appid: u32,
    pub client_dir: PathBuf,
    pub compatdata_dir: PathBuf,
    pub proton_dir: PathBuf,
}

impl LaunchTarget {
    pub fn prefix_dir(&self) -> PathBuf {
        self.compatdata_dir.join("pfx")
    }
}

/// A Proton game currently running, for disambiguating when several are open.
pub struct RunningGame {
    pub appid: u32,
    pub name: String,
}

/// Scan /proc for `SteamLaunch AppId=<N>` cmdlines. Every AppId found is
/// returned, not just the first: several processes in one game's tree carry
/// the same marker (wrapper shell, reaper, pressure-vessel), so results are
/// deduplicated, and they're sorted so the answer doesn't depend on /proc
/// iteration order the way picking "whichever turns up first" did.
pub fn find_running_appids() -> Vec<u32> {
    let self_pid = std::process::id();
    let mut found: Vec<u32> = Vec::new();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return found;
    };

    for entry in entries.flatten() {
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else {
            continue;
        };
        if pid == self_pid {
            continue;
        }
        let Ok(bytes) = std::fs::read(entry.path().join("cmdline")) else {
            continue;
        };
        let args: Vec<&str> = bytes
            .split(|&b| b == 0)
            .filter(|s| !s.is_empty())
            .filter_map(|s| std::str::from_utf8(s).ok())
            .collect();

        if let Some(appid) = appid_from_cmdline(&args) {
            if !found.contains(&appid) {
                found.push(appid);
            }
        }
    }
    found.sort_unstable();
    found
}

/// The AppId a process belongs to, if its cmdline is a Steam game launch.
/// Both markers are required: `AppId=` alone shows up in unrelated command
/// lines (this app's own tooling included), and `SteamLaunch` is what makes it
/// a game rather than a mention.
fn appid_from_cmdline(args: &[&str]) -> Option<u32> {
    if !args.iter().any(|a| *a == "SteamLaunch") {
        return None;
    }
    args.iter()
        .find_map(|a| a.strip_prefix("AppId=")?.parse::<u32>().ok())
}

/// Running games with display names resolved from their Steam appmanifests.
pub fn running_games() -> Vec<RunningGame> {
    let libraries = steam::steam_client_dir()
        .map(|dir| steam::library_folders(&dir))
        .unwrap_or_default();

    let games: Vec<RunningGame> = find_running_appids()
        .into_iter()
        .map(|appid| RunningGame {
            name: steam::game_name(&libraries, &appid.to_string())
                .unwrap_or_else(|| format!("AppId {appid}")),
            appid,
        })
        .collect();

    crate::applog::log(&format!(
        "running_games -> [{}]",
        games
            .iter()
            .map(|g| format!("{} ({})", g.name, g.appid))
            .collect::<Vec<_>>()
            .join(", ")
    ));
    games
}

/// Resolve `<proton_dir>` from the currently running wineserver whose
/// STEAM_COMPAT_DATA_PATH matches this game's compatdata dir. This is the
/// source of truth — config_info can be stale (observed: pointing at a
/// different Proton build than the one actually running).
fn find_proton_dir_from_wineserver(compatdata_dir: &Path) -> Option<PathBuf> {
    let target = normalize_lexical(compatdata_dir);
    let entries = std::fs::read_dir("/proc").ok()?;

    for entry in entries.flatten() {
        if entry.file_name().to_string_lossy().parse::<u32>().is_err() {
            continue;
        }
        let exe_link = entry.path().join("exe");
        let Ok(exe_target) = std::fs::read_link(&exe_link) else {
            continue;
        };
        let normalized_exe = normalize_lexical(&exe_target);
        if !normalized_exe.to_string_lossy().ends_with("/bin/wineserver") {
            continue;
        }

        let Ok(environ) = std::fs::read(entry.path().join("environ")) else {
            continue;
        };
        let matches_prefix = environ
            .split(|&b| b == 0)
            .filter_map(|s| std::str::from_utf8(s).ok())
            .filter_map(|kv| kv.strip_prefix("STEAM_COMPAT_DATA_PATH="))
            .any(|v| normalize_lexical(Path::new(v)) == target);

        if matches_prefix {
            if let Some(proton_dir) = proton_dir_from_wineserver_exe(&normalized_exe) {
                return Some(proton_dir);
            }
        }
    }
    None
}

/// `<proton_dir>/files/bin/wineserver` -> `<proton_dir>`, after the path has
/// already been lexically normalized (Proton's own libwine loader constructs
/// this path with `../..` components, e.g. `files/lib/wine/../../bin/wineserver`).
fn proton_dir_from_wineserver_exe(normalized_exe: &Path) -> Option<PathBuf> {
    let s = normalized_exe.to_string_lossy();
    s.strip_suffix("/files/bin/wineserver").map(PathBuf::from)
}

/// Resolve `..`/`.` components without touching the filesystem (no
/// canonicalize — components may not all exist, and we don't want symlink
/// resolution changing the answer).
fn normalize_lexical(path: &Path) -> PathBuf {
    let mut out: Vec<std::ffi::OsString> = Vec::new();
    for comp in path.components() {
        match comp {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::CurDir => {}
            other => out.push(other.as_os_str().to_os_string()),
        }
    }
    out.into_iter().collect()
}

/// Fallback when no wineserver is found yet: line 2 of config_info is a path
/// inside the Proton build (`<proton_dir>/files/...`), so the Proton dir is
/// everything before the `/files/` marker. Split on the marker rather than on
/// a substring like "proton", which misfires on library paths such as
/// `/mnt/ProtonDrive/...` or usernames containing it. Never split on
/// whitespace — Proton directory names contain spaces ("Proton - Experimental").
fn proton_dir_from_config_line(line: &str) -> Option<PathBuf> {
    let (dir, _) = line.split_once("/files/")?;
    if dir.is_empty() {
        None
    } else {
        Some(PathBuf::from(dir))
    }
}

fn find_proton_dir_from_config_info(compatdata_dir: &Path) -> Option<PathBuf> {
    let contents = std::fs::read_to_string(compatdata_dir.join("config_info")).ok()?;
    proton_dir_from_config_line(contents.lines().nth(1)?)
}

fn find_proton_dir(compatdata_dir: &Path) -> Option<PathBuf> {
    find_proton_dir_from_wineserver(compatdata_dir)
        .or_else(|| find_proton_dir_from_config_info(compatdata_dir))
}

/// Resolve everything needed to launch against a specific running game: its
/// compatdata and the Proton build whose wineserver it's actually using. The
/// AppId is passed in rather than discovered here, so that choosing between
/// several running games stays a decision the caller makes explicitly.
pub fn resolve_launch_target(appid: u32) -> Result<LaunchTarget> {
    crate::applog::log(&format!("resolve_launch_target: AppId {appid}"));

    let client_dir = steam::steam_client_dir();
    crate::applog::log(&format!("resolve_launch_target: steam_client_dir -> {client_dir:?}"));
    let client_dir =
        client_dir.ok_or_else(|| anyhow!("Could not find a Steam installation."))?;

    let libraries = steam::library_folders(&client_dir);
    let compatdata_dir = steam::compatdata_dir(&libraries, &appid.to_string());
    crate::applog::log(&format!(
        "resolve_launch_target: compatdata_dir for AppId {appid} -> {compatdata_dir:?} (searched {} libraries)",
        libraries.len()
    ));
    let compatdata_dir = compatdata_dir
        .ok_or_else(|| anyhow!("Could not find compatdata for AppId {appid}."))?;

    let proton_dir = find_proton_dir(&compatdata_dir);
    crate::applog::log(&format!("resolve_launch_target: find_proton_dir -> {proton_dir:?}"));
    let proton_dir = proton_dir.ok_or_else(|| {
        anyhow!("Could not determine which Proton build the game is currently running.")
    })?;

    Ok(LaunchTarget {
        appid,
        client_dir,
        compatdata_dir,
        proton_dir,
    })
}

/// The literal ASCII marker Wine embeds in the DOS-stub area of any DLL it
/// hasn't been overridden with a real file for (confirmed via `file`/hexdump
/// against an affected prefix — this is also how `file(1)` itself detects
/// "PE32 executable for WINE (DLL)"). A "native" DllOverrides entry only
/// makes Wine *prefer* a real file over this stub if one actually made it to
/// disk — winetricks can log a dotnet verb as done, and the override can be
/// set correctly, while this placeholder is still what's actually sitting in
/// system32, e.g. if the underlying installer failed partway through.
const WINE_BUILTIN_DLL_MARKER: &[u8] = b"Wine builtin DLL";

/// True if `path` is Wine's own builtin placeholder rather than a real DLL.
/// Only reads the first 1KB — the marker sits right after the DOS header on
/// every observed builtin stub, and these files can otherwise be large.
fn is_wine_builtin_dll(path: &Path) -> bool {
    let Ok(file) = std::fs::File::open(path) else {
        return false;
    };
    use std::io::Read;
    let mut buf = [0u8; 1024];
    let Ok(n) = file.take(1024).read(&mut buf) else {
        return false;
    };
    buf[..n]
        .windows(WINE_BUILTIN_DLL_MARKER.len())
        .any(|w| w == WINE_BUILTIN_DLL_MARKER)
}

/// `Release` value of .NET Framework 4.6.2 — the minimum current FLiNG
/// trainers accept ("This trainer requires .NET Framework 4.6.2 or higher").
/// Older trainers run happily on 4.0, which is why a prefix can look fine for
/// one game and fail for another.
const DOTNET_462_RELEASE: u32 = 394802;

/// CRT libraries `clr.dll` links against, installed into system32/syswow64 by
/// the .NET installer itself (not under Microsoft.NET/). Copying the runtime
/// tree without these leaves a prefix that looks complete but where the CLR
/// never loads — Wine logs `err:module:import_dll` for each and the managed
/// process dies before `main`.
const CLR_CRT_SUFFIX: &str = "_clr0400.dll";

/// Everything that has to be true for a modern .NET trainer to actually run.
pub struct DotnetStatus {
    pub clr_present: bool,
    pub mscoree_native: bool,
    pub crt_present: bool,
    pub release: Option<u32>,
}

impl DotnetStatus {
    pub fn is_usable(&self) -> bool {
        self.clr_present
            && self.mscoree_native
            && self.crt_present
            && self.release.is_some_and(|r| r >= DOTNET_462_RELEASE)
    }
}

/// Case-insensitive lookup — the .NET installer and our own copies disagree on
/// casing (`VCRUNTIME140_CLR0400.dll` in clr.dll's import table vs
/// `vcruntime140_clr0400.dll` on disk), and Linux filesystems care.
fn find_file_ci(dir: &Path, name: &str) -> Option<PathBuf> {
    let entries = std::fs::read_dir(dir).ok()?;
    entries.flatten().map(|e| e.path()).find(|p| {
        p.file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.eq_ignore_ascii_case(name))
    })
}

fn dir_has_suffix_ci(dir: &Path, suffix: &str) -> bool {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return false;
    };
    entries.flatten().any(|e| {
        e.file_name()
            .to_str()
            .is_some_and(|n| n.to_ascii_lowercase().ends_with(suffix))
    })
}

/// Parse `Release` out of `NDP\v4\Full` in a prefix's system.reg. This is the
/// canonical way to tell 4.8 from 4.0 — both live in the same
/// `v4.0.30319` directory, so the path alone says nothing about the version.
fn dotnet_release(prefix: &Path) -> Option<u32> {
    parse_dotnet_release(&std::fs::read_to_string(prefix.join("system.reg")).ok()?)
}

fn parse_dotnet_release(text: &str) -> Option<u32> {
    let mut in_full = false;
    for line in text.lines() {
        if line.starts_with('[') {
            in_full = line.starts_with(r"[Software\\Microsoft\\NET Framework Setup\\NDP\\v4\\Full]");
            continue;
        }
        if in_full {
            if let Some(hex) = line.strip_prefix(r#""Release"=dword:"#) {
                return u32::from_str_radix(hex.trim(), 16).ok();
            }
        }
    }
    None
}

pub fn dotnet_status(prefix: &Path) -> DotnetStatus {
    let system32 = prefix.join("drive_c/windows/system32");
    let mscoree = system32.join("mscoree.dll");
    DotnetStatus {
        clr_present: prefix
            .join("drive_c/windows/Microsoft.NET/Framework64/v4.0.30319/clr.dll")
            .is_file(),
        mscoree_native: mscoree.is_file() && !is_wine_builtin_dll(&mscoree),
        crt_present: dir_has_suffix_ci(&system32, CLR_CRT_SUFFIX),
        release: dotnet_release(prefix),
    }
}

/// True when the prefix can actually run a current .NET trainer. Each
/// component is logged because they fail independently — a prefix can have a
/// real mscoree.dll and a 4.8 registry while still missing the CRT files.
pub fn has_usable_dotnet(target: &LaunchTarget) -> bool {
    let s = dotnet_status(&target.prefix_dir());
    let usable = s.is_usable();
    crate::applog::log(&format!(
        "has_usable_dotnet -> {usable} (clr.dll: {}, native mscoree.dll: {}, \
         clr CRT libs: {}, Release: {})",
        s.clr_present,
        s.mscoree_native,
        s.crt_present,
        s.release
            .map_or_else(|| "absent".to_string(), |r| format!("{r} (need >= {DOTNET_462_RELEASE})")),
    ));
    usable
}

/// Logs each `dosdevices/` drive-letter mapping in the prefix and whether its
/// target actually resolves — a stale or broken one here is a known source
/// of Wine returning ERROR_BAD_NETPATH ("network path not found") for
/// otherwise-valid paths, and this is the only place our own code can look
/// before handing off to Proton.
fn log_dosdevices(target: &LaunchTarget) {
    let dosdevices = target.prefix_dir().join("dosdevices");
    let Ok(entries) = std::fs::read_dir(&dosdevices) else {
        crate::applog::log(&format!(
            "launch_trainer: could not read {}",
            dosdevices.display()
        ));
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        // Only drive letters (e.g. "c:", "z:") matter for path resolution —
        // com1..com32/lpt1..lpt3 are Wine's default virtual port symlinks,
        // routinely dangling on any machine without legacy serial hardware,
        // and would just bury the real signal in expected-looking noise.
        let is_drive_letter = matches!(entry.file_name().to_str(), Some(n) if n.len() == 2 && n.ends_with(':'));
        if !is_drive_letter {
            continue;
        }
        // read_link (not the entry's own target) since dosdevices symlinks
        // are commonly relative to the dosdevices dir itself — canonicalize
        // resolves that correctly, and its success/failure is the existence
        // check (safer than testing the raw link string, which would
        // resolve relative targets against our own CWD instead).
        let Ok(link_target) = std::fs::read_link(&path) else {
            continue;
        };
        let resolves = std::fs::canonicalize(&path).is_ok();
        crate::applog::log(&format!(
            "launch_trainer: dosdevice {} -> {} (resolves: {resolves})",
            path.display(),
            link_target.display()
        ));
    }
}

/// Launch a trainer against the resolved target, detached: the FLiNG exe
/// unpacks itself to a TrainerCacheData folder and relaunches, so this
/// initial process exiting quickly is expected, not a failure.
///
/// Returns the process group ID to track for the trainer's lifetime (see
/// `process_group_alive`/`stop_trainer`). `process_group(0)` below makes
/// this the same number as the spawned pid, but the group — not the single
/// pid — is what stays valid across a self-relaunch: confirmed against a
/// real FLiNG trainer (GTA San Andreas Definitive Edition) that the
/// unpacked `Z:\...\<trainer>.exe` process it relaunches into keeps the
/// same pgid as this original process, with no explicit setpgid of its
/// own, and that `kill -TERM -<pgid>` reliably takes down the whole tree
/// (wrapper + relaunched trainer) in one shot.
pub fn launch_trainer(target: &LaunchTarget, trainer_path: &Path, log_path: &Path) -> Result<u32> {
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};

    if let Some(parent) = log_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let log_out = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)
        .with_context(|| format!("opening log file {}", log_path.display()))?;
    let log_err = log_out.try_clone()?;

    log_dosdevices(target);

    let proton = target.proton_dir.join("proton");
    crate::applog::log(&format!(
        "launch_trainer: {} runinprefix {} (STEAM_COMPAT_CLIENT_INSTALL_PATH={} STEAM_COMPAT_DATA_PATH={})",
        proton.display(),
        trainer_path.display(),
        target.client_dir.display(),
        target.compatdata_dir.display(),
    ));
    let spawn_result = Command::new(&proton)
        .arg("runinprefix")
        .arg(trainer_path)
        .env("STEAM_COMPAT_CLIENT_INSTALL_PATH", &target.client_dir)
        .env("STEAM_COMPAT_DATA_PATH", &target.compatdata_dir)
        .stdin(Stdio::null())
        .stdout(log_out)
        .stderr(log_err)
        .process_group(0)
        .spawn();

    let child = match spawn_result {
        Ok(child) => child,
        Err(e) => {
            crate::applog::log(&format!("launch_trainer: spawn failed: {e}"));
            return Err(e).with_context(|| format!("spawning {}", proton.display()));
        }
    };
    let pid = child.id();
    crate::applog::log(&format!("launch_trainer: spawned pid {pid}"));

    // Report back to the caller immediately (the toast shouldn't wait on
    // this) but keep watching in the background: FLiNG trainers unpack
    // themselves and relaunch, so this initial process exiting quickly is
    // normal — logged as information, not an error — but the exit status
    // and anything it wrote to log_path (captured above) are the only
    // window we get into a Proton/wine-level failure our own code can't see.
    //
    // This thread also owns reaping: nothing else in the app ever calls
    // wait()/try_wait() on this Child, so if it stopped polling once the
    // quick-exit window passed, a process that later exits (on its own, or
    // via stop_trainer's SIGKILL — confirmed live) would sit as a zombie
    // for the rest of the app's session, since only this Child handle can
    // reap it. Keeps polling at a slower cadence indefinitely instead.
    std::thread::spawn(move || {
        let mut child = child;
        for i in 0.. {
            std::thread::sleep(std::time::Duration::from_millis(if i < 10 { 200 } else { 1000 }));
            match child.try_wait() {
                Ok(Some(status)) => {
                    if i < 10 {
                        crate::applog::log(&format!(
                            "launch_trainer: pid {pid} exited with {status} \
                             (a quick exit here is expected — FLiNG trainers unpack \
                             and relaunch themselves; check the output above/below \
                             this line and the exit code for signs of an actual error)"
                        ));
                    } else {
                        crate::applog::log(&format!("launch_trainer: pid {pid} exited with {status}"));
                    }
                    return;
                }
                Ok(None) => {
                    if i == 9 {
                        crate::applog::log(&format!("launch_trainer: pid {pid} still running after 2s"));
                    }
                    continue;
                }
                Err(e) => {
                    crate::applog::log(&format!("launch_trainer: try_wait error for pid {pid}: {e}"));
                    return;
                }
            }
        }
    });

    Ok(pid)
}

/// True if any *non-zombie* process currently belongs to process group
/// `pgid`. Used instead of a single `Child::try_wait()` because a trainer
/// that unpacks and relaunches itself (see `launch_trainer`) can leave the
/// originally tracked process exited while the relaunched one — never
/// explicitly moved to its own process group — keeps running under the
/// same `pgid`.
///
/// Zombies are deliberately excluded: a killed process stays visible in
/// /proc in state `Z` until its parent reaps it (`launch_trainer`'s
/// watcher thread does this, but not instantly), and a zombie is doing
/// nothing — for "is the trainer still running" it should read the same as
/// not present. Confirmed live: without this, a stopped trainer kept
/// showing as running.
pub fn process_group_alive(pgid: u32) -> bool {
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return false;
    };
    for entry in entries.flatten() {
        if entry.file_name().to_string_lossy().parse::<u32>().is_err() {
            continue;
        }
        let Ok(stat) = std::fs::read_to_string(entry.path().join("stat")) else {
            continue;
        };
        // Fields after the "(comm)" can't be split on plain whitespace up
        // front — comm itself may contain spaces or parens — so split off
        // everything after the *last* ')' first. What follows is, in
        // order: state, ppid, pgrp, ... (pgrp is the 3rd field here).
        let Some((_, after_comm)) = stat.rsplit_once(')') else {
            continue;
        };
        let mut fields = after_comm.split_whitespace();
        let state = fields.next();
        if state == Some("Z") {
            continue;
        }
        let pgrp = fields.nth(1).and_then(|s| s.parse::<u32>().ok());
        if pgrp == Some(pgid) {
            return true;
        }
    }
    false
}

/// Stop a trainer launched via `launch_trainer`: SIGTERM the whole process
/// group first (not just the tracked pid — a lone `kill <pid>` would miss
/// the relaunched trainer process; see `launch_trainer`'s doc comment),
/// then SIGKILL after a ~2s grace period if it hasn't exited. Blocks for up
/// to that grace period — call via `spawn_blocking`, not on the GTK thread.
pub fn stop_trainer(pgid: u32) {
    crate::applog::log(&format!("stop_trainer: SIGTERM -{pgid}"));
    let s = std::process::Command::new("kill").arg("-TERM").arg(format!("-{pgid}")).status();
    crate::applog::log(&format!("stop_trainer: kill -TERM -{pgid} -> {s:?}"));

    // Same grace window as the post-launch quick-exit check above.
    for _ in 0..10 {
        std::thread::sleep(std::time::Duration::from_millis(200));
        if !process_group_alive(pgid) {
            crate::applog::log(&format!("stop_trainer: pgid {pgid} exited after SIGTERM"));
            return;
        }
    }

    crate::applog::log(&format!("stop_trainer: pgid {pgid} still alive after 2s, sending SIGKILL"));
    let s = std::process::Command::new("kill").arg("-KILL").arg(format!("-{pgid}")).status();
    crate::applog::log(&format!("stop_trainer: kill -KILL -{pgid} -> {s:?}"));

    // Deliberately not waiting to confirm SIGKILL took effect: observed live
    // against a real wine/proton tree that full cleanup can take anywhere
    // from under a second to several seconds under load, with no reliable
    // upper bound worth blocking the caller on here. The 1s periodic poll
    // in ui.rs (see process_group_alive) is what actually converges the
    // running badge to "gone" once /proc reflects it, however long that
    // takes — this function's job is just to have sent the signals.
}

/// The system32/syswow64 files a copied runtime tree depends on: the CLR's own
/// CRT builds plus the mscoree.dll shim. The .NET installer puts these outside
/// Microsoft.NET/, so cloning only that tree leaves a prefix that looks
/// complete but can't load the CLR.
fn dotnet_system_files(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .map(|n| n.to_ascii_lowercase())
                .is_some_and(|n| n.ends_with(CLR_CRT_SUFFIX) || n == "mscoree.dll")
        })
        .collect()
}

/// Put `src`'s bytes at `dst`, replacing whatever is already there.
///
/// `std::fs::copy` opens the destination for writing, which fails in two ways that
/// both occur throughout a Proton prefix, and either one used to abort the whole .NET
/// repair through `?`:
///
/// * **Symlinks into the Proton installation.** A prefix ships its builtin DLLs as
///   links into Proton's own (read-only) tree, and `copy` follows them — so writing
///   the donor's native `mscoree.dll` tried to write it *into Proton itself* and got
///   permission denied. Measured 2026-09-17: `system32/mscoree.dll` was a 109-byte
///   symlink to `.../Proton-GE Latest/files/lib/wine/x86_64-windows/mscoree.dll`.
/// * **Read-only regular files.** A previous clone copies the donor's permission bits
///   along with its bytes, so the framework tree it leaves behind contains read-only
///   files (28 of them, measured on the same prefix). Every later repair then died
///   partway through re-copying that tree — which is why the first repair on a fresh
///   prefix appeared to work and every one after it failed.
///
/// Unlinking first fixes both, and for the builtin case it is also what makes the
/// result *correct* rather than merely writable: overriding a builtin means a real
/// file inside the prefix, not a redirect back to the one being overridden.
fn replace_file(src: &Path, dst: &Path) -> Result<()> {
    match std::fs::remove_file(dst) {
        Ok(()) => {}
        // Nothing there yet is the normal case for a prefix that never had this DLL.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            return Err(e)
                .with_context(|| format!("clearing {} before copying over it", dst.display()))
        }
    }
    std::fs::copy(src, dst)
        .with_context(|| format!("copying {} -> {}", src.display(), dst.display()))?;
    Ok(())
}

fn copy_dir_all(from: &Path, to: &Path) -> Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)?.flatten() {
        let src = entry.path();
        let dst = to.join(entry.file_name());
        if src.is_dir() {
            copy_dir_all(&src, &dst)?;
        } else {
            // Replaces rather than writes through: a re-clone hits the read-only files
            // the previous clone left behind, and `fs::copy` cannot overwrite those.
            replace_file(&src, &dst)?;
        }
    }
    Ok(())
}

/// Convert the .NET sections of a prefix's system.reg into a .reg file for
/// regedit. Wine writes system.reg with doubled backslashes in key paths and a
/// timestamp after each key, neither of which regedit accepts.
///
/// Importing through regedit rather than editing system.reg directly is
/// deliberate: the game's wineserver keeps the registry in memory and would
/// overwrite a direct edit on its next flush.
fn dotnet_reg_export(system_reg: &str) -> String {
    const WANTED: [&str; 4] = [
        r"Software\\Microsoft\\NET Framework Setup",
        r"Software\\Wow6432Node\\Microsoft\\NET Framework Setup",
        r"Software\\Microsoft\\.NETFramework",
        r"Software\\Wow6432Node\\Microsoft\\.NETFramework",
    ];

    let mut out = String::from("Windows Registry Editor Version 5.00\n");
    let mut keep = false;
    let mut continuing = false;

    for line in system_reg.lines() {
        if line.starts_with('[') {
            continuing = false;
            let key = line[1..].split(']').next().unwrap_or_default();
            keep = WANTED
                .iter()
                .any(|w| key == *w || key.starts_with(&format!("{w}\\\\")));
            if keep {
                out.push_str("\n[HKEY_LOCAL_MACHINE\\");
                out.push_str(&key.replace("\\\\", "\\"));
                out.push_str("]\n");
            }
            continue;
        }
        if !keep {
            continue;
        }
        // Values can wrap across lines with a trailing backslash (long hex
        // blobs do this routinely), so a continuation is copied verbatim
        // rather than re-tested for a leading quote.
        if continuing {
            out.push_str(line);
            out.push('\n');
            continuing = line.ends_with('\\');
        } else if line.starts_with('"') || line.starts_with('@') {
            out.push_str(line);
            out.push('\n');
            continuing = line.ends_with('\\');
        }
    }
    out
}

fn apply_dotnet_registry(target: &LaunchTarget, donor_prefix: &Path) -> Result<()> {
    let donor_reg_path = donor_prefix.join("system.reg");
    let donor_reg = std::fs::read_to_string(&donor_reg_path)
        .with_context(|| format!("reading {}", donor_reg_path.display()))?;

    let temp_dir = target.prefix_dir().join("drive_c/windows/temp");
    std::fs::create_dir_all(&temp_dir)?;
    let reg_path = temp_dir.join("steam-punk-dotnet.reg");
    std::fs::write(&reg_path, dotnet_reg_export(&donor_reg))
        .with_context(|| format!("writing {}", reg_path.display()))?;

    let proton = target.proton_dir.join("proton");
    let status = std::process::Command::new(&proton)
        .arg("runinprefix")
        .arg("regedit")
        .arg("/S")
        .arg(r"C:\windows\temp\steam-punk-dotnet.reg")
        .env("STEAM_COMPAT_CLIENT_INSTALL_PATH", &target.client_dir)
        .env("STEAM_COMPAT_DATA_PATH", &target.compatdata_dir)
        .status();
    // The temp .reg is only needed for the import; drop it either way.
    let _ = std::fs::remove_file(&reg_path);
    let status = status.with_context(|| format!("running regedit via {}", proton.display()))?;

    crate::applog::log(&format!("apply_dotnet_registry: regedit exited {status:?}"));
    if !status.success() {
        return Err(anyhow!("regedit failed to import .NET registry keys ({status})"));
    }
    Ok(())
}

/// Rebuild a prefix's .NET runtime by cloning it from another Proton prefix on
/// this system that already has a working one.
///
/// This is the preferred repair rather than a last resort: on Wine's new wow64
/// mode the Microsoft installers winetricks drives fail outright (status 67,
/// `Failed to extract cabinet: netfx_core.mzz`) and their rollback strips .NET
/// back out, leaving the prefix worse off than before it was attempted.
///
/// All four pieces have to move together — the runtime tree, the GAC, the CRT
/// libraries clr.dll imports from system32, and the registry that reports the
/// version — since a prefix missing any single one of them still fails, just
/// with a less obvious symptom.
///
/// Returns Ok(false) if no prefix on this system had a usable .NET to clone.
pub fn repair_dotnet_from_sibling_prefix(target: &LaunchTarget) -> Result<bool> {
    let Some(compatdata_root) = target.compatdata_dir.parent() else {
        return Ok(false);
    };
    let Ok(entries) = std::fs::read_dir(compatdata_root) else {
        return Ok(false);
    };

    let ours = target.prefix_dir();
    let donor = entries
        .flatten()
        .map(|e| e.path().join("pfx"))
        .filter(|p| *p != ours)
        .find(|p| dotnet_status(p).is_usable());

    let Some(donor) = donor else {
        crate::applog::log(
            "repair_dotnet_from_sibling_prefix: no prefix on this system has a usable .NET to clone",
        );
        return Ok(false);
    };
    crate::applog::log(&format!(
        "repair_dotnet_from_sibling_prefix: cloning .NET from {}",
        donor.display()
    ));

    for tree in ["drive_c/windows/Microsoft.NET", "drive_c/windows/assembly"] {
        let from = donor.join(tree);
        if from.is_dir() {
            copy_dir_all(&from, &ours.join(tree)).with_context(|| format!("cloning {tree}"))?;
            crate::applog::log(&format!("repair_dotnet_from_sibling_prefix: cloned {tree}"));
        }
    }

    for dir in ["drive_c/windows/system32", "drive_c/windows/syswow64"] {
        let to = ours.join(dir);
        if !to.is_dir() {
            continue;
        }
        let mut copied = 0usize;
        for src in dotnet_system_files(&donor.join(dir)) {
            let Some(name) = src.file_name() else { continue };
            // Overwrite whatever is there under its existing casing — what's
            // present is typically Wine's builtin stub or a 4.0-era copy, and
            // both are exactly the problem being repaired.
            let dst = find_file_ci(&to, &name.to_string_lossy()).unwrap_or_else(|| to.join(name));
            if let Err(e) = replace_file(&src, &dst) {
                // Logged as well as returned: this failure used to surface only as a
                // dialog, so the app log showed the framework trees being cloned and
                // then simply stopped, with nothing to say the repair had aborted.
                crate::applog::log(&format!(
                    "repair_dotnet_from_sibling_prefix: FAILED to install {} -> {}: {e:#}",
                    src.display(),
                    dst.display()
                ));
                return Err(e);
            }
            copied += 1;
        }
        crate::applog::log(&format!(
            "repair_dotnet_from_sibling_prefix: copied {copied} CLR support libraries into {dir}"
        ));
    }

    apply_dotnet_registry(target, &donor)?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_info_splits_on_files_marker_not_proton_substring() {
        assert_eq!(
            proton_dir_from_config_line(
                "/mnt/ProtonDrive/steamapps/common/Proton - Experimental/files/share/fonts/"
            ),
            Some(PathBuf::from("/mnt/ProtonDrive/steamapps/common/Proton - Experimental"))
        );
        assert_eq!(proton_dir_from_config_line("/no/marker/here"), None);
    }

    /// A scratch directory under the test binary's own temp space, removed on drop.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(tag: &str) -> Self {
            let p = std::env::temp_dir().join(format!(
                "steam-punk-test-{tag}-{}-{:?}",
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

    #[test]
    fn installing_over_a_builtin_symlink_replaces_the_link_not_its_target() {
        // The real failure this reproduces: a Proton prefix ships builtin DLLs as
        // symlinks into the Proton installation, which is read-only. Copying onto the
        // link followed it and tried to write into Proton's tree, which failed and
        // aborted the whole .NET repair -- leaving mscoree.dll as Wine's builtin
        // forever and every .NET trainer unable to load the CLR.
        let s = Scratch::new("symlink");
        let proton = s.0.join("proton");
        let prefix = s.0.join("prefix");
        std::fs::create_dir_all(&proton).unwrap();
        std::fs::create_dir_all(&prefix).unwrap();

        // Stand in for Proton's own builtin, and make it read-only the way a real
        // installation's files are.
        let builtin = proton.join("mscoree.dll");
        std::fs::write(&builtin, b"wine builtin stub").unwrap();
        let mut perms = std::fs::metadata(&builtin).unwrap().permissions();
        perms.set_readonly(true);
        std::fs::set_permissions(&builtin, perms).unwrap();

        let dst = prefix.join("mscoree.dll");
        std::os::unix::fs::symlink(&builtin, &dst).unwrap();

        let src = s.0.join("native-mscoree.dll");
        std::fs::write(&src, b"native microsoft mscoree").unwrap();

        replace_file(&src, &dst).expect("replacing a builtin symlink must succeed");

        // The prefix now holds a real file with the native bytes...
        assert!(!dst.is_symlink(), "destination is still a symlink");
        assert_eq!(std::fs::read(&dst).unwrap(), b"native microsoft mscoree");
        // ...and Proton's own copy was left completely alone, which is the part that
        // used to fail with permission denied.
        assert_eq!(std::fs::read(&builtin).unwrap(), b"wine builtin stub");
    }

    #[test]
    fn replacing_a_read_only_file_succeeds() {
        // The failure that actually blocked the repair in practice: a previous clone
        // copies the donor's permission bits along with its bytes, so the framework
        // tree it leaves behind contains read-only files. `fs::copy` cannot overwrite
        // those, so every repair after the first died partway through re-cloning.
        let s = Scratch::new("readonly");
        let dst = s.0.join("System.dll");
        std::fs::write(&dst, b"old").unwrap();
        let mut perms = std::fs::metadata(&dst).unwrap().permissions();
        perms.set_readonly(true);
        std::fs::set_permissions(&dst, perms).unwrap();
        assert!(std::fs::File::create(&dst).is_err(), "test setup: dst should be unwritable");

        let src = s.0.join("new-System.dll");
        std::fs::write(&src, b"fresh").unwrap();

        replace_file(&src, &dst).expect("replacing a read-only file must succeed");
        assert_eq!(std::fs::read(&dst).unwrap(), b"fresh");
    }

    #[test]
    fn installing_over_a_plain_file_still_overwrites_it() {
        let s = Scratch::new("plain");
        let dst = s.0.join("mscoree.dll");
        std::fs::write(&dst, b"old 4.0-era copy").unwrap();
        let src = s.0.join("new.dll");
        std::fs::write(&src, b"native").unwrap();

        replace_file(&src, &dst).expect("overwriting a plain file must succeed");
        assert_eq!(std::fs::read(&dst).unwrap(), b"native");
    }

    #[test]
    fn installing_where_nothing_exists_yet_is_not_an_error() {
        // A prefix that never had the DLL at all is the normal case, not a failure.
        let s = Scratch::new("absent");
        let src = s.0.join("new.dll");
        std::fs::write(&src, b"native").unwrap();
        let dst = s.0.join("subdir-absent.dll");

        replace_file(&src, &dst).expect("a missing destination must not error");
        assert_eq!(std::fs::read(&dst).unwrap(), b"native");
    }

    const SAMPLE: &str = concat!(
        "WINE REGISTRY Version 2\n",
        "#arch=win64\n",
        "\n",
        r"[Software\\Microsoft\\NET Framework Setup\\NDP\\v4\\Full] 1785895174",
        "\n",
        "#time=1dd247e0fdefec8\n",
        "\"Install\"=dword:00000001\n",
        "\"Release\"=dword:00080eb1\n",
        "\"Version\"=\"4.8.03761\"\n",
        "\n",
        r"[Software\\Valve\\Steam] 123",
        "\n",
        "\"Unrelated\"=\"leave me alone\"\n",
    );

    #[test]
    fn reg_export_selects_dotnet_keys_and_unescapes_them() {
        let out = dotnet_reg_export(SAMPLE);
        assert!(out.starts_with("Windows Registry Editor Version 5.00\n"));
        assert!(out.contains(
            r"[HKEY_LOCAL_MACHINE\Software\Microsoft\NET Framework Setup\NDP\v4\Full]"
        ));
        assert!(out.contains("\"Release\"=dword:00080eb1"));
    }

    #[test]
    fn reg_export_drops_unrelated_keys_and_wine_metadata() {
        let out = dotnet_reg_export(SAMPLE);
        assert!(!out.contains("Unrelated"));
        assert!(!out.contains("Valve"));
        assert!(!out.contains("#time="));
        assert!(!out.contains("1785895174"));
    }

    #[test]
    fn release_is_read_from_the_v4_full_key() {
        assert_eq!(parse_dotnet_release(SAMPLE), Some(0x00080eb1));
        assert!(parse_dotnet_release(SAMPLE).is_some_and(|r| r >= DOTNET_462_RELEASE));
    }

    #[test]
    fn appid_is_read_from_a_steam_game_cmdline() {
        let args = ["reaper", "SteamLaunch", "AppId=1547000", "--", "proton"];
        assert_eq!(appid_from_cmdline(&args), Some(1547000));
    }

    #[test]
    fn appid_requires_the_steamlaunch_marker() {
        // A bare "AppId=" turns up in command lines that aren't a running game
        // — matching those would target a prefix for a game that isn't open.
        let args = ["grep", "AppId=1547000"];
        assert_eq!(appid_from_cmdline(&args), None);
    }

    #[test]
    fn non_numeric_appid_is_ignored() {
        let args = ["reaper", "SteamLaunch", "AppId=notanumber"];
        assert_eq!(appid_from_cmdline(&args), None);
    }

    #[test]
    fn release_is_absent_when_only_dotnet40_is_installed() {
        let dotnet40 = SAMPLE.replace("\"Release\"=dword:00080eb1\n", "");
        assert_eq!(parse_dotnet_release(&dotnet40), None);
    }

    /// Live end-to-end check against a real running game + trainer (GTA San
    /// Andreas Definitive Edition, AppId 1547000 — must already be running
    /// with the trainer imported, same as the manual `Testing` steps in the
    /// running-indicator feature handoff). Not run in CI.
    #[test]
    #[ignore]
    fn live_launch_track_and_stop_a_real_trainer() {
        let target = resolve_launch_target(1547000).expect("resolve_launch_target(GTA SA)");
        let trainer_path = PathBuf::from(std::env::var("HOME").unwrap())
            .join(".local/share/steam-punk/trainers")
            .join("Grand Theft Auto San Andreas The Definitive Edition v1.0-v1.0.8.11827 Plus 49 Trainer.exe");
        assert!(trainer_path.is_file(), "test trainer not found at {trainer_path:?}");
        let log_path = PathBuf::from("/tmp/steam-punk-live-test.log");

        let pgid = launch_trainer(&target, &trainer_path, &log_path).expect("launch_trainer");
        println!("launched, pgid={pgid}");

        std::thread::sleep(std::time::Duration::from_secs(3));
        assert!(
            process_group_alive(pgid),
            "expected the trainer's process group to still be alive 3s after launch"
        );
        println!("confirmed alive at pgid {pgid}");

        stop_trainer(pgid);

        // stop_trainer itself doesn't block until fully confirmed gone (see
        // its doc comment) — mirror the UI's own periodic poll here instead
        // of asserting immediately.
        let mut gone = false;
        for _ in 0..50 {
            if !process_group_alive(pgid) {
                gone = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(200));
        }
        assert!(gone, "pgid {pgid} still alive 10s after stop_trainer");
        println!("confirmed pgid {pgid} fully stopped");
    }
}

/// Stop every tracked trainer by signalling exactly the process groups we
/// launched, instead of `pkill -f` pattern matching against every process's
/// command line (which can hit unrelated processes). Blocks up to ~2s per
/// group — call via `spawn_blocking`.
pub fn stop_all(pgids: &[u32]) {
    for &pgid in pgids.iter().filter(|&&p| p > 1) {
        stop_trainer(pgid);
    }
    crate::applog::log(&format!("stop_all: stopped pgids {pgids:?}"));
}

/// Per-game trainer-logs directories FLiNG trainers leave behind. Each may
/// contain a stale info.ini lock file that makes a trainer refuse to start
/// because it thinks a previous instance is still running; deleting it is
/// the documented fix. There's no reliable way to map the running game to
/// exactly one of these (the folder name is the trainer's internal title,
/// not necessarily Steam's), so recovery lists all of them and lets the
/// user pick.
pub fn trainer_log_dirs(target: &LaunchTarget) -> Vec<PathBuf> {
    let base = target
        .prefix_dir()
        .join("drive_c/users/steamuser/AppData/Local/FLiNGTrainer/trainer-logs");
    let Ok(entries) = std::fs::read_dir(&base) else {
        return Vec::new();
    };
    entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect()
}
