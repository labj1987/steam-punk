#!/usr/bin/env bash
# build-appimage.sh — build the Steam Punk AppImage.
# Run from the repo root on Ubuntu (the GitHub Actions runner), as an ordinary user: it writes
# only inside the checkout. Only its from-scratch dependency install below needs root, which it
# gets through sudo.
set -euo pipefail

APP="steam-punk"
# Single source of truth: the version in Cargo.toml
VERSION="$(grep -m1 '^version' Cargo.toml | cut -d'"' -f2)"
ARCH="x86_64"
BUILD_DIR="build-appimage"
APPDIR="$BUILD_DIR/AppDir"

echo "==> Building $APP $VERSION AppImage"

# ── Build dependencies ────────────────────────────────────────────────
if ! command -v cargo >/dev/null 2>&1 || ! pkg-config --exists gtk4 2>/dev/null; then
    echo "==> Installing build dependencies"
    # Tolerate an unrelated third-party repo (e.g. the runner image's preinstalled
    # Google Chrome source) failing to refresh -- apt falls back to its cached index
    # for that repo and still refreshes everything else; only `apt-get install`
    # failing on a package we actually need should be fatal.
    SUDO=()
    if [[ $EUID -ne 0 ]]; then
        SUDO=(sudo)
    fi
    "${SUDO[@]}" apt-get update -qq || true
    "${SUDO[@]}" apt-get install -y -qq cargo rustc libgtk-4-dev libadwaita-1-dev \
        pkg-config zsync wget file desktop-file-utils
fi
# On a machine that already has the toolchain (CI: the workflow installs the packaging
# tools) nothing above runs, so a missing tool is reported here instead of mid-build.
for tool in wget file desktop-file-validate; do
    command -v "$tool" >/dev/null 2>&1 || { echo "==> ERROR: $tool is not installed" >&2; exit 1; }
done

# ── Release build ─────────────────────────────────────────────────────
echo "==> cargo build --release --locked"
cargo build --release --locked

# ── AppDir layout ─────────────────────────────────────────────────────
rm -rf "$BUILD_DIR"
mkdir -p "$APPDIR/usr/bin" \
         "$APPDIR/usr/share/applications" \
         "$APPDIR/usr/share/icons/hicolor/256x256/apps" \
         "$APPDIR/usr/share/metainfo"

# Cargo builds into CARGO_TARGET_DIR when it is set (e.g. a target dir shared by several
# checkouts), else into target/.
cp "${CARGO_TARGET_DIR:-target}/release/$APP"         "$APPDIR/usr/bin/"
cp data/$APP.desktop                                  "$APPDIR/usr/share/applications/"
cp data/$APP-256.png                                    "$APPDIR/usr/share/icons/hicolor/256x256/apps/$APP.png"
# The <releases> list is generated from CHANGELOG.md's version headings, and fails the
# build if the newest one is not this Cargo.toml version.
python3 scripts/sync_appdata_releases.py
cp data/io.github.labj1987.SteamPunk.appdata.xml   "$APPDIR/usr/share/metainfo/"

# Top-level AppImage requirements
cp data/$APP.desktop "$APPDIR/"
cp data/$APP-256.png "$APPDIR/$APP.png"

desktop-file-validate "$APPDIR/$APP.desktop"

# ── AppRun ────────────────────────────────────────────────────────────
# Just starts the app. The privileged setup script and polkit policy are compiled into the
# binary (not shipped as files, which an extracted AppDir would let the user rewrite) and are
# installed to their fixed system paths (polkit refuses relative/user paths) by the app itself,
# in the same pkexec call that runs the setup, and only when the user starts that setup.
cat > "$APPDIR/AppRun" << 'APPRUN'
#!/usr/bin/env bash
HERE="$(dirname "$(readlink -f "$0")")"

# >>> libcheck: before any setup, make sure the app's shared libraries resolve.
# LIBCHECK_LDD, LIBCHECK_OS_RELEASE and LIBCHECK_DIALOG=none exist for the tests.
libcheck_missing() {
    local out
    if [[ -n "${LIBCHECK_LDD:-}" ]]; then
        out="$("$LIBCHECK_LDD" "$1" 2>/dev/null)"
    elif command -v ldd >/dev/null 2>&1; then
        out="$(ldd "$1" 2>/dev/null)"
    else
        out="$(LD_TRACE_LOADED_OBJECTS=1 "$1" 2>/dev/null)"
    fi
    sed -n 's/^[[:space:]]*\([^[:space:]]*\) => not found.*/\1/p' <<<"$out"
}

libcheck_install_hint() {
    local f="${LIBCHECK_OS_RELEASE:-/etc/os-release}" ids id
    [[ -r "$f" ]] || return 0
    # shellcheck disable=SC1090
    ids="$(. "$f" 2>/dev/null; echo "${ID:-} ${ID_LIKE:-}")"
    for id in $ids; do
        case "$id" in
            debian|ubuntu|linuxmint|pop) echo "sudo apt install libgtk-4-1 libadwaita-1-0"; return 0 ;;
            fedora|rhel|centos|rocky|almalinux) echo "sudo dnf install gtk4 libadwaita"; return 0 ;;
            arch|cachyos|manjaro|endeavouros) echo "sudo pacman -S gtk4 libadwaita"; return 0 ;;
            opensuse*|suse|sles) echo "sudo zypper install libgtk-4-1 libadwaita-1-0"; return 0 ;;
        esac
    done
}

