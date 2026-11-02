#!/usr/bin/env bash
# INTERIM: delete the interim/ directory once the upstream launcher override ships.
#
# Tests apply-launcher-override.sh against a fixture shaped like OpenShell's entry
# point. Needs only bash, grep, awk and cmp.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
patch="$here/apply-launcher-override.sh"
fixture="$here/fixtures/main.rs"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

failures=0
pass() { printf 'ok    %s\n' "$1"; }
fail() { printf 'FAIL  %s\n' "$1" >&2; failures=$((failures + 1)); }
check() { # check <description> <command...>
    local what="$1"; shift
    if "$@" >/dev/null 2>&1; then pass "$what"; else fail "$what"; fi
}
refuses() { # refuses <description> <command...>
    local what="$1"; shift
    if "$@" >/dev/null 2>&1; then fail "$what"; else pass "$what"; fi
}

# 1. The two edits land.
cp "$fixture" "$work/main.rs"
check "applies to a fresh file" "$patch" "$work/main.rs"
check "adds the environment variable" grep -q 'env = "OPENSHELL_VM_LAUNCHER_BIN"' "$work/main.rs"
check "adds the argument inside Args" grep -q '^    launcher_bin: Option<PathBuf>,$' "$work/main.rs"
check "passes it to the config" grep -q '^        launcher_bin: args.launcher_bin.clone(),$' "$work/main.rs"
refuses "no longer hardcodes the launcher to None" grep -q 'launcher_bin: None' "$work/main.rs"

# 2. Idempotent: a second run changes nothing.
cp "$work/main.rs" "$work/after-first.rs"
check "second run succeeds" "$patch" "$work/main.rs"
check "second run changes nothing" cmp -s "$work/main.rs" "$work/after-first.rs"

# 3. The rest of the file is untouched.
check "other lines are preserved" grep -q '^    state_dir: PathBuf,$' "$work/main.rs"

# 4. Refuses to guess when an anchor is missing or repeated.
grep -v 'launcher_bin: None' "$fixture" > "$work/no-none.rs"
refuses "fails without the 'launcher_bin: None' anchor" "$patch" "$work/no-none.rs"
check "leaves that file alone" cmp -s "$work/no-none.rs" <(grep -v 'launcher_bin: None' "$fixture")

grep -v '^struct Args {$' "$fixture" > "$work/no-struct.rs"
refuses "fails without the 'struct Args' anchor" "$patch" "$work/no-struct.rs"

{ cat "$fixture"; printf '\nstruct Args {\n}\n'; } > "$work/two-structs.rs"
refuses "fails when 'struct Args' appears twice" "$patch" "$work/two-structs.rs"

# 5. Argument handling.
refuses "fails with no argument" "$patch"
refuses "fails on a missing file" "$patch" "$work/absent.rs"

if [ "$failures" -ne 0 ]; then
    printf '%s check(s) failed\n' "$failures" >&2
    exit 1
fi
echo "all checks passed"
