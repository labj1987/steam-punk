#!/usr/bin/env bash
# install-privileged.sh — runs as root via pkexec, only when the user starts
# the automatic setup and the system copy of privileged-setup.sh or its polkit
# policy is missing or differs from the one compiled into the app. polkit only
# accepts both at fixed system paths, so they are installed there, then the
# setup script runs — one password prompt in total.
#
# Arguments, for the script and then the policy: the user-staged copy the app
# wrote from its compiled-in bytes, the system destination, and the SHA-256
# pinned when the app was built (6 arguments in all).
#
# Root first copies each staged file into its own root-owned temp file and
# verifies that copy against the pinned checksum, so nothing the user can
# rewrite after the check is ever installed (no TOCTOU window).

set -uo pipefail

install_verified() {
    local src="$1" dst="$2" mode="$3" sum="$4" tmp
    tmp="$(mktemp)" || return 1
    cat "$src" > "$tmp" || { rm -f "$tmp"; return 1; }
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

if [[ $# -ne 6 ]]; then
    echo "steam-punk: install-privileged.sh needs 6 arguments, got $#" >&2
    exit 2
fi
install_verified "$1" "$2" 755 "$3" || exit 1
install_verified "$4" "$5" 644 "$6" || exit 1
exec "$2"
