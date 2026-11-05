#!/usr/bin/env bash
# Move the pinned OpenShell revision to a release tag (or another commit), so a new
# OpenShell version can be tried in place.
#
# The pin lives in three places, and this edits all of them:
#   driver/UPSTREAM.env               OPENSHELL_COMMIT (and OPENSHELL_TAG, informational)
#   vendor/openshell/Cargo.toml       the `rev` of every OpenShell git dependency
#   vendor/openshell/Cargo.lock       re-locked for the new revision (third-party crates are
#                                     left at their locked versions)
#
# It then type-checks the driver against the new revision and summarises what changed
# upstream in the driver crate, because the vendored copy (vendor/openshell/crates/
# openshell-driver-vm) is a fork of the old revision and may need re-syncing. Changes are
# left in the working tree for you to inspect; `git checkout` the three files to undo.
#
# Usage: driver/update-pin.sh [options] [REF]
#
#   REF   a release tag such as v0.1.7; `latest` (newest stable release, the default);
#         `latest-pre` (newest including -pre.N tags); `main`; or a 40-character commit.
#
#   --list       list release tags and mark the current pin; change nothing
#   --check      exit 0 if the pin is at the newest stable release, 10 if a newer one
#                exists (prints which); change nothing
#   --dry-run    show what would change; change nothing
#   --no-build   do not type-check the driver after updating
#   --no-diff    do not fetch and summarise the upstream driver changes
#   -h, --help   this help
#
# Needs: bash, git, sed, sort, awk, network access to the OpenShell repository, and cargo
# (unless --no-build, in which case Cargo.lock is not refreshed either).
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
root="$(dirname "$here")"
env_file="$here/UPSTREAM.env"
cargo_toml="$root/vendor/openshell/Cargo.toml"

die() { printf 'update-pin: %s\n' "$*" >&2; exit 1; }
say() { printf '%s\n' "$*"; }

MODE=update
DRY=0
BUILD=1
DIFF=1
REF=""
while [ $# -gt 0 ]; do
    case "$1" in
        --list) MODE=list ;;
        --check) MODE=check ;;
        --dry-run) DRY=1 ;;
        --no-build) BUILD=0 ;;
        --no-diff) DIFF=0 ;;
        -h | --help) sed -n '2,/^set -euo/p' "${BASH_SOURCE[0]}" | sed '$d' | sed 's/^# \{0,1\}//'; exit 0 ;;
        -*) die "unknown option: $1 (try --help)" ;;
        *) [ -z "$REF" ] || die "one REF at most"; REF="$1" ;;
    esac
    shift
done
REF="${REF:-latest}"

# shellcheck source=driver/UPSTREAM.env
. "$env_file"
[ -n "${OPENSHELL_REPO:-}" ] && [ -n "${OPENSHELL_COMMIT:-}" ] || die "$env_file lacks OPENSHELL_REPO or OPENSHELL_COMMIT"
OLD="$OPENSHELL_COMMIT"

# --- Tags ----------------------------------------------------------------------------
# name -> commit. An annotated tag lists the tag object and, peeled, its commit (`^{}`);
# the commit is what we want.
declare -A TAGS=()
while read -r sha ref; do
    name="${ref#refs/tags/}"
    case "$name" in
        *'^{}') TAGS["${name%'^{}'}"]="$sha" ;;
        *) [ -n "${TAGS[$name]:-}" ] || TAGS["$name"]="$sha" ;;
    esac
done < <(git ls-remote --tags "$OPENSHELL_REPO") || die "cannot list tags of $OPENSHELL_REPO"
[ "${#TAGS[@]}" -gt 0 ] || die "no tags found in $OPENSHELL_REPO"

# Release tags only (not e.g. `vm-runtime`), oldest first. `-` becomes `~` while sorting
# so that v0.1.7-pre.2 sorts before v0.1.7, as a version would.
sorted_tags() { # $1: regex
    local name
    for name in "${!TAGS[@]}"; do printf '%s\n' "$name"; done \
        | grep -E "$1" | sed 's/-/~/' | sort -V | sed 's/~/-/'
}
STABLE='^v[0-9]+\.[0-9]+\.[0-9]+$'
ANY='^v[0-9]+\.[0-9]+\.[0-9]+(-pre\.[0-9]+)?$'
latest_stable="$(sorted_tags "$STABLE" | tail -n 1)"
latest_any="$(sorted_tags "$ANY" | tail -n 1)"
[ -n "$latest_stable" ] || die "no stable release tag found"

current_tag=""
for name in $(sorted_tags "$ANY"); do
    [ "${TAGS[$name]}" = "$OLD" ] && current_tag="$name"
done
current_desc="${current_tag:-no release tag}"

case "$MODE" in
    list)
        say "pin: ${OLD:0:10} ($current_desc)"
        say
        while read -r name; do
            mark=" "; [ "${TAGS[$name]}" = "$OLD" ] && mark="*"
            printf '%s %-16s %s\n' "$mark" "$name" "${TAGS[$name]:0:10}"
        done < <(sorted_tags "$ANY" | tail -n 25)
        say
        say "latest stable: $latest_stable   newest incl. pre-releases: $latest_any"
        exit 0
        ;;
    check)
        say "pin: ${OLD:0:10} ($current_desc); latest stable release: $latest_stable"
        if [ "${TAGS[$latest_stable]}" = "$OLD" ]; then say "up to date"; exit 0; fi
        newer="$(printf '%s\n%s\n' "$current_tag" "$latest_stable" | sed '/^$/d' | sed 's/-/~/' | sort -V | tail -n 1 | sed 's/~/-/')"
        if [ -n "$current_tag" ] && [ "$newer" != "$latest_stable" ]; then
            say "the pin is newer than the latest stable release"; exit 0
        fi
        say "a newer release exists: driver/update-pin.sh $latest_stable"
        exit 10
        ;;
