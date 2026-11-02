#!/usr/bin/env bash
# INTERIM: delete the interim/ directory once the upstream launcher override ships.
#
# Add an `OPENSHELL_VM_LAUNCHER_BIN` option to OpenShell's VM driver entry point,
# in an OpenShell checkout:
#
#   interim/apply-launcher-override.sh <openshell>/crates/openshell-driver-vm/src/main.rs
#
# The driver already has a `launcher_bin` setting; its `main` just always leaves it
# unset (`launcher_bin: None`). This edits two places, found by anchor, so it works on
# whichever release is checked out rather than on a copy of one release's file:
#
#   1. a `launcher_bin` argument is added at the top of `struct Args`;
#   2. `launcher_bin: None,` becomes `launcher_bin: args.launcher_bin.clone(),`.
#
# It refuses to guess: if either anchor is missing or appears more than once, it
# changes nothing and fails. It does nothing if the option is already there, so it is
# safe to run on a release that has shipped the upstream change.
set -euo pipefail

die() { printf 'apply-launcher-override: %s\n' "$*" >&2; exit 1; }

file="${1:-}"
[ -n "$file" ] || die "usage: apply-launcher-override.sh <openshell>/crates/openshell-driver-vm/src/main.rs"
[ -f "$file" ] || die "not a file: $file"

if grep -q 'OPENSHELL_VM_LAUNCHER_BIN' "$file"; then
    echo "launcher override already present in $file; nothing to do"
    exit 0
fi

struct_anchor='^struct Args \{$'
none_anchor='^        launcher_bin: None,$'

n="$(grep -c -- "$struct_anchor" "$file" || true)"
[ "$n" = 1 ] || die "expected exactly one 'struct Args {' in $file, found $n"
n="$(grep -c -- "$none_anchor" "$file" || true)"
[ "$n" = 1 ] || die "expected exactly one '        launcher_bin: None,' in $file, found $n"

tmp="$(mktemp)"
trap 'rm -f "$tmp"' EXIT

awk '
    /^struct Args \{$/ {
        print
        print "    /// VM launcher executable the driver runs for every VM. Defaults to this"
        print "    /// binary. The launcher receives the driver'"'"'s `--internal-run-vm` flags."
        print "    #[arg(long, env = \"OPENSHELL_VM_LAUNCHER_BIN\")]"
        print "    launcher_bin: Option<PathBuf>,"
        print ""
        next
    }
    /^        launcher_bin: None,$/ {
        print "        launcher_bin: args.launcher_bin.clone(),"
        next
    }
    { print }
' "$file" > "$tmp"

# Both edits must have landed.
grep -q 'launcher_bin: Option<PathBuf>,' "$tmp" || die "internal error: argument not added"
grep -q 'launcher_bin: args.launcher_bin.clone(),' "$tmp" || die "internal error: config not updated"

cat "$tmp" > "$file"
echo "added OPENSHELL_VM_LAUNCHER_BIN to $file"
