#!/usr/bin/env bash
# Print the version the driver declares to the gateway: this plugin's release version.
#
#   tag build (GITHUB_REF_TYPE=tag, or a v* tag at HEAD)   v0.1.0 -> 0.1.0
#   after a tag                                            0.1.0-dev.3+g1a2b3c4d5
#   no tags at all                                         0.0.0-dev.57+g1a2b3c4d5
#
# `mise run driver:build` passes the result to the build as OPENSHELL_DRIVER_VERSION. Without
# it, the driver reports the version OpenShell's core crate computes, which for a git
# dependency is a development string that never names a release.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
release='^v[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.]+)?$'

# CI checks a tag out as a shallow clone; the ref name is the reliable source there.
if [ "${GITHUB_REF_TYPE:-}" = tag ] && [[ "${GITHUB_REF_NAME:-}" =~ $release ]]; then
    printf '%s\n' "${GITHUB_REF_NAME#v}"
    exit 0
fi

git_ok() { git -C "$root" "$@" 2>/dev/null; }

if ! git_ok rev-parse --git-dir >/dev/null; then
    echo "0.0.0-dev"
    exit 0
fi

exact="$(git_ok tag --points-at HEAD | grep -E "$release" | sed 's/^v//' | sed 's/-/~/' | sort -V | sed 's/~/-/' | tail -n 1 || true)"
if [ -n "$exact" ]; then
    printf '%s\n' "$exact"
    exit 0
fi

sha="$(git_ok rev-parse --short=9 HEAD || echo unknown)"
if desc="$(git_ok describe --tags --long --match 'v[0-9]*')" \
    && [[ "$desc" =~ ^v(.+)-([0-9]+)-g[0-9a-f]+$ ]]; then
    printf '%s-dev.%s+g%s\n' "${BASH_REMATCH[1]}" "${BASH_REMATCH[2]}" "$sha"
else
    printf '0.0.0-dev.%s+g%s\n' "$(git_ok rev-list --count HEAD || echo 0)" "$sha"
fi
