#!/usr/bin/env bash
# build-appimage.sh — build the SteamPunk AppImage.
# Run from the repo root on Ubuntu (CI uses ubuntu-latest). Run as root in CI.
set -euo pipefail

APP="steampunk"
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

# zsync is installed unconditionally (after the index refresh above): on CI a
# prior workflow step already installs cargo, so a `command -v cargo` guard
# around this evaluates false and anything gated behind it — zsync included —
# gets silently skipped.
apt-get install -y -qq zsync

if ! command -v cargo >/dev/null 2>&1 || ! pkg-config --exists gtk4 2>/dev/null; then
    echo "==> Installing build dependencies"
    apt-get install -y -qq cargo rustc libgtk-4-dev libadwaita-1-dev \
        pkg-config libssl-dev wget file desktop-file-utils zsync
fi

# ── Release build ─────────────────────────────────────────────────────
echo "==> cargo build --release"
cargo build --release

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
cp data/io.github.labj1987.SteamPunk.appdata.xml   "$APPDIR/usr/share/metainfo/"

# Make sure the appdata <releases> list starts with the version being built
# (taken from Cargo.toml), dated from its CHANGELOG heading when there is one.
APPDATA="$APPDIR/usr/share/metainfo/io.github.labj1987.SteamPunk.appdata.xml"
if ! grep -q "<release version=\"$VERSION\"" "$APPDATA"; then
    REL_DATE="$(grep -m1 "^## $VERSION " CHANGELOG.md | grep -o '[0-9]\{4\}-[0-9]\{2\}-[0-9]\{2\}' || true)"
    REL_DATE="${REL_DATE:-$(date -u +%F)}"
    sed -i "s|<releases>|<releases>\n    <release version=\"$VERSION\" date=\"$REL_DATE\"/>|" "$APPDATA"
fi

# Top-level AppImage requirements
cp data/$APP.desktop "$APPDIR/"
cp data/$APP-256.png "$APPDIR/$APP.png"

# ── AppRun ────────────────────────────────────────────────────────────
# On first launch (or after an update) the privileged script and polkit
# policy must exist at fixed system paths — polkit refuses relative/user
# paths — so AppRun installs them via pkexec when missing or outdated, then
# execs the app. Same pattern as KernelPop's AppRun.
cat > "$APPDIR/AppRun" << 'APPRUN'
#!/usr/bin/env bash
HERE="$(dirname "$(readlink -f "$0")")"
APP="steampunk"

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
            echo "steampunk: checksum mismatch for $dst, refusing to install" >&2
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
exec "$HERE/usr/bin/steampunk" "$@"
APPRUN
chmod 755 "$APPDIR/AppRun"

# ── appimagetool ──────────────────────────────────────────────────────
# Cached outside $BUILD_DIR (which is wiped above) so the cache check is live.
# Set APPIMAGETOOL_URL to a fixed release asset and APPIMAGETOOL_SHA256 to its
# checksum to pin it; the download is verified when a checksum is given, and
# the computed hash is always printed so it can be pinned.
TOOL_URL="${APPIMAGETOOL_URL:-https://github.com/AppImage/appimagetool/releases/download/continuous/appimagetool-x86_64.AppImage}"
TOOL_SHA256="${APPIMAGETOOL_SHA256:-}"
TOOL_CACHE=".cache"
TOOL="$TOOL_CACHE/appimagetool"
mkdir -p "$TOOL_CACHE"
if [[ -f "$TOOL" && -n "$TOOL_SHA256" ]] && \
   [[ "$(sha256sum "$TOOL" | cut -d' ' -f1)" != "$TOOL_SHA256" ]]; then
    echo "==> Cached appimagetool does not match the pinned checksum, re-downloading"
    rm -f "$TOOL"
fi
if [[ ! -f "$TOOL" ]]; then
    echo "==> Downloading appimagetool"
    wget -q -O "$TOOL.part" "$TOOL_URL"
    ACTUAL="$(sha256sum "$TOOL.part" | cut -d' ' -f1)"
    echo "==> appimagetool sha256: $ACTUAL"
    if [[ -n "$TOOL_SHA256" && "$ACTUAL" != "$TOOL_SHA256" ]]; then
        rm -f "$TOOL.part"
        echo "ERROR: appimagetool checksum mismatch (expected $TOOL_SHA256)" >&2
        exit 1
    fi
    mv "$TOOL.part" "$TOOL"
    chmod +x "$TOOL"
fi

echo "==> Packing AppImage"
OUT="$APP-$VERSION-$ARCH.AppImage"

UPDATE_INFORMATION="gh-releases-zsync|labj1987|SteamPunk|latest|steampunk-*-x86_64.AppImage.zsync"
VERSION="$VERSION" ARCH="$ARCH" "$TOOL" --appimage-extract-and-run \
    -u "$UPDATE_INFORMATION" "$APPDIR" "$OUT"

echo "==> Done: $OUT"
ls -lh "$OUT"

# appimagetool's built-in zsync generation silently no-ops on GitHub Actions
# runners even when zsyncmake is installed and working (see KernelPop's CLAUDE.md
# for the diagnosis) — build the .zsync sidecar directly instead. Non-fatal:
# the AppImage itself is already valid without it.
echo "==> Generating .zsync sidecar"
if zsyncmake "$OUT"; then
    echo "==> .zsync generated: $OUT.zsync"
else
    echo "==> WARNING: zsyncmake failed — continuing without .zsync"
fi
