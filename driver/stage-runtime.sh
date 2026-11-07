#!/usr/bin/env bash
# Stage the compressed artifacts the vendored driver embeds at build time:
#
#   openshell-sandbox.zst      guest sandbox (static musl)
#   openshell-vm-init.zst      guest init helper (static musl)
#   openshell-supervisor.zst   host supervisor
#   umoci.zst                  guest OCI unpacker
#
# The first three are built from the OpenShell revision in driver/UPSTREAM.env (its
# runtime override if set, see below), using that revision's own bundling script; their
# crates are not vendored here. umoci is downloaded. Nothing for libkrun is needed.
#
# Needs: git, curl, zstd, readelf, a Rust toolchain with the musl target for this
# architecture, and zig + cargo-zigbuild (the mise task provides them).
#
#   OPENSHELL_DIR                        checkout location (default: ./openshell)
#   OPENSHELL_VM_RUNTIME_COMPRESSED_DIR  output (default: $OPENSHELL_DIR/target/vm-runtime-compressed)
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
root="$(dirname "$here")"
# shellcheck source=driver/UPSTREAM.env
. "$here/UPSTREAM.env"

# The runtime can come from another revision than the one the vendored driver crate
# pins, to try OpenShell changes against an unchanged driver: set
# OPENSHELL_RUNTIME_COMMIT (and optionally OPENSHELL_RUNTIME_REPO and
# OPENSHELL_RUNTIME_TAG) in UPSTREAM.env. Unset, the runtime uses the driver's pin.
if [ -n "${OPENSHELL_RUNTIME_COMMIT:-}" ]; then
    runtime_repo="${OPENSHELL_RUNTIME_REPO:-$OPENSHELL_REPO}"
    runtime_commit="$OPENSHELL_RUNTIME_COMMIT"
    runtime_tag="${OPENSHELL_RUNTIME_TAG:-}"
else
    runtime_repo="$OPENSHELL_REPO"
    runtime_commit="$OPENSHELL_COMMIT"
    runtime_tag="${OPENSHELL_TAG:-}"
fi

dir="${OPENSHELL_DIR:-$root/openshell}"
out="${OPENSHELL_VM_RUNTIME_COMPRESSED_DIR:-$dir/target/vm-runtime-compressed}"

case "$(uname -m)" in
    x86_64 | amd64) umoci_arch=amd64 ;;
    aarch64 | arm64) umoci_arch=arm64 ;;
    *) echo "stage-runtime: unsupported architecture $(uname -m)" >&2; exit 1 ;;
esac

# --- the pinned OpenShell revision -------------------------------------------------
# $dir may already hold a restored build cache (target/), so fetch into it rather than
# cloning into a non-empty directory.
if [ "$(git -C "$dir" rev-parse HEAD 2>/dev/null || true)" != "$runtime_commit" ]; then
    echo "==> Fetching OpenShell $runtime_commit"
    mkdir -p "$dir"
    git -C "$dir" init -q
    git -C "$dir" remote remove origin 2>/dev/null || true
    git -C "$dir" remote add origin "$runtime_repo"
    git -C "$dir" fetch -q --depth 1 origin "$runtime_commit"
    git -C "$dir" checkout -q --force FETCH_HEAD
fi
[ "$(git -C "$dir" rev-parse HEAD)" = "$runtime_commit" ] \
    || { echo "stage-runtime: checkout is not at $runtime_commit" >&2; exit 1; }

# The binaries built below take their version from the release tag at HEAD (OpenShell's
# build script reads `git tag --points-at HEAD`); a tagless checkout would report a
# development version such as 0.0.1-dev.1+g<sha>. So fetch the pinned tag, and refuse a
# tag that no longer points at the pinned commit.
if [ -n "${runtime_tag}" ]; then
    if ! git -C "$dir" rev-parse -q --verify "refs/tags/$runtime_tag^{commit}" >/dev/null; then
        git -C "$dir" fetch -q --depth 1 origin "refs/tags/$runtime_tag:refs/tags/$runtime_tag"
    fi
    [ "$(git -C "$dir" rev-parse "refs/tags/$runtime_tag^{commit}")" = "$runtime_commit" ] \
        || { echo "stage-runtime: tag $runtime_tag no longer points at $runtime_commit" >&2; exit 1; }
fi

# --- supervisor, guest sandbox, guest init -----------------------------------------
echo "==> Building the supervisor bundle"
OPENSHELL_VM_RUNTIME_COMPRESSED_DIR="$out" "$dir/tasks/scripts/vm/build-supervisor-bundle.sh"

# --- umoci -------------------------------------------------------------------------
echo "==> Downloading umoci $UMOCI_VERSION (linux/$umoci_arch)"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
base="https://github.com/opencontainers/umoci/releases/download/$UMOCI_VERSION"
curl -fsSL -o "$tmp/umoci" "$base/umoci.linux.$umoci_arch" \
    || curl -fsSL -o "$tmp/umoci" "$base/umoci.$umoci_arch"
chmod +x "$tmp/umoci"
zstd -19 -T0 -f -q "$tmp/umoci" -o "$out/umoci.zst"

for f in openshell-sandbox openshell-vm-init openshell-supervisor umoci; do
    [ -s "$out/$f.zst" ] || { echo "stage-runtime: missing $out/$f.zst" >&2; exit 1; }
done
echo "==> Staged in $out"
ls -l "$out"
