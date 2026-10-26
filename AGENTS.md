# Steam Punk (formerly proton-trainer / SteamPunk)

## Naming convention

Decided by the user: the display name is "Steam Punk". The binary, Cargo
crate, GitHub repo, AppImage filename, `.desktop` and icon filenames, data
dir and log files all use hyphenated lowercase `steam-punk`. The app ID
`io.github.labj1987.SteamPunk` and the polkit action ids stay UNCHANGED
(PascalCase) — never rename them. Old names (`steampunk`, `proton-trainer`)
are gone from the code with no back-compat of any kind — don't add new uses.

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
which has been observed stale. Proton builds keep their wine under `files/`
(Proton 9 and later) or `dist/` (Proton 8 and older); both are recognised.

Every Proton command for a running game goes through
`launcher::proton_command`. Under Flatpak Steam the game's wineserver runs in
the Steam sandbox, whose `/tmp` is private, so a host-side `proton` can't
reach it and would start a second wineserver on the prefix. When the game's
wineserver is in a Flatpak sandbox (`/proc/<pid>/root/.flatpak-info`
exists), the command runs as `flatpak enter <wineserver pid> env
STEAM_COMPAT_…=… <proton> …`, which joins that process's namespaces and
environment. Verified with a Flatpak app on a native system: env passing,
the private `/tmp`, and that the entered processes stay in the host process
group of `flatpak enter` (so trainer tracking and Stop work). Not verified
against a real Flatpak Steam game. The game's Steam install (native or
Flatpak) is the one whose library holds its compatdata.

