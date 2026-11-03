#!/usr/bin/env bash
# INTERIM: delete with the rest of interim/ once OpenShell's init script does this itself.
#
# OpenShell's guest init script (crates/openshell-driver-vm/scripts/
# openshell-vm-sandbox-init.sh) uses bash process substitution (`done < <(...)`),
# which needs /dev/fd. With libkrun, libkrun's own init creates /dev/fd and
# /dev/stdin, /dev/stdout and /dev/stderr before the script runs. Firecracker boots
# the script directly as PID 1, so nothing does, and the script dies with
# "/dev/fd/63: No such file or directory" and the guest panics.
#
# This adds the links to the script's mount_initial_fs, right after /dev is mounted:
#
#   interim/apply-guest-init-fix.sh <openshell>/crates/openshell-driver-vm/scripts/openshell-vm-sandbox-init.sh
#
# Found by anchor (one whole line); refuses to guess if it is missing or repeated, and
# does nothing if the script already creates /dev/fd.
set -euo pipefail

die() { printf 'apply-guest-init-fix: %s\n' "$*" >&2; exit 1; }

file="${1:-}"
[ -n "$file" ] || die "usage: apply-guest-init-fix.sh <openshell>/crates/openshell-driver-vm/scripts/openshell-vm-sandbox-init.sh"
[ -f "$file" ] || die "not a file: $file"

if grep -q '/dev/fd' "$file"; then
    echo "/dev/fd is already set up in $file; nothing to do"
    exit 0
fi

anchor='    mount -t devtmpfs devtmpfs /dev 2>/dev/null || true'
n="$(grep -cxF -- "$anchor" "$file" || true)"
[ "$n" = 1 ] || die "expected exactly one '$anchor' in $file, found $n"

tmp="$(mktemp)"
trap 'rm -f "$tmp"' EXIT

awk -v anchor="$anchor" '
    $0 == anchor {
        print
        print "    # Firecracker boots this script as PID 1, so nothing else creates these"
        print "    # (libkrun'"'"'s init does). Process substitution needs /dev/fd."
        print "    ln -sfn /proc/self/fd /dev/fd 2>/dev/null || true"
        print "    ln -sf /proc/self/fd/0 /dev/stdin 2>/dev/null || true"
        print "    ln -sf /proc/self/fd/1 /dev/stdout 2>/dev/null || true"
        print "    ln -sf /proc/self/fd/2 /dev/stderr 2>/dev/null || true"
        next
    }
    { print }
' "$file" > "$tmp"

grep -q 'ln -sfn /proc/self/fd /dev/fd' "$tmp" || die "internal error: links not added"
bash -n "$tmp" || die "internal error: patched script does not parse"

cat "$tmp" > "$file"
echo "added /dev/fd links to $file"
