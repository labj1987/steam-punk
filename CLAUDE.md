# Steam Punk (formerly proton-trainer / SteamPunk)

## Naming convention

Decided by the user: the display name is "Steam Punk". The binary, Cargo
crate, GitHub repo, AppImage filename, `.desktop` and icon filenames, data
dir and log files all use hyphenated lowercase `steam-punk`. The app ID
`io.github.labj1987.SteamPunk` and the polkit action ids stay UNCHANGED
(PascalCase) — never rename them. Old names (`steampunk`, `proton-trainer`)
appear only in deliberate back-compat/migration code (data-dir migration,
the legacy-named AppImage copy) — don't add new uses.

GTK4 + libadwaita desktop app, written in Rust, for launching Windows
game-trainer executables (e.g. FLiNG trainers) through Proton directly
against a currently running Steam game's existing wine session, on Linux.
Distributed as a single AppImage.

The trainer is run on the host as `<proton_dir>/proton runinprefix <trainer.exe>`
with `STEAM_COMPAT_CLIENT_INSTALL_PATH`/`STEAM_COMPAT_DATA_PATH` pointed at
the game's own compatdata, so the trainer's wine client joins the game's
existing wineserver (whose socket lives under `/tmp`, shared with the host)
instead of starting its own. The Proton build to use is detected from the
actual running `wineserver` process, not from `compatdata`'s `config_info`,
which has been observed stale.

## Module layout (`src/`)

- `main.rs` — entry point, runs the legacy data-dir migration before
  anything else touches it, sets up the shared Tokio runtime, wires up the
  GTK application.
- `ui.rs` — the GTK4/libadwaita UI: trainer list, import (drag-and-drop or
  +), AppID search/association, launch/troubleshoot dialogs.
- `launcher.rs` — resolves the running Proton build and compat paths,
  spawns the trainer, and the whole .NET-usability check/repair path (see
  gotcha below). Largest file in the app.
- `library.rs` — trainer list persistence (`trainers.json`), the
  pre-rename data-dir migration, per-trainer AppID metadata.
- `gamedata.rs` — resolves a game's name and cover art from a Steam AppID
  via the public Steam Store API/CDN (see caching design below).
- `steam.rs` — locates the local Steam client install and library folders.
- `setup.rs` — the privileged one-time system-package setup (wine32:i386,
  winetricks) via `pkexec`, same split as KernelPop's `install.rs`: system
  packages need root, the winetricks install itself only touches the
  user-owned wine prefix and needs no privilege escalation.
- `applog.rs` — single per-session log covering launches, setup steps, and
  user actions, with an in-app export so a user can hand it to whoever's
  troubleshooting without needing a terminal.

## Known quirks

**The dotnet40/wine-mono fix.** Proton's bundled wine-mono is not real .NET
and cannot run WPF-based trainers — current FLiNG trainers require .NET
Framework 4.6.2+, older ones run on 4.0. The app does modify the game's
prefix to fix this: the repair copies runtime files in and imports a `.reg`
file (via `proton runinprefix regedit`), or runs winetricks `dotnet48`. Checking the registry `Release`
value alone is not enough: wine-mono advertises a 4.8 `Release` in prefixes
that have no real .NET at all. `launcher::dotnet_status` instead verifies
everything that actually has to hold for the CLR to load: `clr.dll`
present, a real (non-builtin) `mscoree.dll`, the CRT libraries `clr.dll`
imports from system32 (`*_clr0400.dll` — missing these is silent, the CLR
simply never loads), and a registry `Release` of 4.6.2 or newer
(`DOTNET_462_RELEASE`). The one-time repair (`setup.rs`) prefers cloning a
working runtime from another game's Proton prefix on the same system over
the Microsoft installer — on Wine's new wow64 mode the installer fails
outright and its rollback strips .NET back out of the prefix, so trying it
first can leave a game worse off than before. `winetricks -f dotnet48` is
used with `-f` (force) because winetricks marks a verb done in
`winetricks.log` on first attempt regardless of whether it actually
succeeded.

