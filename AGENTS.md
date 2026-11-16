# Steam Punk (formerly proton-trainer / SteamPunk)

## Naming convention

Decided by the user: the display name is "Steam Punk". The binary, Cargo
crate, GitHub repo, AppImage filename, `.desktop` and icon filenames, data
dir and log files all use hyphenated lowercase `steam-punk`. The app ID
`io.github.labj1987.SteamPunk` and the polkit action ids stay UNCHANGED
(PascalCase) — never rename them. Old names (`steampunk`, `proton-trainer`)
are gone from the code with no back-compat of any kind — don't add new uses.

Desktop app, written in Rust with the GPUI Kit UI toolkit (see "UI" below), for launching Windows
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

- `main.rs` — entry point: claims the single instance, starts the log and the shared Tokio
  runtime, runs the GPUI application.
- `ui.rs` — the GPUI Kit UI: trainer list, import (drag-and-drop or
  +), AppID search/association, launch/troubleshoot dialogs.
- `instance.rs` — one running copy per user login: a socket in `XDG_RUNTIME_DIR` (bound under a
  `flock`ed lock file; a socket file is removed only when nobody listens on it); a second
  launch sends `activate <token>\n` and waits for the answer. The listener thread answers
  `starting\n` at once until the window's first drawn frame sets `UiUp` (`on_next_frame` in
  `ui::build_ui`); the UI loop answers `ok\n` (`Activation::done`) after it has raised the
  window. The second launch waits 2 s for any answer and 15 s in all for `ok`. Still
  "starting" after that -> `Claim::Starting` (main.rs shows `starting_message` as information
  and exits 0: the request is queued and the window comes up on its own); no answer ->
  `Claim::NotResponding` ("not responding ... (PID n)", the PID from `SO_PEERCRED`, as an
  error, exit 1). A connection closed with no answer (EOF or reset: the copy is exiting) is
  asked again every 75 ms until nobody listens (this launch becomes the primary) or the 2 s
  answer wait runs out (exit quietly). `instance::release` removes the socket file, then shuts the
  listener (this order matters: a launch in between finds no file, not a dead socket), and
  `ui::no_window` also drops `Activations` so queued requests see EOF and retry; it runs when the last window closes and before `ui::no_window` shows its
  message, so no launch waits on a copy that is going away. Without `XDG_RUNTIME_DIR` there is
  no single-instance check.
- `notice.rs` — a message with no window of the app's own, as an error (no graphics driver, a
  running copy that does not answer) or as information (a running copy still starting):
  zenity (`--error`/`--info`), kdialog (`--error`/`--msgbox`), notify-send, `gdbus` straight to
  `org.freedesktop.Notifications.Notify`, xmessage; the first that shows it ends the chain. An
  error's notifications are critical with expiry 0 (COSMIC drops others after seconds, and has
  no notify-send); information is normal urgency with the default expiry. A dialog's stderr
  goes to a temporary file read after it exits (a pipe would wait for anything it left
  running); exit 1 with "cannot open display" there is not shown. AppRun's `libcheck_show`
  (`libcheck_dialog`) tries the same programs in the same order for errors (a test compares
  the two); keep them in step.
