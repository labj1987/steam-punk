# Changelog

## 0.4.8 — 2026-09-21

- The app is now displayed as "Steam Punk" (About dialog, window title, desktop
  entry, appdata). Internal identifiers are unchanged. Credits use the full
  name, and the About dialog now shows the AGPL-3.0 license.
- Fixed re-importing a trainer that is already in the library truncating it to
  zero bytes; imports are now copied atomically (temp file, then rename).
  Trainers with an uppercase `.EXE` extension now show up in the list.
- Fixed Stop All killing unrelated processes (it used `pkill -f` with the
  trainers path as a regex); it now stops only the tracked process groups.
- Fixed the Proton-dir fallback misfiring on library paths containing
  "proton"; it now splits on the `/files/` marker.
- Fixed a fast double-click on Launch orphaning an untracked process.
- Cover art: retry a missing name or cover in later sessions, reject
  non-image or oversized responses before caching, debounce store search, and
  share one HTTP client.
- The .NET check and the running-trainer poll no longer run on the GTK thread.
- Packaging: automatic setup is only offered where `apt-get` exists; AppRun
  installs the privileged script via a checksum-verified root-owned copy;
  polkit auth is no longer cached (`auth_admin`); build script fixes (apt
  ordering, live appimagetool cache, optional checksum pin). README notes the
  libadwaita >= 1.5 requirement.
- CI: the release workflow now checks the tag against `Cargo.toml` and runs
  `cargo test`; appdata release list filled in and generated at build time;
  tokio features narrowed; docs corrected (`runinprefix`, .NET 4.6.2, prefix
  modification); the temporary `.reg` file is deleted after import.

## 0.4.7 — 2026-09-17

- Fixed the .NET repair still failing on any prefix it had already tried once.
  0.4.6 fixed replacing the builtin `mscoree.dll` symlink, but the repair never
  reached that step: it aborts earlier, while re-cloning the framework trees.
  `std::fs::copy` preserves the donor's permission bits, so the first clone
  leaves read-only files behind (28 of them, measured on the real prefix), and
  `fs::copy` cannot overwrite a read-only file — so the first repair on a fresh
  prefix appeared to work and every repair after it died partway through. Both
  the tree copy and the CLR support-library copy now unlink the destination
  before writing, which covers read-only files and Proton's builtin symlinks
  with the same rule.

## 0.4.6 — 2026-09-17

- Fixed the .NET repair silently aborting partway, which left a prefix that
  looked repaired but could never run a trainer. A Proton prefix ships its
  builtin DLLs as symlinks into the Proton installation itself, which is
  read-only; `std::fs::copy` follows symlinks, so copying the donor prefix's
  native `mscoree.dll` over the builtin tried to write it *into Proton's own
  tree*, failed with permission denied, and took the whole repair down with
  it. The framework trees (`Microsoft.NET`, `assembly`) were cloned first and
  so appeared to succeed, while the CLR support libraries that follow them
  were never copied at all — leaving `mscoree.dll` as Wine's 109-byte stub,
  `has_usable_dotnet` false, and every .NET trainer unable to load the CLR.
  Confirmed live against Cyberpunk 2077's prefix, whose `system32/mscoree.dll`
  was a symlink to `Proton-GE Latest/files/lib/wine/x86_64-windows/mscoree.dll`
  while the donor prefix held the real 444 KB native DLL. The destination is
  now unlinked before copying, which is also what makes the result correct
  rather than merely writable: overriding a builtin means a real file in the
  prefix, not a redirect back to the one being overridden.
- The repair now logs each failure and the number of CLR support libraries it
  copied into each directory. The abort above surfaced only as a dialog, so
  the app log showed the framework trees being cloned and then simply stopped,
  with nothing to say the repair had failed — a count of zero would have named
  the problem immediately.

## 0.4.5 — 2026-09-09

- Credits Claude Code (Anthropic) in the About dialog's acknowledgements.

## 0.4.4 — 2026-08-12

- Fixed AppID search returning zero results for any trainer whose guessed
  search term included "Early Access" (common in filenames for early-
  access games) — Steam's storesearch API reliably returns 0 hits for a
  query containing that phrase, confirmed live, even for real, currently-
  listed games. The term guesser now strips it wherever it appears.
- Fixed cover art sometimes never getting cached even after successfully
  picking a game — the guessed CDN image paths (library_600x338.jpg,
  then flat header.jpg) 404 for a growing number of current games now
  that Steam has moved most of them to content-hashed asset paths;
  confirmed live for Monster Hunter Stories 3 (AppID 2852190), which 404s
  on both. Now falls back to the authoritative `header_image` URL from
  the same appdetails response already fetched for the name. Also: a
  trainer with a resolved name but no cover (from before this fix, or any
  future transient failure) now gets one retry per AppID per app session
  instead of being stuck with no art forever.

