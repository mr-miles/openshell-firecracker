#!/usr/bin/env bash
# Build the Firecracker guest kernel and package it for the release.
#
#   mise run kernel:package
#
# Users do not run this: the release workflow does, and install.sh downloads the
# result (openshell-firecracker-kernel-<arch>-linux.tar.gz). It is here so a
# different kernel can be built the same way.
#
# The kernel is Linux $KERNEL_SERIES (latest patch release), configured from
#   1. Firecracker's own CI guest config for that series (virtio-mmio, vsock, ...),
#   2. kernel/openshell-firecracker.kconfig (this repository),
#   3. OpenShell's openshell.kconfig for the release the driver is built against
#      (Landlock, seccomp, cgroups, hardening),
# then every symbol the last two ask for is asserted in the final .config, because
# a missing one shows up later as a silent hang or a panic at boot.
#
# Needs: bash curl tar xz sha256sum gcc make flex bison bc, libelf and OpenSSL
# headers (Debian/Ubuntu: build-essential flex bison bc libelf-dev libssl-dev xz-utils).
#
# Environment (all optional):
#   KERNEL_SERIES       Linux series (default 6.18; Firecracker ships a config for it)
#   KERNEL_VERSION      exact version, e.g. 6.18.7 (default: newest patch of the series)
#   FIRECRACKER_REF     Firecracker tag to take the base config from (default v1.17.0)
#   OPENSHELL_TAG       OpenShell release to take openshell.kconfig from
#                       (default: dist/openshell-version.txt, else the latest release; needs gh)
#   KERNEL_WORKDIR      build directory (default ./linux-build)
set -euo pipefail

die() { printf 'kernel/build.sh: %s\n' "$*" >&2; exit 1; }
say() { printf '==> %s\n' "$*"; }

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
root="$(cd "$here/.." && pwd)"

SERIES="${KERNEL_SERIES:-6.18}"
FIRECRACKER_REF="${FIRECRACKER_REF:-v1.17.0}"
WORK="${KERNEL_WORKDIR:-$root/linux-build}"
OUT="$root/dist"

case "$(uname -m)" in
    x86_64 | amd64) ARCH=x86_64 ;;
    *) die "only x86_64 is built for now (got $(uname -m))" ;;
esac

for tool in curl tar xz sha256sum gcc make flex bison bc; do
    command -v "$tool" >/dev/null 2>&1 || die "$tool is required (Debian/Ubuntu: build-essential flex bison bc libelf-dev libssl-dev xz-utils)"
done
[[ "$SERIES" =~ ^[0-9]+\.[0-9]+$ ]] || die "invalid KERNEL_SERIES: $SERIES"

# --- Which OpenShell release's kernel options to use ---------------------------
tag="${OPENSHELL_TAG:-}"
if [ -z "$tag" ] && [ -f "$root/dist/openshell-version.txt" ]; then
    tag="$(tr -d '[:space:]' < "$root/dist/openshell-version.txt")"
fi
if [ -z "$tag" ]; then
    command -v gh >/dev/null 2>&1 || die "set OPENSHELL_TAG (or install gh to look up the latest release)"
    tag="$(gh api repos/NVIDIA/OpenShell/releases/latest --jq .tag_name)"
fi
[[ "$tag" =~ ^v[0-9][A-Za-z0-9._-]*$ ]] || die "unexpected OpenShell tag: $tag"

# --- Which kernel -------------------------------------------------------------
major="${SERIES%%.*}"
base_url="https://cdn.kernel.org/pub/linux/kernel/v${major}.x"
mkdir -p "$WORK" "$OUT"
cd "$WORK"

curl -fsSL --retry 3 -o sha256sums.asc "$base_url/sha256sums.asc" || die "cannot download $base_url/sha256sums.asc"
series_re="${SERIES//./\\.}"
if [ -n "${KERNEL_VERSION:-}" ]; then
    [[ "$KERNEL_VERSION" =~ ^$series_re(\.[0-9]+)?$ ]] || die "KERNEL_VERSION $KERNEL_VERSION is not in series $SERIES"
    version="$KERNEL_VERSION"
else
    version="$(awk -v re="^linux-${series_re}(\\\\.[0-9]+)?\\\\.tar\\\\.xz\$" '$2 ~ re { sub(/^linux-/, "", $2); sub(/\.tar\.xz$/, "", $2); print $2 }' sha256sums.asc | sort -V | tail -n1)"
    [ -n "$version" ] || die "no linux-$SERIES.* tarball listed in $base_url/sha256sums.asc"
fi
tarball="linux-$version.tar.xz"
expected="$(awk -v f="$tarball" '$2 == f { print $1 }' sha256sums.asc | head -n1)"
[ -n "$expected" ] || die "no checksum for $tarball in sha256sums.asc"

say "Linux $version, Firecracker config from $FIRECRACKER_REF, OpenShell options from $tag"

if [ ! -f "$tarball" ]; then
    curl -fsSL --retry 3 -o "$tarball" "$base_url/$tarball" || die "cannot download $base_url/$tarball"
fi
# Integrity of the download against kernel.org's published list; the list itself is
# fetched over TLS from the same host.
printf '%s  %s\n' "$expected" "$tarball" | sha256sum -c --quiet - || die "checksum mismatch: $tarball"
rm -rf "linux-$version"
tar -xJf "$tarball"

# --- Configuration ------------------------------------------------------------
base_config="microvm-kernel-ci-$ARCH-$SERIES.config"
curl -fsSL --retry 3 -o base.config \
    "https://raw.githubusercontent.com/firecracker-microvm/firecracker/$FIRECRACKER_REF/resources/guest_configs/$base_config" \
    || die "Firecracker $FIRECRACKER_REF has no guest config $base_config"
curl -fsSL --retry 3 -o openshell.kconfig \
    "https://raw.githubusercontent.com/NVIDIA/OpenShell/$tag/crates/openshell-driver-vm/runtime/kernel/openshell.kconfig" \
    || die "OpenShell $tag has no crates/openshell-driver-vm/runtime/kernel/openshell.kconfig"
ours="$here/openshell-firecracker.kconfig"

cd "linux-$version"
cp ../base.config .config
scripts/kconfig/merge_config.sh -m .config "$ours" ../openshell.kconfig >/dev/null
make olddefconfig >/dev/null

say "Checking that every required option survived"
missing=0
while read -r line; do
    grep -qxF -- "$line" .config || { echo "MISSING $line" >&2; missing=1; }
done < <(cat "$ours" ../openshell.kconfig | grep -E '^(CONFIG_[A-Z0-9_]+=y|# CONFIG_[A-Z0-9_]+ is not set)$' | sort -u)
[ "$missing" -eq 0 ] || die "the final kernel configuration lacks options listed above (a dependency overrode them)"

# --- Build --------------------------------------------------------------------
say "Building vmlinux with $(nproc) jobs"
make -j"$(nproc)" vmlinux >/dev/null
strip --strip-debug vmlinux

stage="$(mktemp -d)"
trap 'rm -rf "$stage"' EXIT
install -m 0644 vmlinux "$stage/vmlinux"
{
    echo "linux $version"
    echo "firecracker-config $FIRECRACKER_REF $base_config"
    echo "openshell-options $tag"
} > "$stage/kernel-version.txt"
tar -czf "$OUT/openshell-firecracker-kernel-$ARCH-linux.tar.gz" -C "$stage" vmlinux kernel-version.txt
say "Wrote $OUT/openshell-firecracker-kernel-$ARCH-linux.tar.gz ($(du -h "$stage/vmlinux" | cut -f1) uncompressed)"
