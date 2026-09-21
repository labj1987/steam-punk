#!/usr/bin/env bash
# privileged-setup.sh — runs as root via pkexec.
# One-time system package setup for wine .NET installs. Idempotent —
# apt no-ops if packages are already present, safe to invoke every launch.

set -uo pipefail

LOGFILE="/var/log/steam-punk.log"
log() {
    local msg="[steam-punk] $*"
    echo "$msg"
    echo "$(date '+%Y-%m-%d %H:%M:%S') $msg" >> "$LOGFILE" 2>/dev/null || true
}
die() { log "ERROR: $*"; exit 1; }

log "==== Wine .NET prerequisite setup started ===="
dpkg --add-architecture i386 || die "dpkg --add-architecture i386 failed"
apt-get update >>"$LOGFILE" 2>&1 || die "apt-get update failed"
apt-get install -y winetricks cabextract wine32:i386 >>"$LOGFILE" 2>&1 \
    || die "apt-get install failed"
log "==== Done. System packages ready. ===="
exit 0
