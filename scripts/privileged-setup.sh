#!/usr/bin/env bash
# privileged-setup.sh — runs as root via pkexec.
# One-time system package setup for the .NET installer: cabextract, which
# winetricks unpacks the Microsoft installers with, and curl, which it
# downloads them with. winetricks itself is not a system package: the app keeps
# a pinned upstream copy per user and runs it with the game's own Proton wine,
# so no system wine is needed (the distribution's winetricks package depends on
# its whole wine). Idempotent — apt no-ops if packages are already present.

set -uo pipefail
# pkexec keeps the caller's umask; nothing root writes here may be
# group- or world-writable.
umask 022
export DEBIAN_FRONTEND=noninteractive
# Wait for another package manager (unattended-upgrades after boot) instead of
# failing at once, and never stop at a configuration-file question: there is
# no terminal to answer it.
APT_OPTS=(-o DPkg::Lock::Timeout=120 -o Dpkg::Options::=--force-confdef -o Dpkg::Options::=--force-confold)

LOGFILE="/var/log/steam-punk.log"
# The log is opened once, without following a symlink someone put in its
# place (/var/log is group-writable on Debian and Ubuntu): a symlink there is
# refused, and so is an opened file that turns out not to be the log itself.
if [[ -L "$LOGFILE" ]] || ! exec 3>>"$LOGFILE" \
    || [[ "$(readlink /proc/self/fd/3)" != "$LOGFILE" ]]; then
    exec 3>/dev/null
    echo "[steam-punk] WARNING: $LOGFILE is not a plain file, not logging to it" >&2
fi
log() {
    local msg="[steam-punk] $*"
    echo "$msg"
    echo "$(date '+%Y-%m-%d %H:%M:%S') $msg" >&3 2>/dev/null || true
}
die() { log "ERROR: $*"; exit 1; }

log "==== .NET installer prerequisite setup started ===="
# A failed refresh (typically one broken third-party repository) doesn't stop
# the install: apt still has the package lists from the last good update, and
# cabextract and curl are in the distribution's own main repositories. Only
# the install failing is fatal, and then the update failure is named too.
update_failed=0
if ! apt-get "${APT_OPTS[@]}" update >&3 2>&1; then
    update_failed=1
    log "WARNING: apt-get update failed (details in $LOGFILE); installing from the existing package lists"
fi
if ! apt-get "${APT_OPTS[@]}" install -y --no-install-recommends cabextract curl >&3 2>&1; then
    if [[ $update_failed -eq 1 ]]; then
        die "apt-get install failed, after apt-get update had also failed (details in $LOGFILE)"
    fi
    die "apt-get install failed (details in $LOGFILE)"
fi
log "==== Done. System packages ready. ===="
exit 0
