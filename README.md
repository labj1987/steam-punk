# Steam Punk

Desktop app for launching Windows game-trainer executables
(e.g. FLiNG trainers) through Proton against a currently running Steam
game's wine session, on Linux. It looks and works the same on every desktop.

## Screenshot

The trainer list, with cover art from each trainer's Steam AppID. A running trainer shows a Running badge and a Stop button in place of Launch:

![Trainer list showing imported games, cover art, and Launch buttons](screenshots/main.png)

## How it works

The trainer is launched directly on the host — no Steam Linux Runtime
container, no umu-launcher:

```
<proton_dir>/proton runinprefix <trainer.exe>
```

with two environment variables set:

- `STEAM_COMPAT_CLIENT_INSTALL_PATH` — the Steam client install dir
- `STEAM_COMPAT_DATA_PATH` — `<library>/steamapps/compatdata/<appid>`

Because the game's wineserver socket lives under `/tmp`, which
pressure-vessel shares with the host, the trainer's wine client joins the
game's existing wineserver session and can see the running game process.
Under Flatpak Steam the game's `/tmp` is private to Steam's sandbox, so the same
command runs inside the sandbox with `flatpak enter <wineserver pid>` and joins
the game's wineserver there.

It works with native and Flatpak Steam, and with Proton 8 and older as well as
Proton 9 and newer and GE-Proton builds.

The trainer *must* be launched with the exact same Proton build whose
wineserver the game is currently running — a mismatch fails with
`wine client error: version mismatch`. This app detects that build from the
actual running `wineserver` process rather than trusting `compatdata`'s
`config_info`, which has been observed stale.

## Using it

1. Start the game, load past any menus.
2. Drag a trainer `.exe` onto the window (or use the + button) to import it.
   Importing copies the `.exe` into `~/.local/share/steam-punk/trainers/`, then
   asks which Steam game it is for: search by name or enter an AppID, or skip.
   That only sets the name and cover shown; to change it later, remove the
   trainer and import it again.
3. Click Launch on the trainer's row. A running trainer shows a Running badge
   and a Stop button in its place. The trainer goes into the running game's
   prefix: with no game running you are asked to start one first, and with
   several running you pick which one.

The trash button on a row deletes Steam Punk's copy of that trainer and its
AppID link at once.

Game names and cover art come from your local Steam install (including
Flatpak Steam) when available, and from the public Steam Store otherwise. On
import, the app suggests the installed game that matches the trainer's file
name. Online games with an anti-cheat get a warning the first time you launch a
trainer for them in each session, with Cancel or Launch Anyway
(single-player-only games never do): use trainers in single-player only. If a trainer dies right after launch, the likely cause
(game closed, different Proton build, missing .NET) is shown in a notification,
and the full log is at `~/.local/share/steam-punk/steam-punk.log`.

If the game's prefix doesn't have a real .NET Framework 4.6.2 or newer yet
(required by current WPF-based trainers, and never provided by Proton's
bundled wine-mono), the app offers a one-time setup. It first tries to copy a
working runtime from another game's Proton prefix in any of your Steam
libraries, which works with the game running. Otherwise it installs `dotnet48`
with `winetricks`, run with the game's own Proton wine. That installer needs the
game closed: the app asks you to quit the game, installs, then tells you to
start the game again and launch the trainer. The installer downloads about
200 MB the first time; a step that hangs is stopped after a time limit, and the
message names it. If winetricks or cabextract is missing and you choose the
automatic setup, they are installed after a password prompt on Debian/Ubuntu;
elsewhere the manual commands are shown. This modifies the game's Wine prefix:
it copies in the .NET runtimes and imports a `.reg` file, or, through
winetricks, installs `dotnet48` and sets the prefix to Windows 10.

## Requirements

Steam Punk needs no GTK, Qt or other toolkit from the system: it draws its own window,
the same on every desktop, and follows the desktop's light or dark setting and accent colour. What it
takes from the system is what every desktop has:

- **glibc 2.39 or newer** (Ubuntu 24.04, Fedora 40, Debian 13 and later).
- **A graphics driver with Vulkan or OpenGL ES.** Any working GPU driver has one. With
  no GPU, Mesa's software renderers do (`mesa-vulkan-drivers` on Debian, Ubuntu and
  Fedora; `vulkan-swrast` on Arch).
- The X11 and Wayland client libraries: `libxcb`, `libxkbcommon`, `libxkbcommon-x11`,
  and `libwayland-client` on a Wayland session.

If a library is missing or the system is too old, Steam Punk says which, and how to
install it, instead of not starting. File dialogs, the light or dark setting and the accent colour come
through the desktop's portal (`xdg-desktop-portal`); without one, dropping a trainer
on the window still imports it.

