#!/usr/bin/env bash
# build-appimage.sh — build the Steam Punk AppImage.
# Run from the repo root on Ubuntu (CI uses ubuntu-latest). Run as root in CI.
set -euo pipefail

APP="steam-punk"
# Single source of truth: the version in Cargo.toml
VERSION="$(grep -m1 '^version' Cargo.toml | cut -d'"' -f2)"
ARCH="x86_64"
BUILD_DIR="build-appimage"
APPDIR="$BUILD_DIR/AppDir"

echo "==> Building $APP $VERSION AppImage"

# ── Build dependencies ────────────────────────────────────────────────
# Tolerate an unrelated third-party repo (e.g. the runner image's preinstalled
# Google Chrome source) failing to refresh -- apt falls back to its cached index
# for that repo and still refreshes everything else; only `apt-get install`
# failing on a package we actually need should be fatal.
apt-get update -qq || true

# The tools this script itself uses are installed unconditionally (after the
# index refresh above): on CI a prior workflow step already installs cargo, so
# a `command -v cargo` guard around this evaluates false and anything gated
# behind it gets silently skipped.
apt-get install -y -qq zsync wget file desktop-file-utils

if ! command -v cargo >/dev/null 2>&1 || ! pkg-config --exists gtk4 2>/dev/null; then
    echo "==> Installing build dependencies"
    apt-get install -y -qq cargo rustc libgtk-4-dev libadwaita-1-dev \
        pkg-config
fi

# ── Release build ─────────────────────────────────────────────────────
echo "==> cargo build --release --locked"
cargo build --release --locked

# ── AppDir layout ─────────────────────────────────────────────────────
rm -rf "$BUILD_DIR"
mkdir -p "$APPDIR/usr/bin" \
         "$APPDIR/usr/lib/$APP" \
         "$APPDIR/usr/share/applications" \
         "$APPDIR/usr/share/icons/hicolor/256x256/apps" \
         "$APPDIR/usr/share/polkit-1/actions" \
         "$APPDIR/usr/share/metainfo"

cp "target/release/$APP"                              "$APPDIR/usr/bin/"
cp scripts/privileged-setup.sh                        "$APPDIR/usr/lib/$APP/"
chmod 755 "$APPDIR/usr/lib/$APP/privileged-setup.sh"
cp data/$APP.desktop                                  "$APPDIR/usr/share/applications/"
cp data/$APP-256.png                                    "$APPDIR/usr/share/icons/hicolor/256x256/apps/$APP.png"
cp data/io.github.labj1987.SteamPunk.setup.policy  "$APPDIR/usr/share/polkit-1/actions/"
# The <releases> list is generated from CHANGELOG.md's version headings, and fails the
# build if the newest one is not this Cargo.toml version.
python3 scripts/sync_appdata_releases.py
cp data/io.github.labj1987.SteamPunk.appdata.xml   "$APPDIR/usr/share/metainfo/"

# Top-level AppImage requirements
cp data/$APP.desktop "$APPDIR/"
cp data/$APP-256.png "$APPDIR/$APP.png"

desktop-file-validate "$APPDIR/$APP.desktop"

# ── AppRun ────────────────────────────────────────────────────────────
# On first launch (or after an update) the privileged script and polkit
# policy must exist at fixed system paths — polkit refuses relative/user
# paths — so AppRun installs them via pkexec when missing or outdated, then
# execs the app.
cat > "$APPDIR/AppRun" << 'APPRUN'
#!/usr/bin/env bash
HERE="$(dirname "$(readlink -f "$0")")"
APP="steam-punk"

SRC_SCRIPT="$HERE/usr/lib/$APP/privileged-setup.sh"
SRC_POLICY="$HERE/usr/share/polkit-1/actions/io.github.labj1987.SteamPunk.setup.policy"
DST_SCRIPT="/usr/lib/$APP/privileged-setup.sh"
DST_POLICY="/usr/share/polkit-1/actions/io.github.labj1987.SteamPunk.setup.policy"