Proton commands the app waits on have timeouts (`launcher::run_with_timeout`
kills the command's process group): registry read/import 90 s, winetricks
60 min, the installer's wineserver exit 2 min; after a winetricks timeout
the prefix's wine is stopped with Proton's `wineserver -k` (the game is
closed then, so it is all the installer's).

A launched trainer is tracked by its process group id, and the group's
leader is deliberately left unreaped (its exit is observed with `WNOWAIT`)
until the UI sees the group gone or Stop finishes: while the leader is a
zombie its pid, and so the pgid number, can't be reused, so Stop can never
signal an unrelated group. Stop only signals pgids that are still tracked.

`trainers.json` (the AppID associations) is never overwritten when it can't
be parsed: the list still shows the trainers by filename, the user gets the
error, and saving an AppID fails instead of wiping the file.

## Module layout (`src/`)

- `main.rs` — entry point, sets up the shared Tokio runtime, wires up the
  GTK application.
- `ui.rs` — the GTK4/libadwaita UI: trainer list, import (drag-and-drop or
  +), AppID search/association, launch/troubleshoot dialogs.
- `launcher.rs` — resolves the running Proton build and compat paths,
  spawns the trainer, and the whole .NET-usability check/repair path (see
  Known quirks below). Largest file in the app.
- `library.rs` — trainer list persistence (`trainers.json`), per-trainer
  AppID metadata.
- `gamedata.rs` — resolves a game's name and cover art from a Steam AppID,
  local Steam data first and the public Steam Store API/CDN as the fallback
  (see caching design below); also image-size probing and the trainer-name to
  installed-game matcher.
- `steam.rs` — locates the local Steam client installs (native and Flatpak),
  library folders, and parses appmanifests (name, installdir, installed games).
- `setup.rs` — the privileged one-time system-package setup (winetricks,
  cabextract) via `pkexec`, and the winetricks `dotnet48` fallback: system
  packages need root, the winetricks install itself only touches the
  user-owned wine prefix and needs no privilege escalation.
- `applog.rs` — single per-session log covering launches, setup steps, and
  user actions, at `~/.local/share/steam-punk/steam-punk.log` (unit tests
  log to stderr instead, never to a file). Only the
  primary instance starts (truncates) it, from GApplication `startup`: a
  second launch forwards to the running instance and must not wipe its log,
  whose byte offsets the launch diagnosis reads trainer output back from.
- `fsutil.rs` — `write_atomic` (temp file, fsync, rename, directory fsync),
  used for every app data file: `trainers.json` and the cover/name cache.

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
(`DOTNET_462_RELEASE`). While the game's wineserver runs, a `Release` that
falls short on disk is re-read from the live registry (`proton runinprefix
reg query`): the server keeps the registry in memory and only writes
`system.reg` out periodically and on exit, so keys the repair's regedit
import just added aren't on disk yet. "A wineserver serves this prefix" is
`launcher::prefix_in_use`: a wineserver whose working directory is wine's
`/tmp/.wine-<uid>/server-<dev>-<ino>` for the prefix.

The one-time repair prefers cloning a working runtime from another game's
Proton prefix on the same system (every library's `compatdata`, every Steam
install) over the Microsoft installer — on Wine's new wow64 mode the
installer fails outright and its rollback strips .NET back out of the
prefix, so trying it first can leave a game worse off than before. Cloning
works with the game running. The installer fallback
(`setup::install_dotnet48`) does not: winetricks runs `wineserver -w`
between steps, which waits for every process in the prefix, so the user is
asked to quit the game first and the app waits for its wineserver to go.
winetricks is run with the game's own Proton wine (`launcher::ProtonWine`:
`WINE`, `WINELOADER`, `WINESERVER`, `WINEDLLPATH`, `LD_LIBRARY_PATH`, `PATH`
as that Proton version's own `init_wine` sets them — `lib64`+`lib` for
Proton 9 and the `dist/` builds, per-triplet `lib/<arch>-linux-gnu` plus
`lib/vkd3d` for Proton 10+ and GE; check a new layout's `init_wine` before
changing this), never the system wine, which is a
different wine build than the one that made the prefix. The system packages
are therefore only winetricks and cabextract (no `wine32:i386`). `winetricks
-f dotnet48` is used with `-f` (force) because winetricks marks a verb done
in `winetricks.log` on first attempt regardless of whether it actually
succeeded. The dialog's manual commands are generated from the same
environment and arguments, so they match what the automatic path runs.

**AppID cover-art caching design.** A trainer's optional AppID association
(set at import time, fully opt-in — reverses the original "no per-game
association" decision from 0.1.0) drives a name/cover-art lookup. The local
Steam install is tried first (`appmanifest_<appid>.acf` name, and the header
art in `appcache/librarycache/<appid>/` chosen by decoded dimensions, plus
the old flat `<appid>_*` files); the public Steam Store API/CDN fills in only
what is missing, again with header art. Covers are always header art
(460x215), never the portrait capsule: the list gives every cover one
landscape slot, and a mix of shapes made the rows uneven. This happens exactly once per AppID and is
cached to disk under `data_dir()/cache/` (`<appid>.name.txt`,
`<appid>.jpg`), keyed by AppID rather than by trainer since multiple
trainers can share one game. Cache files are written atomically, and a
cached cover only counts if it is a whole image (JPEG/PNG end marker
present), so a torn file is a cache miss rather than a broken cover forever.
Image URLs from the Store API are only fetched from Steam's https image CDN
(`*.steamstatic.com`, `steamcdn-a.akamaihd.net`), and the shared HTTP client
follows redirects only between Steam hosts. `gamedata::fetch_and_cache` treats a failed
name fetch as a real error but a failed cover fetch as log-only — a game
with a name and no art is still strictly better than falling back to the
filename. `cached_name`/`cached_cover` never hit the network; a cache miss
just means the caller falls back to the trainer's filename-derived title.

## Build process

`build-appimage.sh` builds the AppImage, `appimagetool`-direct
(no `linuxdeploy`). It runs as an ordinary user and writes only inside the
checkout:
1. Only on a machine with no toolchain (no cargo or no GTK4 headers) does it
   install build deps via apt (cargo, rustc, gtk4/adwaita dev headers,
   `wget`, `zsync`, `file`, `desktop-file-utils`); that is the one part
   needing root, run through `sudo` when the script isn't root. In CI the workflow installs the packaging tools, and the
   script stops early if `wget`, `file` or `desktop-file-validate` is missing.
2. `cargo build --release --locked`. Rust is pinned in `rust-toolchain.toml`;
   bump it there and in both workflows together (each checks it).
3. Assembles the AppDir (binary, appdata, desktop file, icon, generated
   `AppRun`) and runs `desktop-file-validate` on the copied desktop file
   (fatal). `AppRun` only starts the app; it asks for no password. The
   privileged script and polkit policy are compiled into the binary
   (`include_bytes!`), with their SHA-256 pinned at build time by `build.rs`,
   not shipped as files (an extracted AppDir is user-writable). They must sit
   at fixed system paths for polkit, and `setup::run_system_setup` installs
   them only when the user starts the automatic setup and the system copies
   are missing or differ: one pkexec call runs `scripts/install-privileged.sh`
   (also compiled in), which copies the user-staged files into root-owned
   temp files, checks the pinned sums, installs both and runs the setup
   script. That one prompt is pkexec's generic one for `/bin/bash`: polkit
   can only show the policy's message once the policy is installed. When
   the system copies are current, pkexec runs the script directly and
   polkit shows the policy's own message. `APT_INSTALL` in setup.rs and the
   script's install line are held together by a unit test.
4. Downloads `appimagetool` (pinned to 1.9.1 and verified against a SHA-256
   checksum in the script, cached in `.cache/`) and packs the AppDir into
   `steam-punk-$VERSION-x86_64.AppImage`, with `UPDATE_INFORMATION` set for
   `gh-releases-zsync` delta updates (`steam-punk-*` pattern). The AppImage
   type2 runtime is pinned and checksum-verified the same way and passed with
   `--runtime-file` (otherwise `appimagetool` downloads a moving build).
5. Runs `zsyncmake` directly on the built AppImage to produce the `.zsync`
   sidecar (`appimagetool`'s own zsync generation silently no-ops on GitHub Actions
   runners). A missing or failing `zsyncmake` is fatal when `CI` is set (a
   release without the sidecar cannot update); a local build only warns.

## Release process

1. Bump `version` in `Cargo.toml`.
2. Add a `CHANGELOG.md` entry (see Changelog below).
3. Run `python3 scripts/sync_appdata_releases.py` to regenerate the
   appdata `<releases>` list; CI (`.github/workflows/ci.yml`, on every push
   to `main` and every pull request: build, test, clippy, shellcheck and the
   changelog checks) and the release workflow fail if it is out of
   date.
4. Commit, push to `main`.
5. `git tag vX.Y.Z && git push origin vX.Y.Z`.
6. The tag push triggers `.github/workflows/release.yml` ("Build and
   Release"). It first runs the whole of `ci.yml` (called as a reusable
   workflow: build, tests, clippy with `-D warnings`, shellcheck, the
   changelog and AppStream checks) on the tagged commit. Only then does its
   `build` job, with a read-only token, check the tag matches `Cargo.toml`
   and the changelog and run `build-appimage.sh`. A separate `publish` job, the only one with write
   access, attaches the AppImage (+ `.zsync`) to a GitHub Release via
   `softprops/action-gh-release`, with that version's changelog section as
   the release text.

Actions in both workflows are pinned to full commit SHAs; the weekly
maintenance round updates them (see `.github/dependabot.yml`). Every job has a
`timeout-minutes`; CI cancels a superseded pull-request run, and a release
is never cancelled midway (one per tag at a time).

## Changelog

- One `## X.Y.Z — YYYY-MM-DD` heading per released version, newest
  first. No entries for builds that were never released.
- Write each entry for the people using the app: what changed for them
  and anything they need to do. Leave out implementation detail (file
  paths, flags, internal names, CI and packaging changes) unless a user
  needs it to act.
- The release page is the version's section written out in full
  (`scripts/release_notes.py`), never a link to the changelog. The
  release fails if the section is missing.
- The appdata `<releases>` list is generated from the headings
  (`scripts/sync_appdata_releases.py`). Don't edit it by hand. The script
  fails when the metadata doesn't have exactly one `<releases>` block.

## Conventions

- Don't use `sed`/`awk` to edit files — use direct file writes/edits.
  `tee` is fine for one-off terminal inspection, but Claude Code sessions
  should edit files directly rather than shelling through it.
- Repo lives at `/home/alex/Projects/steam-punk` (GitHub repo: `labj1987/steam-punk`), owned by user `alex` — if
  operating as root, run git commands as `alex`
  (`su -s /bin/bash alex -c '...'`) to keep authorship and file ownership
  correct.

## Shared memory

The owner's Claude sessions (local, cloud and chat) share notes in the Google Drive folder "Claude Memory", one doc per topic; this repo's doc is "app: steam-punk". If you have the Google Drive connector:

- Before starting work, read that doc and the "prefs:" docs for decisions, things already tried or ruled out, and the release state.
- After meaningful work (a release, a decision, a failed attempt worth not repeating, an open issue), update the doc. The connector can't edit a doc's text: create a replacement doc with the same title in that folder, then trash the old one. The folder's "README - how this folder works" doc has the conventions.