## 0.4.3 — 2026-08-12

- Actually fixed the trainer list compressing instead of scrolling this
  time — 0.4.2's fix didn't hold up under real testing. Root cause,
  confirmed by screenshotting a real build under X11: without an explicit
  `max-content-height` on the list's `ScrolledWindow`, it tries to grow to
  fit *all* content before ever committing to scrolling, so on first
  launch or a large/maximized window, whatever doesn't fit gets silently
  clipped instead of scrolled. Capped it so the list always commits to
  scrolling past a fixed height, verified with up to 28 trainers in a
  maximized window.
- Fixed trainer/game titles containing `&` (e.g. "Mount & Blade II:
  Bannerlord") rendering as a blank row — row titles are parsed as Pango
  markup, not plain text, and an unescaped `&` breaks that parsing
  silently. Escaped everywhere a resolved game name, trainer filename, or
  Steam search result can reach a title/subtitle.

## 0.4.2 — 2026-08-12

- Fixed trainer rows getting squeezed smaller instead of scrolling as more
  are added, on a large/maximized window — `Stack`'s default
  `vhomogeneous` was coupling the trainer list's height request to the
  empty-state page's, and the list wasn't reporting its true natural
  height to the surrounding `ScrolledWindow`.

## 0.4.1 — 2026-08-12

- Trainer rows with a resolved game name now show just that name, without
  the trainer's own filename underneath.

- Fixes the 0.4.0 release build, which failed in CI: its gtk4/libadwaita
  feature flags (`v4_22`/`v1_9`) required a newer system GTK4/libadwaita
  than the GitHub Actions runner's `libgtk-4-dev` provides (4.14.5).
  Dialed the feature flags back to `v4_12`/`v1_4,v1_5` — nothing in this
  release actually needs the newer APIs.

## 0.4.0 — 2026-08-12

- Trainer rows now show a live running/stopped badge, and a Stop button
  appears while a trainer is running — SIGTERM, then SIGKILL after a
  grace period if needed, targeting the trainer's whole process group so
  it actually stops the running trainer (not just the launcher wrapper).
- Trainer rows with a resolved game name now show just that name, without
  the trainer's own filename underneath.
- New native 16–512px icon set rendered from a new `data/icon.svg` source,
  replacing the single `steampunk.png`.
- Added `CLAUDE.md` covering build/release process and architecture.
- AppStream metadata: added `developer`, `url`, and `content_rating` tags.
- Dependency bump: gtk4 0.11, libadwaita 0.9, glib/gio 0.22, gdk4 0.11,
  matching GreenLight/KernelPop's target versions.

## 0.3.0 — 2026-08-05

- The AppID prompt shown after importing a trainer is now a live search
  instead of a blind numeric entry. It's pre-filled with a title guessed
  from the trainer's filename and searches Steam as you type, showing
  each result's name and cover thumbnail to pick from. Typing or pasting
  a raw numeric AppID still works too, via the Add button or Enter, for
  anyone who already knows it.

## 0.2.0 — 2026-08-05

proton-trainer is now SteamPunk. This is a pure rebrand of the earlier
0.1.x releases — application ID, package name, desktop file, icon, and
every other user-facing name changed, but none of the actual
launch/dotnet-repair logic did. Existing users' imported trainers and
logs are migrated automatically from `~/.local/share/proton-trainer` to
`~/.local/share/steampunk` the first time the new build runs.

- Trainer list rows now show the game's name and Steam library cover art
  instead of the raw trainer filename/version string, for any trainer
  associated with a Steam AppID. Associating one is optional — the app
  asks for it right after importing a trainer (skip it, or leave it
  blank, and nothing changes) — and the name/art are fetched once from
  Steam's public API and CDN and cached locally, never re-fetched on
  later launches. Trainers without an AppID, including everything
  imported before this release, keep exactly the previous filename-only
  display.
  - This required adding an optional per-trainer AppID association,
    which reverses the earlier "no per-game association" design
    decision from 0.1.0 — reliable cover art isn't possible from a
    filename alone. It's additive and fully optional, not a breaking
    change.

## 0.1.14 — 2026-08-05

- With more than one game running, the app now asks which one the trainer is
  for instead of picking whichever the system happened to list first. Since
  trainers are game-specific, guessing wrong meant attaching to the wrong
  game's prefix — which fails in confusing ways rather than visibly. Games are
  listed by name, read from their Steam appmanifest. With a single game running
  nothing changes; there's no extra prompt.
- The troubleshoot dialog disambiguates the same way, so it can no longer list
  stale trainer logs from a different game than the one you meant.

## 0.1.13 — 2026-08-05

- Fixed trainers that open but never attach — the window appears with an empty
  option list and blank game name/process ID, or reports "This trainer requires
  .NET Framework 4.6.2 or higher". Current FLiNG trainers need .NET 4.6.2+,
  while older ones run on 4.0, so a prefix could work for one game and fail for
  another with no obvious difference. The .NET check now verifies everything
  that actually has to hold: clr.dll present, a real (non-builtin) mscoree.dll,
  the CRT libraries clr.dll imports from system32 (`*_clr0400.dll` — missing
  these is silent, the CLR simply never loads), and a registry `Release` value
  of 4.6.2 or newer. Checking the registry alone is not enough: wine-mono
  advertises a 4.8 `Release` in prefixes that have no real .NET at all.
- The one-time .NET setup now repairs a prefix by cloning a working runtime
  from another game's Proton prefix on the same system, and only falls back to
  the Microsoft installer when there's nothing to clone. On Wine's new wow64
  mode that installer fails outright and its rollback strips .NET back out of
  the prefix, so trying it first could leave a game worse off than before.
  Cloning also needs no system packages, so the password prompt is only reached
  when it's genuinely required.

## 0.1.12 — 2026-08-05

- Fixed trainers that never open at all against certain games — spawned,
  then exited immediately (Wine `STATUS_DLL_NOT_FOUND` / exit 53), even
  though `has_dotnet40`'s clr.dll check passed. Root cause, found via a
  manual `WINEDEBUG=+file,+seh` trace: `has_dotnet40` only checked for
  clr.dll, but trainers import `mscoree.dll` directly, and a prefix can end
  up with Wine's own non-functional builtin `mscoree.dll` still physically
  in `system32`/`syswow64` even when the `native` DLL override is set
  correctly — clr.dll alone doesn't catch this. `has_dotnet40` now also
  verifies `mscoree.dll` isn't Wine's builtin placeholder.
- The one-time .NET setup no longer silently no-ops on a prefix it's meant
  to repair: `winetricks dotnet48` now runs with `-f`, since winetricks
  marks a verb done in `winetricks.log` on first attempt regardless of
  whether it actually succeeded. If the installer still doesn't produce a
  working `mscoree.dll` afterward (observed on Wine's new wow64 mode: its
  own NGEN helper process needs a working `mscoree.dll` just to start, so
  it can crash before ever writing a real one), setup now falls back to
  copying a working `mscoree.dll` from another Proton prefix on the same
  system — mscoree.dll is a thin, largely version-generic redirector, and
  the actual versioned CLR implementation it hands off to installs
  correctly regardless.

## 0.1.11 — 2026-08-05

- The debug log now records the prefix's drive-letter mappings
  (`dosdevices/c:`, `z:`, etc.) and whether each one's target actually
  resolves, right before launching a trainer. A stale or broken mapping
  here is a known cause of Wine returning "network path not found" for an
  otherwise-valid path — something our own code previously had no way to
  see. COM/LPT port symlinks are skipped since those are unrelated to file
  paths and routinely dangling on machines without legacy serial hardware.

## 0.1.10 — 2026-08-05

- The debug log now watches the spawned Proton process for up to 2 seconds
  after launch and records its exit status, without delaying the "Launched"
  toast — giving visibility into Proton/wine-level failures that happen
  after our own code hands off, which the log previously couldn't see.

## 0.1.9 — 2026-08-04

- Added a unified, per-session debug log covering everything the app does —
  resolved launch targets, the exact Proton command run, setup steps, and
  user actions — plus a new "Save Debug Log" button in the header that
  exports it (along with the privileged setup log, if present) to a file
  you pick, so anyone can troubleshoot a failed launch without needing a
  terminal.

## 0.1.8 — 2026-08-04

- Fixed trainers failing to launch against a running game.

## 0.1.7 — 2026-08-04

- The app now sets up the required Windows compatibility component
  automatically — no more copying commands into a terminal. Clicking
  Launch on a game that needs it shows a "Set Up Automatically" dialog
  instead of raw shell commands; confirming installs the one-time system
  packages (a single password prompt, via `pkexec` — same pattern already
  used for other privileged operations in this app family) and then runs
  `winetricks dotnet48 win10` against the game's prefix, launching the
  trainer automatically once done. A collapsed "Show manual commands
  instead" disclosure keeps the exact commands available for advanced
  users or troubleshooting.
- Switched the installed runtime from `dotnet40` to `dotnet48`:
  `dotnet40` reliably fails to install a working `clr.dll` on this
  Ubuntu/wine combination (winetricks reports success, but the actual
  runtime binary silently doesn't land), while `dotnet48` is
  backward-compatible for FLiNG trainers and installs correctly. The
  `clr.dll` presence check (`has_dotnet40`) is unaffected — both versions
  populate the same path.

## 0.1.6 — 2026-08-04

- Root cause of the persistent phantom taskbar entry confirmed with
  `WAYLAND_DEBUG=1 <appimage> 2>&1 | grep set_app_id`: on Wayland, GTK4
  announces the GApplication ID (`io.github.labj1987.ProtonTrainer`) as
  the toplevel's `app_id`, not `prgname`. The `.desktop` file's
  `StartupWMClass` was set to the old prgname (`proton-trainer`), so
  GNOME Shell couldn't match the running window to the desktop
  launcher — one process, two dock entries. Fixed by setting both
  `prgname` (in `main.rs`) and `StartupWMClass` (in the `.desktop`
  file) to the application ID, so the match works regardless of
  backend. The AboutDialog/Dialog conversions and `StartupNotify=true`
  from 0.1.1–0.1.5 were reasonable but orthogonal — this is the actual
  fix.

- The 0.1.1/0.1.3/0.1.4 fixes addressed real GTK-window-subclass dialogs
  but the phantom dock icon persisted immediately at launch, before any
  dialog could fire. Root cause: `proton-trainer.desktop` was missing
  `StartupNotify=true`. Without it, GNOME Shell has no way to associate
  the launch sequence with the eventual mapped window, so it leaves an
  orphaned placeholder entry in the dock (generic icon, tooltip showing
  only the raw `io.github.labj1987.ProtonTrainer` app ID) alongside the
  real, correctly-iconed window. NVI already had `StartupNotify=true` and
  never exhibited this. Added the line to match.

## 0.1.4 — 2026-08-04

- The About dialog used `gtk4::AboutDialog`, and the Troubleshoot dialog
  used `libadwaita::Window` — both are real `Gtk.Window` subclasses and
  create separate top-level Wayland surfaces, each showing as a second,
  unnamed window in the dock (the same bug class as the `MessageDialog`
  fix in 0.1.1). Switched to `libadwaita::AboutDialog` and
  `libadwaita::Dialog` respectively, both `Adw.Dialog` subclasses that
  render as sheets inside the main window's own surface.

## 0.1.3 — 2026-08-04

- 0.1.2 corrected the `UPDATE_INFORMATION` string but the fix never took
  effect: `build-appimage.sh` passed it to `appimagetool` as an
  environment variable, and this appimagetool build (continuous, git
  8c8c91f) silently ignores that env var — it only reads update info via
  the `-u`/`--updateinformation` CLI flag. Confirmed by inspecting the
  0.1.2 release AppImage's `.upd_info` ELF section: empty. Switched to
  passing `-u "$UPDATE_INFORMATION"` as an argument.

## 0.1.2 — 2026-08-04

- Fixed `UPDATE_INFORMATION` in `build-appimage.sh` to reference the
  `.zsync` sidecar filename instead of the AppImage filename, per the
  AppImage update spec. This was breaking update detection in tools like
  Gear Lever even though the `.zsync` sidecar was already being generated
  and published correctly. Packaging-only fix, no application behavior
  changes.

## 0.1.1 — 2026-08-04

- Fixed a phantom, unnamed taskbar/dock window appearing whenever the
  one-time .NET 4.0 setup dialog was shown. It used
  `libadwaita::MessageDialog`, a `Gtk.Window` subclass that creates a real
  separate top-level Wayland surface without the app's icon/app_id.
  Switched to `libadwaita::AlertDialog`, which renders as a sheet inside
  the main window's own surface instead.

## 0.1.0 — 2026-07-17

Initial release.

- Detects the running Steam game from `/proc` and resolves its Proton build
  from the actual running wineserver process (not from potentially stale
  `config_info` metadata), falling back to `config_info` only if no
  wineserver is found yet.
- Launches a trainer `.exe` directly on the host via `<proton>/proton run`
  with `STEAM_COMPAT_CLIENT_INSTALL_PATH` and `STEAM_COMPAT_DATA_PATH` set,
  so it joins the game's existing wineserver session.
- Trainer library: drag-and-drop or file-picker import into
  `~/.local/share/proton-trainer/trainers/`, launch, remove, stop all.
- Detects when a game's prefix lacks real .NET Framework 4.0 (required by
  FLiNG's WPF trainers, absent under Proton's bundled wine-mono) and shows
  the exact one-time `winetricks` setup commands instead of failing silently.
- Stale-instance recovery: lists FLiNG trainer-log folders in the prefix so
  a stuck `info.ini` lock can be cleared without leaving the app.