- `activation.rs` — raises the window for that second launch on Wayland: GPUI has no call that
  takes an activation token, so `xdg_activation_v1.activate(token, surface)` is sent on GPUI's
  own connection through the window's raw handles (wayland-client as a guest on the foreign
  `wl_display`). No token (a start from a terminal) or X11: `Window::activate_window`, which on
  Wayland only marks the window as wanting attention. Passing the token also ends the launcher's
  startup feedback for the second copy; on X11 nothing completes it and it times out.
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
  user-owned wine prefix and needs no privilege escalation. pkexec always runs through
  `run_pkexec`: `setsid` (no controlling terminal) and stdin from /dev/null, so on a desktop
  started from a text console with no polkit agent it fails at once instead of prompting on
  that console. No pkexec on PATH -> `NO_PKEXEC_MESSAGE` (apt's install command: the
  automatic setup is only offered where apt-get is); 126, and 127 with "Not authorized" (polkit-kde-agent's Cancel) ->
  "cancelled or refused"; 127 with a no-agent text -> `NO_AGENT_MESSAGE`.
- `applog.rs` — single per-session log covering launches, setup steps, and
  user actions, at `~/.local/share/steam-punk/steam-punk.log` (unit tests
  log to stderr instead, never to a file). Only the
  primary instance starts (truncates) it, after `instance::claim`: a
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

The AppImage holds one binary, as cargo built it, and no libraries. The binary links glibc, libgcc_s,
libxcb, libxkbcommon and libxkbcommon-x11, and loads by name at run time libwayland-client (Wayland
sessions), libvulkan and/or libEGL (with libwayland-egl) and then the GPU driver's own libraries; it
loads no GTK, GLib, Qt, fontconfig or freetype. AppRun's `libcheck_run` checks this before the app
starts: `ldd -r` on the binary must show no `not found` line and no unresolved symbol of the binary
itself (a system library's own lazy ones don't count; when ldd fails without naming anything, it
asks the loader directly), and `libcheck_runtime` looks in the loader cache (`ldconfig -p`, 64-bit
entries, plus `LD_LIBRARY_PATH`) for libwayland-client on a Wayland session and for at least one of
libvulkan and libEGL. A cache that does not list glibc (NixOS and the like) says nothing, so nothing
is claimed. The message names the glibc version needed and found, or the missing libraries with the
install command for dnf, zypper, apt or pacman (`libcheck_package` holds the package names: add a
row when the binary gains a library). A loader that is there with no working driver behind it is
the app's part: `ui::no_window` shows it through `notice::show_error` (see notice.rs) and
exits. `tests/libcheck.rs` runs the block in bash with a fake ldd, ldconfig and os-release.
Nothing sets `LD_LIBRARY_PATH` or any toolkit variable, so the programs the app starts (pkexec,
proton, winetricks) get the user's environment as it is: plain `std::process::Command`.

`build-appimage.sh` builds the AppImage, `appimagetool`-direct
(no `linuxdeploy`). It runs as an ordinary user and writes only inside the
checkout:
1. Only on a machine with no toolchain (no cargo, or pkg-config can't find xcb, xkbcommon,
   xkbcommon-x11 and fontconfig) does it
   install build deps via apt (cargo, rustc, build-essential, pkg-config, `libxcb1-dev`,
   `libxkbcommon-dev`, `libxkbcommon-x11-dev`, `libfontconfig1-dev` (one crate probes it at build
   time; the binary does not load it),
   `wget`, `zsync`, `file`, `desktop-file-utils`); that is the one part
   needing root, run through `sudo` when the script isn't root. In CI the workflow installs the packaging tools, and the
   script stops early if `wget`, `file` or `desktop-file-validate` is missing.
2. `cargo build --release --locked`. Rust is pinned in `rust-toolchain.toml`;
   bump it there and in both workflows together (each checks it).
3. Assembles the AppDir (binary, appdata, desktop file, icon, generated
   `AppRun`) and runs `desktop-file-validate` on the copied desktop file
   (fatal). `scripts/third_party_licenses.py` writes the notices of every crate in the build
   (from `cargo metadata` and the crates' own licence files; for a crate without one, the standard
   text of each licence its SPDX expression names, kept in `scripts/spdx/`: the build fails when
   a text is missing there, so add it) to
   `usr/share/licenses/steam-punk/THIRD-PARTY-LICENSES.txt`; CI runs it too.
   `AppRun` only starts the app; it asks for no password. The
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

## UI (GPUI Kit)

- The toolkit is `gpui-kit`, pinned to an exact version in Cargo.toml (0.9.x releases weekly and
  pins GPUI itself as the `gpui-pre` snapshot crates). It draws with wgpu: Vulkan first, OpenGL ES
  through EGL second; Mesa's software drivers count. Bump it on purpose, then re-run the desktop
  checks below. Its sources are the reference: `~/.cargo/registry/src/*/gpui-component-*`,
  `gpui-base-*`, `gpui-pre-*`.
- Shape: one view (`MainView`) and two small views for dialog content that changes while open
  (`AppIdPrompt`, `DotnetPrompt`). Dialog builders run on every frame (`Fn`), so one-shot state
  travels in `Rc<RefCell<Option<..>>>`. `spawn_async` runs a future on Tokio and delivers the
  result on the UI thread.
- Dialogs: always through `window.open_dialog` with `.overlay(false)` (the toolkit's dimming layer
  also paints the client-side shadow margin, a dark frame around the window on KDE and GNOME), the
  content in `named_group(..)`, the buttons in `dialog_buttons()` inside that group (not the
  toolkit's footer), the main button given the focus with `InitialFocus`, and the text above the
  buttons capped with `dialog_room(window)` and scrollable: the toolkit cuts off a dialog taller
  than the window. Run what a button starts with `window.defer` after `close_dialog`, so a
  follow-up dialog is not the one that gets closed.
- Enter in a dialog: the toolkit sends Enter to the dialog's `on_ok` before the focused button
  sees it. Every dialog therefore sets `on_ok` and acts as the focused button (`enter_key`):
  confirm only when the action button has the focus, cancel on Cancel, and return false (stay
  open) anywhere else. The AppID dialog (a text field plus Skip, Add and a Use button per game) acts on Skip, Add or the focused Use button, leaves Enter to the field in the search field, and returns false everywhere else (dialog surface, the field's clear button, the toolkit's close button, whose focus the dialog cannot see): it never closes itself. The default button is
  the one given the focus at opening; the anti-cheat warning's is Cancel.
- Nothing may show a notification before the window has the toolkit's root view (that is,
  from `MainView::new`): `push_notification` panics there. `show_toast` returns false instead,
  and the first `refresh` runs from `build_ui` after `open_window`.
- Screen readers: GPUI reports only elements with an id and a role. Use `label(id, text)` for
  text (never a bare string child in a dialog), `accessibility_label` on icon buttons and on
  buttons that repeat per row, and `toast()` for messages (it also feeds the `Announcer` live
  region; the toolkit's notifications are not announced, upstream issue longbridge/gpui-kit#3659).
  The toolkit's dialog cannot be named in 0.9.1; `named_group` plus `InitialFocus` is the stand-in.
  Check with AT-SPI (`Atspi` from Python: tree, FOCUSED state, `object:announcement` events) after
  switching accessibility on (`org.a11y.Status IsEnabled`).
- Fonts: `ui::init` names installed families for the UI and fixed-width fonts before the toolkit
  starts; without that GPUI warns about ".SystemUIFont" on every Linux system. Nothing may log at
  warn level on a normal start (`STEAM_PUNK_LOG=info` shows the renderer picked). The logger in
  main.rs caps chatty dependencies (zbus, zvariant, ashpd, the async and HTTP crates: `NOISY`)
  at warn whatever `STEAM_PUNK_LOG` asks for; add a crate there when it floods info.
- The file dialog (portal) has no file-type filter in GPUI: the `.exe` check is after the pick
  and on drop (`NOT_AN_EXE`).
- Testing the UI without Steam or a game: a scratch `HOME` with a pretend Steam install
  (`steamapps/libraryfolders.vdf`, `appmanifest_<appid>.acf`, `compatdata/<appid>/pfx` with an
  empty `system.reg`, `config_info` naming a Proton dir) and a sleeping process whose command
  line holds `SteamLaunch AppId=<appid>` reach every launch dialog: anti-cheat (AppID 3240220),
  one-time setup, quit-the-game, the pkexec prompt, and the game picker with two such processes.
  Never do this against the real `HOME`: Launch would start wine in a real prefix. Run GUI checks
  on a private display or in a VM, with a private `XDG_RUNTIME_DIR`.
- Before a release, on GNOME, KDE Plasma, Hyprland and COSMIC: start, import by + and by drop,
  the dialogs above, light/dark switch, polkit prompt and its cancel / no-agent messages; and
  with no GPU: lavapipe, GLES only, no driver (the app's dialog), no loader (AppRun's message).
  Retake `screenshots/main.png` when the list's look changes.

## Rust

- Toolchain: Rust 1.100 or newer. `rust-toolchain.toml` pins the exact release that local, CI and release builds use; `rust-version = "1.100"` in Cargo.toml is the minimum.
- Dependency cooldown: `.cargo/config.toml` sets `[registry] global-min-publish-age = "3 days"`, so resolution only takes crate versions at least 3 days old (Dependabot's cargo `cooldown` matches). To take a newer version early for a security advisory: `CARGO_RESOLVER_INCOMPATIBLE_PUBLISH_AGE=allow cargo update -p <crate> --precise <version>`. Until that version is 3 days old, a plain `cargo update` moves it back down, so run that round's `cargo update` with the same variable set (or update the other crates with `-p`).
- Never compare tool versions as text or floats ("1.100" sorts before "1.99" as text): split into integers, or use `sort -V`.
- Never read Cargo's intermediate layout under `target/` (`build/`, `deps/`, `incremental/`, `.fingerprint/`): it is not a stable interface and changed in Cargo 1.100. Use the final artifact paths, `OUT_DIR` inside build.rs, or `cargo build --message-format=json`.
- Cargo's own lints are on (`[lints.cargo]`, `unused_dependencies = "warn"`) and CI sets `CARGO_BUILD_WARNINGS=deny`, so a lint warning fails CI. A direct dependency kept only to turn on a feature of a crate already in the graph stays in `[dependencies]` with a comment saying why: the lint does not flag it, and moving it to a `cfg(false)` table would turn the feature off. (`[target.'cfg(false)'.dependencies]` is only for a dependency listed just to pin a transitive crate's version.)

## Formatting

Run `cargo fmt --all` before every commit; CI runs `cargo fmt --all -- --check`. The one-off
formatting commit is listed in `.git-blame-ignore-revs`; to have local `git blame` skip it, run
`git config blame.ignoreRevsFile .git-blame-ignore-revs`.

## Test builds

- A fix for an issue someone else reported goes to them before it reaches `main`: push it on its own branch,
  carrying the version bump and CHANGELOG section it will be released with, open a PR (CI must pass), then run
  `gh workflow run release.yml --ref BRANCH -f issue=N`. The Build and Release workflow builds that branch
  exactly as a release and publishes it as a pre-release named "Test build for #N (BRANCH)" with the tag
  `test-BRANCH-RUN` (never `v*`, never the latest release, so the AppImage updater does not offer it).
- Link the pre-release in the issue and ask the reporter to try it. Merge and release only once they confirm
  it fixes the problem (or say plainly why you release without that). After the release, delete the test
  pre-releases and their tags (`gh release delete TAG --cleanup-tag --yes`).

## Release process

Dry run (the release build of a branch, nothing published, nobody notified): `gh workflow run release.yml --ref BRANCH -f dry_run=true`; the AppImage and .zsync are the run's `release-files` artifact.

1. Bump `version` in `Cargo.toml`.
2. Add a `CHANGELOG.md` entry (see Changelog below).
3. Run `python3 scripts/sync_appdata_releases.py` to regenerate the
   appdata `<releases>` list; CI (`.github/workflows/ci.yml`, on every push
   to `main` and every pull request: rustfmt check, build, test, clippy, shellcheck and the
   changelog checks) and the release workflow fail if it is out of
   date.
4. Run `cargo fmt --all` (CI fails on unformatted code), then commit and push to `main`.
5. `git tag vX.Y.Z && git push origin vX.Y.Z`.
6. The tag push triggers `.github/workflows/release.yml` ("Build and
   Release"). It first runs the whole of `ci.yml` (called as a reusable
   workflow: rustfmt check, build, tests, clippy with `-D warnings`, shellcheck, the
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