libcheck_show() {
    printf '%s: %s\n' "$1" "$2" >&2
    [[ "${LIBCHECK_DIALOG:-}" == none ]] && return 0
    [[ -n "${DISPLAY:-}${WAYLAND_DISPLAY:-}" ]] || return 0
    if command -v zenity >/dev/null 2>&1; then
        zenity --error --title="$1" --text="$2" --width=460 >/dev/null 2>&1
    elif command -v kdialog >/dev/null 2>&1; then
        kdialog --error "$2" --title "$1" >/dev/null 2>&1
    elif command -v notify-send >/dev/null 2>&1; then
        notify-send -u critical "$1" "$2" >/dev/null 2>&1
    fi
    return 0
}

# libcheck_run NAME BINARY: returns 1 (after showing why) when libraries are missing.
libcheck_run() {
    local missing list hint msg
    missing="$(libcheck_missing "$2")"
    [[ -n "$missing" ]] || return 0
    list="$(paste -sd, - <<<"$missing" | sed 's/,/, /g')"
    hint="$(libcheck_install_hint)"
    msg="$1 can't start because these system libraries are missing: $list."
    if [[ -n "$hint" ]]; then
        msg+=" They come with GTK 4 and libadwaita. Install them with: $hint, then start $1 again."
    else
        msg+=" They come with GTK 4 (4.12 or newer) and libadwaita (1.5 or newer): install both with your package manager, then start $1 again."
    fi
    libcheck_show "$1" "$msg"
    return 1
}
# <<< libcheck

libcheck_run "Steam Punk" "$HERE/usr/bin/steam-punk" || exit 1
export PATH="$HERE/usr/bin:$PATH"
exec "$HERE/usr/bin/steam-punk" "$@"
APPRUN
chmod 755 "$APPDIR/AppRun"

# ── appimagetool ──────────────────────────────────────────────────────
# Pinned and checksum-verified. Cached outside $BUILD_DIR (which is wiped above)
# so a second run reuses it.
APPIMAGETOOL_VERSION="1.9.1"
APPIMAGETOOL_SHA256="ed4ce84f0d9caff66f50bcca6ff6f35aae54ce8135408b3fa33abfc3cb384eb0"
TOOL_DIR=".cache"
TOOL="$TOOL_DIR/appimagetool-$APPIMAGETOOL_VERSION"
if [[ ! -f "$TOOL" ]]; then
    mkdir -p "$TOOL_DIR"
    wget -q --no-hsts -O "$TOOL.part" \
        "https://github.com/AppImage/appimagetool/releases/download/$APPIMAGETOOL_VERSION/appimagetool-x86_64.AppImage"
    mv "$TOOL.part" "$TOOL"
fi
if ! echo "$APPIMAGETOOL_SHA256  $TOOL" | sha256sum -c --status -; then
    echo "==> ERROR: appimagetool checksum mismatch" >&2
    rm -f "$TOOL"
    exit 1
fi
chmod +x "$TOOL"

# The runtime appimagetool puts in front of the squashfs. Without --runtime-file it downloads
# the moving `continuous` build at pack time, so it is pinned and checked the same way.
# To bump: pick a release at https://github.com/AppImage/type2-runtime/releases and take the
# sha256 of its runtime-x86_64 asset (download it and run sha256sum).
RUNTIME_VERSION="20251108"
RUNTIME_SHA256="2fca8b443c92510f1483a883f60061ad09b46b978b2631c807cd873a47ec260d"
RUNTIME="$TOOL_DIR/runtime-x86_64-$RUNTIME_VERSION"
if [[ ! -f "$RUNTIME" ]]; then
    mkdir -p "$TOOL_DIR"
    wget -q --no-hsts -O "$RUNTIME.part" \
        "https://github.com/AppImage/type2-runtime/releases/download/$RUNTIME_VERSION/runtime-x86_64"
    mv "$RUNTIME.part" "$RUNTIME"
fi
if ! echo "$RUNTIME_SHA256  $RUNTIME" | sha256sum -c --status -; then
    echo "==> ERROR: type2-runtime checksum mismatch" >&2
    rm -f "$RUNTIME"
    exit 1
fi

echo "==> Packing AppImage"
OUT="$APP-$VERSION-$ARCH.AppImage"

UPDATE_INFORMATION="gh-releases-zsync|labj1987|steam-punk|latest|steam-punk-*-x86_64.AppImage.zsync"
VERSION="$VERSION" ARCH="$ARCH" "$TOOL" --appimage-extract-and-run \
    --runtime-file "$RUNTIME" -u "$UPDATE_INFORMATION" "$APPDIR" "$OUT"

echo "==> Done: $OUT"
ls -lh "$OUT"

# appimagetool's built-in zsync generation silently no-ops on GitHub Actions
# runners even when zsyncmake is installed and working — build the .zsync
# sidecar directly instead. Fatal in CI (CI is set): the AppImage's
# UPDATE_INFORMATION points at a .zsync, so a release without one cannot
# update. A local build only warns.
echo "==> Generating .zsync sidecar"
if ! command -v zsyncmake >/dev/null 2>&1; then
    if [[ -n "${CI:-}" ]]; then
        echo "==> ERROR: zsyncmake not found (install the zsync package)" >&2
        exit 1
    fi
    echo "==> WARNING: zsyncmake not found — continuing without .zsync"
elif zsyncmake "$OUT"; then
    echo "==> .zsync generated: $OUT.zsync"
elif [[ -n "${CI:-}" ]]; then
    echo "==> ERROR: zsyncmake failed" >&2
    exit 1
else
    echo "==> WARNING: zsyncmake failed — continuing without .zsync"
fi