esac

# --- Resolve REF ---------------------------------------------------------------------
TAG=""
case "$REF" in
    latest) TAG="$latest_stable" ;;
    latest-pre) TAG="$latest_any" ;;
    main) NEW="$(git ls-remote "$OPENSHELL_REPO" refs/heads/main | awk '{print $1}')"; [ -n "$NEW" ] || die "cannot resolve main" ;;
    *)
        if [[ "$REF" =~ ^[0-9a-f]{40}$ ]]; then
            NEW="$REF"
        elif [ -n "${TAGS[$REF]:-}" ] && [[ "$REF" =~ $ANY ]]; then
            TAG="$REF"
        else
            die "unknown REF '$REF': not a release tag, 'latest', 'latest-pre', 'main' or a 40-character commit (see --list)"
        fi
        ;;
esac
if [ -n "$TAG" ]; then NEW="${TAGS[$TAG]}"; fi
[ -n "${NEW:-}" ] || die "could not resolve '$REF'"
label="${TAG:-${NEW:0:10}}"

say "pin:    ${OLD:0:10} ($current_desc)"
say "target: ${NEW:0:10} ($label)"
if [ "$NEW" = "$OLD" ]; then
    say "already at that revision; nothing to change"
    exit 0
fi

# --- Edit ----------------------------------------------------------------------------
# Every OpenShell git dependency must be at the current pin before it is moved: a
# mismatch means the pin and Cargo.toml had already drifted (driver:check-pin).
grep -q "rev = \"$OLD\"" "$cargo_toml" || die "$cargo_toml has no rev = \"$OLD\"; run: mise run driver:check-pin"
others="$(grep -o 'rev = "[0-9a-f]*"' "$cargo_toml" | grep -v "\"$OLD\"" || true)"
[ -z "$others" ] || die "$cargo_toml has revs other than the pin: $others"

if [ "$DRY" -eq 1 ]; then
    say "would set OPENSHELL_COMMIT=$NEW and OPENSHELL_TAG=$TAG in driver/UPSTREAM.env"
    say "would replace $(grep -c "rev = \"$OLD\"" "$cargo_toml") rev(s) in vendor/openshell/Cargo.toml"
    [ "$BUILD" -eq 0 ] || say "would re-lock and type-check the driver"
    exit 0
fi

sed -i "s/^OPENSHELL_COMMIT=.*/OPENSHELL_COMMIT=$NEW/" "$env_file"
if grep -q '^OPENSHELL_TAG=' "$env_file"; then
    sed -i "s/^OPENSHELL_TAG=.*/OPENSHELL_TAG=$TAG/" "$env_file"
else
    sed -i "/^OPENSHELL_COMMIT=/a OPENSHELL_TAG=$TAG" "$env_file"
fi
sed -i "s/rev = \"$OLD\"/rev = \"$NEW\"/g" "$cargo_toml"
say "updated driver/UPSTREAM.env and vendor/openshell/Cargo.toml"

# --- What changed upstream in the driver ---------------------------------------------
if [ "$DIFF" -eq 1 ]; then
    tmp="$(mktemp -d)"
    trap 'rm -rf "$tmp"' EXIT
    say
    say "==> Upstream changes to the VM driver and its protocols, ${OLD:0:10}..${NEW:0:10}"
    if git -C "$tmp" init -q \
        && git -C "$tmp" remote add origin "$OPENSHELL_REPO" \
        && git -C "$tmp" fetch -q --depth 1 origin "$OLD" "$NEW" 2>/dev/null; then
        paths=(crates/openshell-driver-vm proto/compute_driver.proto proto/sandbox_protocol.proto
            proto/isolation_backend.proto proto/supervisor_host.proto proto/sandbox.proto
            proto/datamodel.proto proto/extension.proto)
        stat="$(git -C "$tmp" diff --stat "$OLD" "$NEW" -- "${paths[@]}" || true)"
        if [ -n "$stat" ]; then
            printf '%s\n' "$stat"
            say
            say "The vendored crate is a fork of the old revision. To see the changes to re-apply:"
            say "  git -C <openshell-checkout> diff ${OLD:0:10} ${NEW:0:10} -- crates/openshell-driver-vm"
        else
            say "(none: the driver crate and its protocols are identical)"
        fi
    else
        say "(could not fetch both revisions to compare them; skipped)"
    fi
fi

# --- Re-lock and type-check ----------------------------------------------------------
status=0
if [ "$BUILD" -eq 1 ]; then
    command -v cargo >/dev/null 2>&1 || die "cargo not found; use --no-build to skip (Cargo.lock is then not refreshed)"
    say
    say "==> Re-locking and type-checking the driver against ${label}"
    if (cd "$root/vendor/openshell" && cargo check -p openshell-driver-vm --all-targets); then
        say "type-check: ok"
    else
        status=1
        say "type-check: FAILED. The vendored driver does not build against ${label}; it needs" >&2
        say "re-syncing with upstream (see the changes above)." >&2
    fi
fi

say
say "Next: mise run driver:ci    # pin, format, lint, tests"
say "      git diff --stat        # review; to undo: git checkout -- driver/UPSTREAM.env vendor/openshell/Cargo.toml vendor/openshell/Cargo.lock"
exit "$status"
