# SteamPunk

GTK4 + libadwaita desktop app for launching Windows game-trainer executables
(e.g. FLiNG trainers) through Proton against a currently running Steam
game's wine session, on Linux. (Formerly known as proton-trainer.)

## Screenshot

Trainer list, showing cover art resolved from an associated Steam AppID, and a Stop button on any currently-running trainer:

![Trainer list showing imported games, cover art, and Launch/Stop controls](screenshots/main.png)

## How it works

The trainer is launched directly on the host — no Steam Linux Runtime
container, no umu-launcher:

```
<proton_dir>/proton run <trainer.exe>
```

with two environment variables set:

- `STEAM_COMPAT_CLIENT_INSTALL_PATH` — the Steam client install dir
- `STEAM_COMPAT_DATA_PATH` — `<library>/steamapps/compatdata/<appid>`

Because the game's wineserver socket lives under `/tmp`, which
pressure-vessel shares with the host, the trainer's wine client joins the
game's existing wineserver session and can see the running game process.

The trainer *must* be launched with the exact same Proton build whose
wineserver the game is currently running — a mismatch fails with
`wine client error: version mismatch`. This app detects that build from the
actual running `wineserver` process rather than trusting `compatdata`'s
`config_info`, which has been observed stale.

## Using it

1. Start the game, load past any menus.
2. Drag a trainer `.exe` onto the window (or use the + button) to import it.
3. Click Launch on the trainer's row. A running trainer shows a live badge and a Stop button in its place.

If the game's prefix doesn't have real .NET Framework 4.0 yet (required by
WPF-based trainers, and never provided by Proton's bundled wine-mono), the
app shows the exact one-time `winetricks` commands to run instead of
launching.

## Requirements

The AppImage does not bundle GTK or libadwaita: it uses the host's, so the
system needs **GTK 4 and libadwaita >= 1.5**. Automatic one-time setup (system
packages for the .NET install) is Debian/Ubuntu-only; other distributions get
the manual commands to run instead.

## Building

```
./build-appimage.sh
```

Produces `steampunk-<version>-x86_64.AppImage` (+ `.zsync` sidecar).

## Non-goals

No trainer downloading/update-checking, no SLR/pressure-vessel/umu
integration, no prefix modification (the app only detects and instructs),
no per-game trainer database.

## License

AGPL-3.0-or-later. See [LICENSE](LICENSE).

Code at or before commit `0d97e95b89080c5b53efca8880abfd8834d3009a` remains available
under the MIT License per its original release. From this commit forward,
AGPL-3.0-or-later.