Automatic one-time setup
(system packages for the .NET install) is Debian/Ubuntu-only and needs polkit
(`pkexec`); other distributions get the manual commands to run instead.
If the setup says no polkit authentication agent is running, start one and try again: KDE,
GNOME and COSMIC include one; on Hyprland use `hyprpolkitagent`, on sway or i3 use
`polkit-gnome` (`policykit-1-gnome` on Debian/Ubuntu) or `lxqt-policykit`. This also holds
for a desktop started from a text console: the setup never asks for the password there.

Tested desktops: GNOME, KDE Plasma, Hyprland, sway and COSMIC. Other Wayland and X11 desktops should work too.
Input methods (IBus and Fcitx 5) work in the search field.

## Known issues

- **While a dialog is open, move the window by the dialog's title**; the window's own title
  bar does not respond until the dialog is closed.
- **Screen readers.** Buttons, the trainer list, the search field and all text in dialogs
  are exposed, and notifications are announced. A dialog itself
  has no name: its title names the group that holds its content, where the keyboard
  focus goes when it opens, and is the first text inside it. The window's minimise, maximise and close
  buttons and the two links in About are not exposed (close the window with your
  desktop's shortcut, such as Alt+F4). These are limits of the UI toolkit, GPUI Kit
  ([longbridge/gpui-kit issue 3659](https://github.com/longbridge/gpui-kit/issues/3659)
  covers announcements).
- **The file dialog shows every file**, not only `.exe` files. Picking or dropping
  anything else is refused with a message.
- **The font is one your system already has** (Ubuntu, Adwaita Sans, Cantarell, Noto Sans
  or DejaVu Sans, whichever is found first), not necessarily the one your desktop is
  set to.
- **Starting Steam Punk a second time** shows the window that is already open. On Wayland
  it comes to the front when the second start came from your launcher, dock or menu, which
  hands over the permission to do so (a compositor may still refuse: GNOME shows a
  "Steam Punk is ready" notice instead, as it does for other apps, and Hyprland refuses
  unless `misc:focus_on_activate` is on); started from a terminal, the desktop only marks the
  window as wanting attention. On X11 the busy cursor of the second start goes away on its
  own after a few seconds. If the open copy is still starting, the second start says so in a
  notice and its window appears when it is ready. If the open copy does not respond, the second start says so and
  names its process number; close that process and start Steam Punk again.
- **KDE Plasma:** while the file dialog is open, the Steam Punk window can be hidden
  behind it; it comes back when the dialog closes.

## Install, update and uninstall

Download `steam-punk-<version>-x86_64.AppImage` from
[Releases](https://github.com/labj1987/steam-punk/releases), make it executable
(`chmod +x`) and run it. The AppImage carries zsync update information, so
AppImageUpdate-compatible tools can update it in place.

The automatic setup installs `/usr/lib/steam-punk/privileged-setup.sh` and the
polkit policy `/usr/share/polkit-1/actions/io.github.labj1987.SteamPunk.setup.policy`,
installs `winetricks` and `cabextract` with apt, and logs to
`/var/log/steam-punk.log`.

To uninstall, delete the AppImage and `~/.local/share/steam-punk/`; if the
automatic setup ran, also remove `/usr/lib/steam-punk/`, that polkit policy and
`/var/log/steam-punk.log`.

## Building

```
./build-appimage.sh
```

Produces `steam-punk-<version>-x86_64.AppImage` (+ `.zsync` sidecar). Needs
cargo, a C compiler, `pkg-config` and the headers the UI toolkit builds against
(`libxcb1-dev`, `libxkbcommon-dev`, `libxkbcommon-x11-dev`, `libfontconfig1-dev`; installed
with apt, through sudo, if missing; release builds run on Ubuntu 24.04), `zsync` for the
sidecar, and network access for the pinned appimagetool and AppImage runtime it downloads.

## Non-goals

No trainer downloading/update-checking, no SLR/pressure-vessel/umu
integration, no per-game trainer database. (The app does modify a game's
prefix, but only to repair a missing .NET runtime, and only on request.)

## Acknowledgements

Development assistance: Claude Code (Anthropic) and Codex (OpenAI).

## License

AGPL-3.0-or-later. See [LICENSE](LICENSE).

The window is drawn with [GPUI Kit](https://github.com/longbridge/gpui-kit) and GPUI
(Apache-2.0), with icons from [Lucide](https://lucide.dev) (ISC). The AppImage carries the
licence notices of these and of every other Rust crate built into it, in
`usr/share/licenses/steam-punk/THIRD-PARTY-LICENSES.txt` (`--appimage-extract` shows it;
`scripts/third_party_licenses.py` writes it: each crate's own licence files, or, for a crate
that ships none, the standard text of the licences it names).

Code at or before commit `0d97e95b89080c5b53efca8880abfd8834d3009a` remains available
under the MIT License per its original release. From this commit forward,
AGPL-3.0-or-later.

Linnard Alex Brown Jr.