**AppID cover-art caching design.** A trainer's optional AppID association
(set at import time, fully opt-in — reverses the original "no per-game
association" decision from 0.1.0) drives a name/cover-art fetch from the
public Steam Store API/CDN. This happens exactly once per AppID and is
cached to disk under `data_dir()/cache/` (`<appid>.name.txt`,
`<appid>.jpg`), keyed by AppID rather than by trainer since multiple
trainers can share one game. `gamedata::fetch_and_cache` treats a failed
name fetch as a real error but a failed cover fetch as log-only — a game
with a name and no art is still strictly better than falling back to the
filename. `cached_name`/`cached_cover` never hit the network; a cache miss
just means the caller falls back to the trainer's filename-derived title.

**The data-dir migration (proton-trainer/steampunk → steam-punk).**
`library::migrate_legacy_data_dir` renames `~/.local/share/proton-trainer`
and/or `~/.local/share/steampunk` to `~/.local/share/steam-punk` on first
run (merging without overwriting if both exist, and leaving a symlink at the
old `steampunk` path for not-yet-updated AppImages), so existing imported
trainers and logs survive. It
must run before anything else (including `applog`) touches the data dir —
called first thing in `main.rs`. The `proton-trainer-dotnet.reg` temp
filename in `launcher.rs` is unrelated and intentionally left as-is (a
temp file's name doesn't matter).

## Build process

`build-appimage.sh` builds the AppImage, same `appimagetool`-direct pattern
as GreenLight/KernelPop:
1. Installs build deps via apt (cargo, rustc, gtk4/adwaita dev headers,
   `wget`, `zsync`, `desktop-file-utils`). The `zsync` install is
   deliberately unconditional (not behind the `command -v cargo` guard) —
   in CI a prior step already installs cargo, so that guard evaluates
   false and anything gated behind it gets silently skipped.
2. `cargo build --release`.
3. Assembles the AppDir (binary, privileged script, polkit policy, appdata,
   desktop file, icon, generated `AppRun`).
4. Downloads `appimagetool` (cached in `.cache/`, optionally pinned via
   `APPIMAGETOOL_URL`/`APPIMAGETOOL_SHA256`) and packs the AppDir into
   `steam-punk-$VERSION-x86_64.AppImage`, with `UPDATE_INFORMATION` set for
   `gh-releases-zsync` delta updates (`steam-punk-*` pattern). An identical
   copy named `steampunk-$VERSION-...` (+ `.zsync`) is also produced and
   released so pre-rename installs, which embed the old pattern, can update.
5. Runs `zsyncmake` directly on the built AppImage to produce the `.zsync`
   sidecar (see KernelPop's CLAUDE.md for the diagnosis of why
   `appimagetool`'s own zsync generation silently no-ops on GitHub Actions
   runners). Keep that call non-fatal — the AppImage is valid without it.

## Release process

1. Bump `version` in `Cargo.toml`.
2. Add a `CHANGELOG.md` entry and an appdata `<release>` (the build also
   inserts the current version if it is missing).
3. Commit, push to `main`.
4. `git tag vX.Y.Z && git push origin vX.Y.Z`.
5. The tag push triggers `.github/workflows/release.yml` ("Build and
   Release"), which checks the tag matches `Cargo.toml`, runs
   `cargo test`, then `build-appimage.sh`, and uploads the AppImage
   (+ `.zsync`) to a GitHub Release via `softprops/action-gh-release`.

## Conventions

- Don't use `sed`/`awk` to edit files — use direct file writes/edits.
  `tee` is fine for one-off terminal inspection, but Claude Code sessions
  should edit files directly rather than shelling through it.
- Repo lives at `/home/alex/Projects/SteamPunk` (local dir name unchanged), owned by user `alex` — if
  operating as root, run git commands as `alex`
  (`su -s /bin/bash alex -c '...'`) to keep authorship and file ownership
  correct.
