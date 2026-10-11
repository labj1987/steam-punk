#!/usr/bin/env bash
# privileged-setup.sh — runs as root via pkexec.
# One-time system package setup for the .NET installer: winetricks and the
# cabextract it unpacks the Microsoft installers with. winetricks runs with the
# game's own Proton wine, so no system wine packages are needed. Idempotent —
# apt no-ops if packages are already present.

set -uo pipefail

LOGFILE="/var/log/steam-punk.log"
log() {
    local msg="[steam-punk] $*"
    echo "$msg"
    echo "$(date '+%Y-%m-%d %H:%M:%S') $msg" >> "$LOGFILE" 2>/dev/null || true
}
die() { log "ERROR: $*"; exit 1; }

log "==== .NET installer prerequisite setup started ===="
# A failed refresh (typically one broken third-party repository) doesn't stop
# the install: apt still has the package lists from the last good update, and
# winetricks and cabextract are in the distribution's own repositories. Only
# the install failing is fatal, and then the update failure is named too.
update_failed=0
if ! apt-get update >>"$LOGFILE" 2>&1; then
    update_failed=1
    log "WARNING: apt-get update failed (details in $LOGFILE); installing from the existing package lists"
fi
if ! apt-get install -y --no-install-recommends winetricks cabextract >>"$LOGFILE" 2>&1; then
    if [[ $update_failed -eq 1 ]]; then
        die "apt-get install failed, after apt-get update had also failed (details in $LOGFILE)"
    fi
    die "apt-get install failed (details in $LOGFILE)"
fi
log "==== Done. System packages ready. ===="
exit 0