needs_install=0
if [[ ! -f "$DST_SCRIPT" ]] || ! cmp -s "$SRC_SCRIPT" "$DST_SCRIPT"; then
    needs_install=1
fi
if [[ ! -f "$DST_POLICY" ]] || ! cmp -s "$SRC_POLICY" "$DST_POLICY"; then
    needs_install=1
fi

if [[ $needs_install -eq 1 ]]; then
    # Runs as root. Root reads the file straight from the mounted AppImage
    # when it can (FUSE mounts often deny root), else from a user-staged
    # copy. Either way it first copies into its own root-owned temp file and
    # verifies that copy against the SHA-256 computed from the AppImage's
    # read-only contents, so nothing the user can rewrite after the check is
    # ever installed (no TOCTOU window).
    install_verified() {
        local src="$1" alt="$2" dst="$3" mode="$4" sum="$5" tmp
        tmp="$(mktemp)" || return 1
        if ! cat "$src" > "$tmp" 2>/dev/null; then
            cat "$alt" > "$tmp" || { rm -f "$tmp"; return 1; }
        fi
        if [[ "$(sha256sum "$tmp" | cut -d' ' -f1)" != "$sum" ]]; then
            echo "steam-punk: checksum mismatch for $dst, refusing to install" >&2
            rm -f "$tmp"
            return 1
        fi
        install -D -m "$mode" "$tmp" "$dst"
        local rc=$?
        rm -f "$tmp"
        return $rc
    }

    SUM_SCRIPT="$(sha256sum "$SRC_SCRIPT" | cut -d' ' -f1)"
    SUM_POLICY="$(sha256sum "$SRC_POLICY" | cut -d' ' -f1)"
    STAGE="$(mktemp -d)"
    cp "$SRC_SCRIPT" "$STAGE/privileged-setup.sh"
    cp "$SRC_POLICY" "$STAGE/policy"
    pkexec bash -c "$(declare -f install_verified)"'
        install_verified "$1" "$2" "$3" 755 "$4" && install_verified "$5" "$6" "$7" 644 "$8"' \
        bash "$SRC_SCRIPT" "$STAGE/privileged-setup.sh" "$DST_SCRIPT" "$SUM_SCRIPT" \
        "$SRC_POLICY" "$STAGE/policy" "$DST_POLICY" "$SUM_POLICY"
    rm -rf "$STAGE"
fi

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
    wget -q -O "$TOOL.part" \
        "https://github.com/AppImage/appimagetool/releases/download/$APPIMAGETOOL_VERSION/appimagetool-x86_64.AppImage"
    mv "$TOOL.part" "$TOOL"
fi
if ! echo "$APPIMAGETOOL_SHA256  $TOOL" | sha256sum -c --status -; then
    echo "==> ERROR: appimagetool checksum mismatch" >&2
    rm -f "$TOOL"
    exit 1
fi
chmod +x "$TOOL"

echo "==> Packing AppImage"
OUT="$APP-$VERSION-$ARCH.AppImage"

UPDATE_INFORMATION="gh-releases-zsync|labj1987|steam-punk|latest|steam-punk-*-x86_64.AppImage.zsync"
VERSION="$VERSION" ARCH="$ARCH" "$TOOL" --appimage-extract-and-run \
    -u "$UPDATE_INFORMATION" "$APPDIR" "$OUT"

echo "==> Done: $OUT"
ls -lh "$OUT"

# appimagetool's built-in zsync generation silently no-ops on GitHub Actions
# runners even when zsyncmake is installed and working — build the .zsync
# sidecar directly instead. Non-fatal:
# the AppImage itself is already valid without it.
echo "==> Generating .zsync sidecar"
if zsyncmake "$OUT"; then
    echo "==> .zsync generated: $OUT.zsync"
else
    echo "==> WARNING: zsyncmake failed — continuing without .zsync"
fi
