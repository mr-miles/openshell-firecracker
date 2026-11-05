#!/usr/bin/env bash
# Stage the compressed artifacts the vendored driver embeds at build time:
#
#   openshell-sandbox.zst      guest sandbox (static musl)
#   openshell-vm-init.zst      guest init helper (static musl)
#   openshell-supervisor.zst   host supervisor
#   umoci.zst                  guest OCI unpacker
#
# The first three are built from the OpenShell revision in driver/UPSTREAM.env, using
# that revision's own bundling script; their crates are not vendored here. umoci is
# downloaded. Nothing for libkrun is needed.
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
if [ "$(git -C "$dir" rev-parse HEAD 2>/dev/null || true)" != "$OPENSHELL_COMMIT" ]; then
    echo "==> Fetching OpenShell $OPENSHELL_COMMIT"
    mkdir -p "$dir"
    git -C "$dir" init -q
    git -C "$dir" remote remove origin 2>/dev/null || true
    git -C "$dir" remote add origin "$OPENSHELL_REPO"
    git -C "$dir" fetch -q --depth 1 origin "$OPENSHELL_COMMIT"
    git -C "$dir" checkout -q --force FETCH_HEAD
fi
[ "$(git -C "$dir" rev-parse HEAD)" = "$OPENSHELL_COMMIT" ] \
    || { echo "stage-runtime: checkout is not at $OPENSHELL_COMMIT" >&2; exit 1; }

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
