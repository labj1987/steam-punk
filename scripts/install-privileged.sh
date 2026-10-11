#!/usr/bin/env bash
# install-privileged.sh — runs as root via pkexec, only when the user starts
# the automatic setup and the system copy of privileged-setup.sh or its polkit
# policy is missing or differs from the one this AppImage ships. polkit only
# accepts both at fixed system paths, so they are installed there, then the
# setup script runs — one password prompt in total.
#
# Arguments: for the script, then the policy: the file in the mounted AppImage,
# a user-staged copy of it, the system destination, and the SHA-256 the app
# computed from the AppImage's read-only contents (8 arguments in all).
#
# Root reads each file straight from the mounted AppImage when it can (FUSE
# mounts often deny root), else from the staged copy. Either way it first
# copies into its own root-owned temp file and verifies that copy against the
# checksum, so nothing the user can rewrite after the check is ever installed
# (no TOCTOU window).

set -uo pipefail

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

if [[ $# -ne 8 ]]; then
    echo "steam-punk: install-privileged.sh needs 8 arguments, got $#" >&2
    exit 2
fi
install_verified "$1" "$2" "$3" 755 "$4" || exit 1
install_verified "$5" "$6" "$7" 644 "$8" || exit 1
exec "$3"
