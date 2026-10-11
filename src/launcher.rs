use crate::{gamedata, steam};
use anyhow::{anyhow, Context, Result};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};

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
    if !args.contains(&"SteamLaunch") {
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
        self.clr_present && self.mscoree_native && self.crt_present && self.release_ok()
    }

    fn release_ok(&self) -> bool {
        self.release.is_some_and(|r| r >= DOTNET_462_RELEASE)
    }

    /// One-line prefix health status, e.g. for the setup dialog and the
    /// failure diagnosis: either ready, or exactly which pieces are missing.
    pub fn summary(&self) -> String {
        if self.is_usable() {
            return ".NET runtime ready".to_string();
        }
        let mut missing: Vec<&str> = Vec::new();
        if !self.clr_present {
            missing.push("clr.dll");
        }
        if !self.mscoree_native {
            missing.push("native mscoree.dll");
        }
        if !self.crt_present {
            missing.push("CLR support libraries");
        }
        if !self.release_ok() {
            missing.push("a .NET 4.6.2+ registry entry");
        }
        format!(".NET runtime not usable (missing {})", missing.join(", "))
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

/// Whether the prefix can actually run a current .NET trainer, and the
/// one-line status shown to the user before a launch that can't proceed.
/// Each component is logged because they fail independently — a prefix can
/// have a real mscoree.dll and a 4.8 registry while still missing the CRT
/// files.
///
/// The files are checked on disk. The registry is too, except while the
/// game's wineserver is running and the on-disk `Release` falls short: the
/// server holds the registry in memory and only writes `system.reg` out
/// periodically and on exit, so keys imported moments ago (the repair's
/// regedit step) aren't on disk yet. Then the running server itself is asked
/// (`reg query` through the game's Proton). A good on-disk value is trusted
/// as is, which keeps the usual pre-launch check free of a wine round trip.
pub fn prefix_health(target: &LaunchTarget) -> (bool, String) {
    let prefix = target.prefix_dir();
    let mut s = dotnet_status(&prefix);
    if !s.release_ok() && prefix_in_use(&prefix) {
        match live_dotnet_release(target) {
            Ok(release) => {
                crate::applog::log(&format!(
                    "prefix_health: on-disk Release {:?}, running wineserver reports {release:?}",
                    s.release
                ));
                s.release = release;
            }
            Err(e) => crate::applog::log(&format!(
                "prefix_health: could not query the running wineserver's registry: {e:#}"
            )),
        }
    }
    let usable = s.is_usable();
    crate::applog::log(&format!(
        "prefix_health -> usable {usable} (clr.dll: {}, native mscoree.dll: {}, \
         clr CRT libs: {}, Release: {})",
        s.clr_present,
        s.mscoree_native,
        s.crt_present,
        s.release
            .map_or_else(|| "absent".to_string(), |r| format!("{r} (need >= {DOTNET_462_RELEASE})")),
    ));
    (usable, s.summary())
}

/// The registry key holding the installed .NET 4.x `Release`.
const NDP_V4_FULL_KEY: &str = r"HKLM\Software\Microsoft\NET Framework Setup\NDP\v4\Full";

/// `Release` as the running wineserver has it, read with `reg query` through
/// the game's own Proton (the same `proton runinprefix` the trainer uses, so
/// it joins the game's wineserver with the game's settings). `Ok(None)` when
/// the key or value doesn't exist.
fn live_dotnet_release(target: &LaunchTarget) -> Result<Option<u32>> {
    let proton = target.proton_dir.join("proton");
    let out = std::process::Command::new(&proton)
        .args(["runinprefix", "reg", "query", NDP_V4_FULL_KEY, "/v", "Release"])
        .env("STEAM_COMPAT_CLIENT_INSTALL_PATH", &target.client_dir)
        .env("STEAM_COMPAT_DATA_PATH", &target.compatdata_dir)
        // With PROTON_LOG set, Proton sends wine's stdout to its log file.
        .env_remove("PROTON_LOG")
        .stdin(std::process::Stdio::null())
        .output()
        .with_context(|| format!("running reg query via {}", proton.display()))?;
    let stdout = String::from_utf8_lossy(&out.stdout);
    match parse_reg_query_dword(&stdout, "Release") {
        Some(v) => Ok(Some(v)),
        // reg.exe exits 1 when the key or value is missing.
        None if out.status.code() == Some(1) => Ok(None),
        None => Err(anyhow!(
            "reg query exited {} without a Release value: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        )),
    }
}

/// A REG_DWORD value from `reg query` output, e.g.
/// `    Release    REG_DWORD    0x80ff4`.
fn parse_reg_query_dword(output: &str, name: &str) -> Option<u32> {
    output.lines().find_map(|line| {
        let mut fields = line.split_whitespace();
        if !fields.next()?.eq_ignore_ascii_case(name) || fields.next()? != "REG_DWORD" {
            return None;
        }
        let value = fields.next()?;
        let hex = value.strip_prefix("0x").or_else(|| value.strip_prefix("0X"))?;
        u32::from_str_radix(hex, 16).ok()
    })
}

/// Wine's per-prefix server directory, `/tmp/.wine-<uid>/server-<dev>-<ino>`
/// (hex), keyed by the prefix directory's device and inode. A wineserver
/// serving the prefix runs with this as its working directory.
fn wine_server_dir(prefix: &Path) -> Option<PathBuf> {
    use std::os::unix::fs::MetadataExt;
    let meta = std::fs::metadata(prefix).ok()?;
    // SAFETY: getuid has no preconditions and cannot fail.
    let uid = unsafe { libc::getuid() };
    Some(PathBuf::from(format!(
        "/tmp/.wine-{uid}/server-{:x}-{:x}",
        meta.dev(),
        meta.ino()
    )))
}

/// True while any wineserver serves `prefix` — the game's, or any other
/// wine session in it. Identified the way wine itself pairs servers with
/// prefixes (the server directory above), so it doesn't depend on which
/// environment the server was started with.
pub fn prefix_in_use(prefix: &Path) -> bool {
    let Some(server_dir) = wine_server_dir(prefix) else {
        return false;
    };
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return false;
    };
    entries.flatten().any(|entry| {
        entry.file_name().to_string_lossy().parse::<u32>().is_ok()
            && std::fs::read_link(entry.path().join("exe"))
                .is_ok_and(|exe| normalize_lexical(&exe).to_string_lossy().ends_with("/wineserver"))
            && std::fs::read_link(entry.path().join("cwd")).is_ok_and(|cwd| cwd == server_dir)
    })
}

/// How a variable is changed for a command run with Proton's wine.
#[derive(Debug, PartialEq)]
pub enum EnvChange {
    Set(&'static str, String),
    /// Put this in front of the variable's current value (`:`-separated).
    Prepend(&'static str, String),
    Unset(&'static str),
}

/// The game's own Proton build's wine, set up the way Proton (and
/// protontricks) run it, for tools that drive wine themselves — winetricks.
/// Using the system wine against a Proton prefix instead runs a different
/// wine build over it: a mismatched wineboot, or a "version mismatch" from
/// the game's wineserver.
pub struct ProtonWine {
    files: PathBuf,
    prefix: PathBuf,
}

impl ProtonWine {
    pub fn for_target(target: &LaunchTarget) -> Result<Self> {
        let files = target.proton_dir.join("files");
        for bin in ["bin/wine", "bin/wineserver"] {
            if !files.join(bin).is_file() {
                return Err(anyhow!(
                    "The game's Proton build at {} has no {bin}, so the .NET installer can't \
                     be run with the wine that matches this prefix.",
                    target.proton_dir.display()
                ));
            }
        }
        Ok(Self { files, prefix: target.prefix_dir() })
    }

    pub fn wineserver(&self) -> PathBuf {
        self.files.join("bin/wineserver")
    }

    fn path(&self, rel: &str) -> String {
        self.files.join(rel).to_string_lossy().into_owned()
    }

    /// Mirrors what Proton's own script sets for its wine (`init_wine`):
    /// its bin dir first on PATH, its unix libraries and DLL directories,
    /// and no inherited WINEARCH.
    pub fn env(&self) -> Vec<EnvChange> {
        let wine = self.path("bin/wine");
        vec![
            EnvChange::Set("WINEPREFIX", self.prefix.to_string_lossy().into_owned()),
            EnvChange::Set("WINE", wine.clone()),
            EnvChange::Set("WINELOADER", wine),
            EnvChange::Set("WINESERVER", self.path("bin/wineserver")),
            EnvChange::Set(
                "WINEDLLPATH",
                format!("{}:{}", self.path("lib/vkd3d"), self.path("lib/wine")),
            ),
            EnvChange::Prepend(
                "LD_LIBRARY_PATH",
                format!(
                    "{}:{}",
                    self.path("lib/x86_64-linux-gnu"),
                    self.path("lib/i386-linux-gnu")
                ),
            ),
            EnvChange::Prepend("PATH", self.path("bin")),
            EnvChange::Unset("WINEARCH"),
        ]
    }

    pub fn apply(&self, cmd: &mut std::process::Command) {
        for change in self.env() {
            match change {
                EnvChange::Set(k, v) => {
                    cmd.env(k, v);
                }
                EnvChange::Prepend(k, v) => {
                    let value = match std::env::var(k) {
                        Ok(cur) if !cur.is_empty() => format!("{v}:{cur}"),
                        _ => v,
                    };
                    cmd.env(k, value);
                }
                EnvChange::Unset(k) => {
                    cmd.env_remove(k);
                }
            }
        }
    }

    /// The same environment as a shell command prefix (`env -u … VAR=…`),
    /// for the manual instructions.
    pub fn shell_env(&self) -> String {
        let mut unset = Vec::new();
        let mut set = Vec::new();
        for change in self.env() {
            match change {
                EnvChange::Set(k, v) => set.push(format!("{k}={}", shell_quote(&v))),
                EnvChange::Prepend(k, v) => {
                    set.push(format!("{k}={}\"${{{k}:+:${k}}}\"", shell_quote(&v)))
                }
                EnvChange::Unset(k) => unset.push(format!("-u {k}")),
            }
        }
        let mut parts = vec!["env".to_string()];
        parts.extend(unset);
        parts.extend(set);
        parts.join(" ")
    }
}

/// Single-quotes `s` for a POSIX shell.
pub fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
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

    // Where this trainer's output starts in the shared log, so a failure
    // diagnosis can read back just what it wrote (stderr goes to this file).
    let log_offset = std::fs::metadata(log_path).map(|m| m.len()).unwrap_or(0);

    let proton = target.proton_dir.join("proton");
    crate::applog::log(&format!(
        "launch_trainer: {} runinprefix {} (STEAM_COMPAT_CLIENT_INSTALL_PATH={} STEAM_COMPAT_DATA_PATH={})",
        proton.display(),
        trainer_path.display(),
        target.client_dir.display(),
        target.compatdata_dir.display(),
    ));
    let mut cmd = Command::new(&proton);
    cmd.arg("runinprefix")
        .arg(trainer_path)
        .env("STEAM_COMPAT_CLIENT_INSTALL_PATH", &target.client_dir)
        .env("STEAM_COMPAT_DATA_PATH", &target.compatdata_dir)
        .stdin(Stdio::null())
        .stdout(log_out)
        .stderr(log_err);
    spawn_tracked(
        cmd,
        LaunchInfo {
            log_path: log_path.to_path_buf(),
            log_offset,
            appid: target.appid,
            compatdata_dir: target.compatdata_dir.clone(),
            proton_dir: target.proton_dir.clone(),
        },
    )
    .with_context(|| format!("spawning {}", proton.display()))
}

/// What `launch_trainer` remembers about a launch, so a trainer that dies
/// right after starting can be diagnosed later.
struct LaunchInfo {
    log_path: PathBuf,
    log_offset: u64,
    appid: u32,
    compatdata_dir: PathBuf,
    proton_dir: PathBuf,
}

/// A tracked launch, keyed by its process group id (= the leader's pid).
///
/// `leader` is the spawned process, deliberately left unreaped (its exit is
/// only observed with `WNOWAIT`) until the launch is released. While it is a
/// zombie the kernel can't hand its pid to a new process, so no new process
/// group can take that number either: as long as the record exists, the
/// pgid can only mean this trainer's group, and signalling it can never hit
/// an unrelated group that happens to reuse the number.
struct LaunchRecord {
    started: std::time::Instant,
    info: LaunchInfo,
    leader: Option<std::process::Child>,
    /// Set by `stop_trainer`: a trainer the user stopped is not a failed launch.
    stopping: bool,
}

static LAUNCHES: LazyLock<Mutex<HashMap<u32, LaunchRecord>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn launches() -> std::sync::MutexGuard<'static, HashMap<u32, LaunchRecord>> {
    LAUNCHES.lock().unwrap_or_else(|e| e.into_inner())
}

/// Spawns `cmd` as the leader of a new process group and tracks it (see
/// `LaunchRecord`). Returns the process group id.
fn spawn_tracked(mut cmd: std::process::Command, info: LaunchInfo) -> Result<u32> {
    use std::os::unix::process::CommandExt;
    cmd.process_group(0);
    let child = match cmd.spawn() {
        Ok(child) => child,
        Err(e) => {
            crate::applog::log(&format!("launch_trainer: spawn failed: {e}"));
            return Err(e.into());
        }
    };
    let pid = child.id();
    crate::applog::log(&format!("launch_trainer: spawned pid {pid}"));
    launches().insert(
        pid,
        LaunchRecord {
            started: std::time::Instant::now(),
            info,
            leader: Some(child),
            stopping: false,
        },
    );

    // Report back to the caller immediately (the toast shouldn't wait on
    // this) but keep watching in the background: FLiNG trainers unpack
    // themselves and relaunch, so this initial process exiting quickly is
    // normal — logged as information, not an error — but the exit status
    // and anything it wrote to the log are the only window we get into a
    // Proton/wine-level failure our own code can't see. The watcher only
    // observes the exit; reaping waits for `release_launch` (see
    // `LaunchRecord`).
    std::thread::spawn(move || {
        for i in 0u32.. {
            std::thread::sleep(std::time::Duration::from_millis(if i < 10 { 200 } else { 1000 }));
            // Checked under the lock that `release_launch` takes, so the pid
            // can't be reaped (and reused) between the check and the peek.
            let peek = {
                let l = launches();
                if !l.contains_key(&pid) {
                    return;
                }
                peek_exit(pid)
            };
            match peek {
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
                }
                Err(e) => {
                    crate::applog::log(&format!("launch_trainer: waitid error for pid {pid}: {e}"));
                    return;
                }
            }
        }
    });

    Ok(pid)
}

/// The exit status of our child `pid` if it has exited, without reaping it.
fn peek_exit(pid: u32) -> std::io::Result<Option<String>> {
    // SAFETY: waitid only writes into the zeroed siginfo_t it is given.
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    let rc = unsafe {
        libc::waitid(
            libc::P_PID,
            pid as libc::id_t,
            &mut info,
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        )
    };
    if rc != 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: filled in by a successful waitid for a child-state change.
    let (child, status) = unsafe { (info.si_pid(), info.si_status()) };
    if child == 0 {
        return Ok(None);
    }
    Ok(Some(if info.si_code == libc::CLD_EXITED {
        format!("exit status: {status}")
    } else {
        format!("signal: {status}")
    }))
}

/// Stops tracking a launch and reaps its leader, which frees the pgid number
/// for reuse — so only once the group is gone (or has been killed).
fn release_launch(pgid: u32) -> Option<LaunchRecord> {
    let mut rec = launches().remove(&pgid)?;
    if let Some(mut leader) = rec.leader.take() {
        match leader.try_wait() {
            Ok(Some(_)) => {}
            // The leader left its own group and is still running: reap it
            // whenever it does exit, without blocking the caller.
            _ => {
                std::thread::spawn(move || {
                    let _ = leader.wait();
                });
            }
        }
    }
    Some(rec)
}

/// Sends `signal` to a tracked launch's process group. Never signals a pgid
/// that isn't a current launch of this app: the check and the kill happen
/// under the same lock `release_launch` takes, so the number can't be
/// released and reused in between.
fn signal_launch(pgid: u32, signal: libc::c_int) -> std::io::Result<bool> {
    let l = launches();
    if !l.contains_key(&pgid) || pgid < 2 {
        return Ok(false);
    }
    // SAFETY: plain syscall; a negative pid addresses the process group.
    if unsafe { libc::kill(-(pgid as libc::pid_t), signal) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(true)
}

/// A trainer whose whole process group is gone this soon after launch is
/// treated as a failed launch (a healthy trainer stays up until closed).
const EARLY_EXIT_SECS: u64 = 20;

/// How much of the trainer's output (its stderr goes to the app log) is
/// scanned for known failure signatures.
const OUTPUT_TAIL_BYTES: u64 = 16 * 1024;

pub fn record_failure(text: &str) {
    crate::applog::log(&format!("launch failure: {}", text.replace('\n', " | ")));
}

/// Facts gathered after an early exit, kept separate from the gathering so
/// the reasoning can be tested.
struct DiagInputs<'a> {
    game_running: bool,
    wineserver_proton: Option<&'a Path>,
    launched_proton: &'a Path,
    /// `Some(summary)` when the prefix's .NET is not usable.
    dotnet_problem: Option<String>,
    output: &'a str,
}

/// Likely causes of a trainer dying right after launch, most likely first.
/// Empty means nothing obvious was found.
fn likely_causes(i: &DiagInputs) -> Vec<String> {
    let mut causes = Vec::new();
    if !i.game_running {
        causes.push(
            "The game is no longer running. Start it, load past the menus, then launch the trainer again."
                .to_string(),
        );
    }
    let differs = i
        .wineserver_proton
        .is_some_and(|p| normalize_lexical(p) != normalize_lexical(i.launched_proton));
    if differs || i.output.contains("version mismatch") {
        causes.push(
            "The trainer ran under a different Proton build than the game's wineserver. \
             Close the game, start it again, then launch the trainer."
                .to_string(),
        );
    }
    if let Some(problem) = &i.dotnet_problem {
        causes.push(format!("{problem}. Launch again to run the one-time setup."));
    } else if i.output.contains("err:module:import_dll") {
        causes.push(
            "Wine could not load a library the trainer needs; ~/.local/share/steam-punk/steam-punk.log has the details."
                .to_string(),
        );
    }
    causes
}

fn read_output_tail(path: &Path, offset: u64) -> String {
    use std::io::{Read, Seek, SeekFrom};
    let Ok(mut file) = std::fs::File::open(path) else {
        return String::new();
    };
    let len = file.metadata().map(|m| m.len()).unwrap_or(0);
    let start = offset.max(len.saturating_sub(OUTPUT_TAIL_BYTES));
    let mut buf = Vec::new();
    if file.seek(SeekFrom::Start(start)).is_err() || file.take(OUTPUT_TAIL_BYTES).read_to_end(&mut buf).is_err() {
        return String::new();
    }
    String::from_utf8_lossy(&buf).into_owned()
}

/// Called once a tracked trainer's whole process group has exited: stops
/// tracking it (see `release_launch`). If that happened within
/// `EARLY_EXIT_SECS` of launch, the user didn't stop it, and a likely cause
/// is found, records it as the last failure and returns it (one cause per
/// line). Reads /proc and the prefix, so call via `spawn_blocking`.
pub fn finish_launch(pgid: u32) -> Option<String> {
    let rec = release_launch(pgid)?;
    let age = rec.started.elapsed().as_secs();
    if rec.stopping || age >= EARLY_EXIT_SECS {
        return None;
    }
    let rec_appid = rec.info.appid;
    crate::applog::log(&format!(
        "finish_launch: pgid {pgid} (AppId {rec_appid}) gone {age}s after launch"
    ));
    let rec = rec.info;

    let output = read_output_tail(&rec.log_path, rec.log_offset);
    let dotnet = dotnet_status(&rec.compatdata_dir.join("pfx"));
    let wineserver_proton = find_proton_dir_from_wineserver(&rec.compatdata_dir);
    let causes = likely_causes(&DiagInputs {
        game_running: find_running_appids().contains(&rec.appid),
        wineserver_proton: wineserver_proton.as_deref(),
        launched_proton: &rec.proton_dir,
        dotnet_problem: (!dotnet.is_usable()).then(|| dotnet.summary()),
        output: &output,
    });
    if causes.is_empty() {
        crate::applog::log("finish_launch: no obvious cause found");
        return None;
    }
    let text = causes.join("\n");
    record_failure(&text);
    Some(text)
}

/// Games shipped with online modes where trainers risk an account ban. Kept
/// to a handful of known titles; everything else needs anti-cheat in the
/// install folder (see `install_has_anticheat`) and confirmed online play
/// (see `gamedata::cached_online`).
const ONLINE_ANTICHEAT_APPIDS: &[(u32, &str)] = &[
    (3240220, "Grand Theft Auto V Enhanced"),
    (271590, "Grand Theft Auto V Legacy"),
];

/// True if the game's top-level install folder ships an online anti-cheat
/// (Easy Anti-Cheat or BattlEye). Best effort: a missing folder is just false.
fn install_has_anticheat(dir: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return false;
    };
    entries.flatten().any(|e| {
        e.file_name().to_str().is_some_and(|n| {
            let n = n.to_ascii_lowercase();
            n.starts_with("easyanticheat") || n.starts_with("battleye")
        })
    })
}

static ANTICHEAT_NOTICED: LazyLock<Mutex<HashSet<u32>>> = LazyLock::new(|| Mutex::new(HashSet::new()));

/// Whether to warn: the game is on the known online list, or anti-cheat is
/// in its install AND it is known to have online play. An unknown online
/// status (`None`) never warns on its own.
fn should_warn_anticheat(known: bool, detected: bool, online: Option<bool>) -> bool {
    known || (detected && online == Some(true))
}

/// The game's name if it is an online game with anti-cheat and the notice
/// has not yet been shown for it this session. Marks it shown, so each game
/// is warned about once per session. Reads local files and the cache only;
/// never touches the network.
pub fn anticheat_notice(target: &LaunchTarget) -> Option<String> {
    let known = ONLINE_ANTICHEAT_APPIDS
        .iter()
        .find(|(id, _)| *id == target.appid)
        .map(|(_, name)| (*name).to_string());
    let libs = steam::library_folders(&target.client_dir);
    let appid = target.appid.to_string();
    let online = gamedata::cached_online(target.appid);
    // Skip the folder scan when it cannot change the outcome.
    let detected = known.is_none()
        && online == Some(true)
        && steam::game_install_dir(&libs, &appid).is_some_and(|d| install_has_anticheat(&d));
    if !should_warn_anticheat(known.is_some(), detected, online) {
        return None;
    }
    let name = known.unwrap_or_else(|| {
        steam::game_name(&libs, &appid).unwrap_or_else(|| format!("AppId {}", target.appid))
    });
    let first_time = ANTICHEAT_NOTICED.lock().ok()?.insert(target.appid);
    first_time.then_some(name)
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
/// then SIGKILL after a ~2s grace period if it hasn't exited. Only a group
/// this app launched and still tracks is ever signalled (see
/// `LaunchRecord`). Blocks for up to that grace period — call via
/// `spawn_blocking`, not on the GTK thread.
pub fn stop_trainer(pgid: u32) {
    match launches().get_mut(&pgid) {
        Some(rec) => rec.stopping = true,
        None => {
            crate::applog::log(&format!(
                "stop_trainer: pgid {pgid} is not a running launch of this app; not signalling it"
            ));
            return;
        }
    }
    let r = signal_launch(pgid, libc::SIGTERM);
    crate::applog::log(&format!("stop_trainer: SIGTERM -{pgid} -> {r:?}"));

    // Same grace window as the post-launch quick-exit check above.
    for _ in 0..10 {
        std::thread::sleep(std::time::Duration::from_millis(200));
        if !process_group_alive(pgid) {
            crate::applog::log(&format!("stop_trainer: pgid {pgid} exited after SIGTERM"));
            release_launch(pgid);
            return;
        }
    }

    crate::applog::log(&format!("stop_trainer: pgid {pgid} still alive after 2s, sending SIGKILL"));
    let r = signal_launch(pgid, libc::SIGKILL);
    crate::applog::log(&format!("stop_trainer: SIGKILL -{pgid} -> {r:?}"));

    // Deliberately not waiting to confirm SIGKILL took effect: observed live
    // against a real wine/proton tree that full cleanup can take anywhere
    // from under a second to several seconds under load, with no reliable
    // upper bound worth blocking the caller on here. Releasing now is safe:
    // the killed members still hold the pgid number until they are gone, so
    // it can't be reused while any of them is alive.
    release_launch(pgid);
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
        if let Some(rest) = line.strip_prefix('[') {
            continuing = false;
            let key = rest.split(']').next().unwrap_or_default();
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
        if continuing || line.starts_with('"') || line.starts_with('@') {
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

/// Every `steamapps/compatdata` folder on this system: each library of the
/// Steam install the game runs from, plus those of any other Steam install
/// (native or Flatpak), without duplicates.
fn compatdata_roots(target: &LaunchTarget) -> Vec<PathBuf> {
    let mut clients = vec![target.client_dir.clone()];
    clients.extend(steam::metadata_roots());
    let mut seen: Vec<PathBuf> = Vec::new();
    let mut roots = Vec::new();
    for client in clients {
        for lib in steam::library_folders(&client) {
            let root = lib.join("steamapps/compatdata");
            let canon = std::fs::canonicalize(&root).unwrap_or_else(|_| root.clone());
            if root.is_dir() && !seen.contains(&canon) {
                seen.push(canon);
                roots.push(root);
            }
        }
    }
    roots
}

/// The first Proton prefix under any of `roots` (in order, entries sorted by
/// name so the choice is stable) that isn't `ours` and has a usable .NET.
/// Read-only: a donor is only ever copied from.
fn find_donor_prefix(roots: &[PathBuf], ours: &Path) -> Option<PathBuf> {
    let ours_canon = std::fs::canonicalize(ours).unwrap_or_else(|_| ours.to_path_buf());
    roots.iter().find_map(|root| {
        let mut prefixes: Vec<PathBuf> = std::fs::read_dir(root)
            .ok()?
            .flatten()
            .map(|e| e.path().join("pfx"))
            .collect();
        prefixes.sort();
        prefixes.into_iter().find(|p| {
            std::fs::canonicalize(p).unwrap_or_else(|_| p.clone()) != ours_canon
                && dotnet_status(p).is_usable()
        })
    })
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
    let ours = target.prefix_dir();
    let donor = find_donor_prefix(&compatdata_roots(target), &ours);

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

    fn inputs<'a>(output: &'a str, proton: &'a Path) -> DiagInputs<'a> {
        DiagInputs {
            game_running: true,
            wineserver_proton: Some(proton),
            launched_proton: proton,
            dotnet_problem: None,
            output,
        }
    }

    #[test]
    fn healthy_inputs_give_no_cause() {
        let p = PathBuf::from("/lib/Proton 9");
        assert!(likely_causes(&inputs("", &p)).is_empty());
    }

    #[test]
    fn game_gone_is_the_first_cause() {
        let p = PathBuf::from("/lib/Proton 9");
        let mut i = inputs("", &p);
        i.game_running = false;
        let causes = likely_causes(&i);
        assert_eq!(causes.len(), 1);
        assert!(causes[0].contains("no longer running"));
    }

    #[test]
    fn proton_build_mismatch_is_detected_from_paths_and_from_output() {
        let launched = PathBuf::from("/lib/Proton 9");
        let server = PathBuf::from("/lib/Proton - Experimental");
        let mut i = inputs("", &launched);
        i.wineserver_proton = Some(&server);
        assert!(likely_causes(&i)[0].contains("different Proton build"));

        let same = inputs("wine client error:1c: version mismatch 800/805", &launched);
        assert!(likely_causes(&same)[0].contains("different Proton build"));

        // Same build spelled with a `..` component is not a mismatch.
        let dotted = PathBuf::from("/lib/x/../Proton 9");
        let mut i = inputs("", &launched);
        i.wineserver_proton = Some(&dotted);
        assert!(likely_causes(&i).is_empty());
    }

    #[test]
    fn dotnet_problem_is_reported_with_its_summary() {
        let p = PathBuf::from("/lib/Proton 9");
        let mut i = inputs("", &p);
        i.dotnet_problem = Some(".NET runtime not usable (missing clr.dll)".to_string());
        let causes = likely_causes(&i);
        assert!(causes[0].starts_with(".NET runtime not usable (missing clr.dll)"));
    }

    #[test]
    fn dotnet_summary_names_what_is_missing() {
        let ready = DotnetStatus {
            clr_present: true,
            mscoree_native: true,
            crt_present: true,
            release: Some(0x00080eb1),
        };
        assert_eq!(ready.summary(), ".NET runtime ready");
        let broken = DotnetStatus {
            clr_present: true,
            mscoree_native: false,
            crt_present: false,
            release: Some(DOTNET_462_RELEASE - 1),
        };
        assert_eq!(
            broken.summary(),
            ".NET runtime not usable (missing native mscoree.dll, CLR support libraries, a .NET 4.6.2+ registry entry)"
        );
    }

    #[test]
    fn output_tail_reads_only_what_was_written_after_the_offset() {
        let s = Scratch::new("tail");
        let log = s.0.join("log.txt");
        std::fs::write(&log, "old line\nnew line\n").unwrap();
        assert_eq!(read_output_tail(&log, 9), "new line\n");
        assert_eq!(read_output_tail(&s.0.join("missing"), 0), "");
    }

    #[test]
    fn anticheat_is_detected_from_install_folder_and_known_list() {
        let s = Scratch::new("anticheat");
        assert!(!install_has_anticheat(&s.0));
        std::fs::create_dir_all(s.0.join("EasyAntiCheat")).unwrap();
        assert!(install_has_anticheat(&s.0));

        let s2 = Scratch::new("battleye");
        std::fs::create_dir_all(s2.0.join("BattlEye")).unwrap();
        assert!(install_has_anticheat(&s2.0));
        assert!(!install_has_anticheat(&s2.0.join("nope")));

        assert!(ONLINE_ANTICHEAT_APPIDS.iter().any(|(id, _)| *id == 3240220));
        assert!(ONLINE_ANTICHEAT_APPIDS.iter().any(|(id, _)| *id == 271590));
    }

    #[test]
    fn anticheat_warning_needs_a_known_game_or_detected_plus_online() {
        // Known list always warns, whatever the lookup says.
        assert!(should_warn_anticheat(true, false, None));
        assert!(should_warn_anticheat(true, false, Some(false)));
        // Detected anti-cheat warns only when online play is confirmed.
        assert!(should_warn_anticheat(false, true, Some(true)));
        assert!(!should_warn_anticheat(false, true, Some(false)));
        assert!(!should_warn_anticheat(false, true, None));
        // Online with no anti-cheat, or neither: no warning.
        assert!(!should_warn_anticheat(false, false, Some(true)));
        assert!(!should_warn_anticheat(false, false, None));
    }

    #[test]
    fn reg_query_dword_is_parsed() {
        let out = "\r\nHKEY_LOCAL_MACHINE\\Software\\Microsoft\\NET Framework Setup\\NDP\\v4\\Full\r\n    Release    REG_DWORD    0x80ff4\r\n\r\n";
        assert_eq!(parse_reg_query_dword(out, "Release"), Some(0x80ff4));
        assert_eq!(parse_reg_query_dword(out, "Version"), None);
        assert_eq!(parse_reg_query_dword("    Release    REG_SZ    4.8\n", "Release"), None);
        assert_eq!(parse_reg_query_dword("", "Release"), None);
    }

    /// A prefix that passes `dotnet_status`, built from the same pieces the
    /// real check looks at.
    fn fake_dotnet_prefix(pfx: &Path) {
        let fw = pfx.join("drive_c/windows/Microsoft.NET/Framework64/v4.0.30319");
        std::fs::create_dir_all(&fw).unwrap();
        std::fs::write(fw.join("clr.dll"), b"MZ real").unwrap();
        let sys32 = pfx.join("drive_c/windows/system32");
        std::fs::create_dir_all(&sys32).unwrap();
        std::fs::write(sys32.join("mscoree.dll"), b"MZ real").unwrap();
        std::fs::write(sys32.join("vcruntime140_clr0400.dll"), b"MZ real").unwrap();
        std::fs::write(
            pfx.join("system.reg"),
            "[Software\\\\Microsoft\\\\NET Framework Setup\\\\NDP\\\\v4\\\\Full] 1\n\"Release\"=dword:00080ff4\n",
        )
        .unwrap();
        assert!(dotnet_status(pfx).is_usable());
    }

    #[test]
    fn donor_is_found_in_another_steam_library() {
        let s = Scratch::new("donor");
        let lib_a = s.0.join("a/steamapps/compatdata");
        let lib_b = s.0.join("b/steamapps/compatdata");
        let ours = lib_a.join("100/pfx");
        std::fs::create_dir_all(&ours).unwrap();
        std::fs::create_dir_all(lib_a.join("101/pfx")).unwrap();
        fake_dotnet_prefix(&lib_b.join("200/pfx"));
        let roots = vec![lib_a.clone(), lib_b.clone()];
        assert_eq!(find_donor_prefix(&roots, &ours), Some(lib_b.join("200/pfx")));
        // Only the target's own library: nothing to copy from.
        assert_eq!(find_donor_prefix(&roots[..1], &ours), None);
        // The game's own prefix never donates to itself, even when usable.
        fake_dotnet_prefix(&ours);
        assert_eq!(find_donor_prefix(&roots[..1], &ours), None);
    }

    #[test]
    fn proton_wine_env_points_at_the_proton_build() {
        let s = Scratch::new("protonwine");
        let proton = s.0.join("Proton 9.0");
        let target = LaunchTarget {
            appid: 1,
            client_dir: s.0.join("Steam"),
            compatdata_dir: s.0.join("compatdata/1"),
            proton_dir: proton.clone(),
        };
        assert!(ProtonWine::for_target(&target).is_err(), "no wine binaries yet");
        std::fs::create_dir_all(proton.join("files/bin")).unwrap();
        std::fs::write(proton.join("files/bin/wine"), b"").unwrap();
        std::fs::write(proton.join("files/bin/wineserver"), b"").unwrap();
        let wine = ProtonWine::for_target(&target).unwrap();

        let mut cmd = std::process::Command::new("true");
        wine.apply(&mut cmd);
        let envs: HashMap<String, Option<String>> = cmd
            .get_envs()
            .map(|(k, v)| (k.to_string_lossy().into_owned(), v.map(|v| v.to_string_lossy().into_owned())))
            .collect();
        let files = proton.join("files");
        let at = |rel: &str| files.join(rel).to_string_lossy().into_owned();
        assert_eq!(envs["WINE"].as_deref(), Some(at("bin/wine").as_str()));
        assert_eq!(envs["WINESERVER"].as_deref(), Some(at("bin/wineserver").as_str()));
        assert_eq!(
            envs["WINEPREFIX"].as_deref(),
            Some(s.0.join("compatdata/1/pfx").to_string_lossy().as_ref())
        );
        assert!(envs["PATH"].as_deref().unwrap().starts_with(&format!("{}:", at("bin"))));
        assert_eq!(envs["WINEARCH"], None, "WINEARCH is removed");
    }

    #[test]
    fn shell_quote_survives_quotes_and_spaces() {
        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg(format!("printf %s {}", shell_quote("it's a \"Proton\" $dir")))
            .output()
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout), "it's a \"Proton\" $dir");
    }

    #[test]
    fn a_prefix_without_a_wineserver_is_not_in_use() {
        let s = Scratch::new("inuse");
        assert!(!prefix_in_use(&s.0));
        let dir = wine_server_dir(&s.0).unwrap();
        assert!(dir.to_string_lossy().starts_with("/tmp/.wine-"), "{dir:?}");
        assert!(!prefix_in_use(&s.0.join("missing")));
    }

    fn info() -> LaunchInfo {
        LaunchInfo {
            log_path: PathBuf::from("/dev/null"),
            log_offset: 0,
            appid: 0,
            compatdata_dir: PathBuf::from("/nonexistent"),
            proton_dir: PathBuf::from("/nonexistent"),
        }
    }

    fn proc_state(pid: u32) -> Option<String> {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        Some(stat.rsplit_once(')')?.1.split_whitespace().next()?.to_string())
    }

    #[test]
    fn an_exited_leader_holds_its_pid_until_the_launch_is_finished() {
        let mut cmd = std::process::Command::new("sh");
        cmd.args(["-c", "exit 3"]);
        let pgid = spawn_tracked(cmd, info()).unwrap();
        let mut zombie = false;
        for _ in 0..50 {
            if proc_state(pgid).as_deref() == Some("Z") {
                zombie = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(zombie, "the exited leader stays unreaped, holding pid {pgid}");
        assert!(!process_group_alive(pgid));
        assert_eq!(peek_exit(pgid).unwrap().as_deref(), Some("exit status: 3"));

        // Gone right after launch, with no game running: diagnosed as such.
        let diagnosis = finish_launch(pgid).expect("an early exit is diagnosed");
        assert!(diagnosis.starts_with("The game is no longer running."), "{diagnosis}");
        assert_eq!(proc_state(pgid), None, "reaped once finished");
        assert!(!launches().contains_key(&pgid));
    }

    #[test]
    fn stop_signals_only_a_tracked_group() {
        use std::os::unix::process::CommandExt;
        // Not launched through the tracker: Stop must leave it alone.
        let mut stranger = std::process::Command::new("sleep")
            .arg("30")
            .process_group(0)
            .spawn()
            .unwrap();
        let stranger_pgid = stranger.id();
        stop_trainer(stranger_pgid);
        assert!(stranger.try_wait().unwrap().is_none(), "an untracked group was signalled");
        assert!(!signal_launch(stranger_pgid, libc::SIGTERM).unwrap());
        stranger.kill().unwrap();
        stranger.wait().unwrap();

        let mut cmd = std::process::Command::new("sleep");
        cmd.arg("30");
        let pgid = spawn_tracked(cmd, info()).unwrap();
        assert!(process_group_alive(pgid));
        stop_trainer(pgid);
        assert!(!launches().contains_key(&pgid), "released after the stop");
        let mut gone = false;
        for _ in 0..50 {
            if proc_state(pgid).is_none() {
                gone = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(gone, "the tracked trainer was stopped and reaped");
    }

    /// Live check of the Proton paths against a throwaway prefix, never a
    /// game's: `SP_LIVE_PROTON` is a Proton build, `SP_LIVE_COMPATDATA` a
    /// scratch compatdata dir whose `pfx` that build already created. A
    /// persistent wineserver of that build stands in for the running game.
    /// With `SP_LIVE_WINETRICKS=1` it then runs the real installer fallback.
    /// Not run in CI.
    #[test]
    #[ignore]
    fn live_proton_registry_and_installer() {
        let proton = PathBuf::from(std::env::var("SP_LIVE_PROTON").expect("SP_LIVE_PROTON"));
        let compat = PathBuf::from(std::env::var("SP_LIVE_COMPATDATA").expect("SP_LIVE_COMPATDATA"));
        let target = LaunchTarget {
            appid: 0,
            client_dir: steam::steam_client_dir().expect("Steam install"),
            compatdata_dir: compat.clone(),
            proton_dir: proton,
        };
        let prefix = target.prefix_dir();
        assert!(prefix.join("system.reg").is_file(), "create the scratch prefix first");
        let wine = ProtonWine::for_target(&target).unwrap();
        let wait_for = |want: bool| {
            for _ in 0..100 {
                if prefix_in_use(&prefix) == want {
                    return;
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            panic!("prefix_in_use never became {want}");
        };

        // "The game is running."
        let mut server = std::process::Command::new(wine.wineserver());
        wine.apply(&mut server);
        assert!(server.env("STEAM_COMPAT_DATA_PATH", &compat).arg("-p").status().unwrap().success());
        wait_for(true);
        let err = crate::setup::install_dotnet48(&target).unwrap_err();
        println!("installer while running: {err:#}");
        assert!(err.to_string().contains("still running"), "{err:#}");

        // The repair's registry import into the running server, then the check.
        let donor = Scratch::new("live-donor");
        std::fs::write(
            donor.0.join("system.reg"),
            "WINE REGISTRY Version 2\n\n[Software\\\\Microsoft\\\\NET Framework Setup\\\\NDP\\\\v4\\\\Full] 1700000000\n#time=1\n\"Release\"=dword:00080ff4\n\"Version\"=\"4.8.04084\"\n",
        )
        .unwrap();
        apply_dotnet_registry(&target, &donor.0).unwrap();
        let disk = dotnet_release(&prefix);
        let live = live_dotnet_release(&target).unwrap();
        println!("right after the import: on disk {disk:?}, live {live:?}");
        assert_eq!(live, Some(0x80ff4));
        let (_, health) = prefix_health(&target);
        println!("prefix_health: {health}");
        assert!(!health.contains("registry entry"), "{health}");

        // "The game was quit": the server flushes the registry as it exits.
        let mut kill = std::process::Command::new(wine.wineserver());
        wine.apply(&mut kill);
        assert!(kill.arg("-k").status().unwrap().success());
        wait_for(false);
        println!("after the server exited: on disk {:?}", dotnet_release(&prefix));

        if std::env::var("SP_LIVE_WINETRICKS").as_deref() == Ok("1") {
            let started = std::time::Instant::now();
            let result = crate::setup::install_dotnet48(&target);
            println!("installer with the game closed ({}s): {result:?}", started.elapsed().as_secs());
            println!("prefix: {}", dotnet_status(&prefix).summary());
            assert!(!prefix_in_use(&prefix), "the installer's wineserver was left running");
        }
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

