#!/usr/bin/env bash
# Install the OpenShell Firecracker plugin from pre-built release assets.
#
# Needs bash, curl, tar, sha256sum and install. No build toolchain.
#
# Download the install.sh attached to a release (it knows which repository to
# fetch from), read it, then run it:
#
#   sudo ./install.sh --binaries-only      # launcher and driver, under /usr/local
#   ./install.sh --config-only             # your user's systemd units and env file
#
# or both at once, to a prefix you can write:
#
#   ./install.sh --prefix "$HOME/.local"
set -euo pipefail

REPO="@REPO@"                   # stamped with the repository by the release workflow
PLACEHOLDER="@""REPO@"
TAG="latest"
PREFIX="/usr/local"
DO_BINARIES=1
DO_CONFIG=1
DRIVER=1                        # INTERIM: remove with interim-driver/
DRY_RUN=0

usage() {
    cat <<'EOF'
Usage: install.sh [options]

  --version TAG            plugin release to install (default: latest)
  --prefix DIR           install under DIR/libexec/openshell-firecracker (default: /usr/local)
  --binaries-only          install the executables only (the part that needs root)
  --config-only            install the systemd units and env file only (run as your user)
  --launcher-only          do not install the interim driver (use when your OpenShell
                           release has OPENSHELL_VM_LAUNCHER_BIN)
  --repo OWNER/NAME        GitHub repository to download from (set in release copies)
  --dry-run                print what would happen; download and change nothing
  -h, --help               this help
EOF
}

die() { printf 'install.sh: %s\n' "$*" >&2; exit 1; }
say() { printf '%s\n' "$*"; }

while [ $# -gt 0 ]; do
    case "$1" in
        --version) TAG="${2:?--version needs a value}"; shift 2 ;;
        --prefix) PREFIX="${2:?--prefix needs a value}"; shift 2 ;;
        --binaries-only) DO_CONFIG=0; shift ;;
        --config-only) DO_BINARIES=0; shift ;;
        --launcher-only) DRIVER=0; shift ;;
        --repo) REPO="${2:?--repo needs a value}"; shift 2 ;;
        --dry-run) DRY_RUN=1; shift ;;
        -h | --help) usage; exit 0 ;;
        *) usage >&2; die "unknown option: $1" ;;
    esac
done

[ "$REPO" != "$PLACEHOLDER" ] || die "no repository set: use the install.sh attached to a release, or pass --repo OWNER/NAME"
[[ "$REPO" =~ ^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$ ]] || die "invalid --repo: $REPO"
[[ "$TAG" == latest || "$TAG" =~ ^v[0-9][A-Za-z0-9._-]*$ ]] || die "invalid --version: $TAG"
[ "$(uname -s)" = Linux ] || die "Linux only"
case "$(uname -m)" in
    x86_64 | amd64) ARCH=x86_64 ;;
    *) die "unsupported architecture $(uname -m): only x86_64 release assets are built" ;;
esac

if [ "$TAG" = latest ]; then
    BASE="https://github.com/$REPO/releases/latest/download"
else
    BASE="https://github.com/$REPO/releases/download/$TAG"
fi
LIBEXEC="$PREFIX/libexec/openshell-firecracker"
CONFIG_HOME="${XDG_CONFIG_HOME:-$HOME/.config}"

LAUNCHER_ASSET="openshell-firecracker-launcher-$ARCH-linux.tar.gz"
# INTERIM: the driver is built against the latest OpenShell release when the plugin
# release is made; openshell-version.txt says which.
DRIVER_ASSET=""
if [ "$DO_BINARIES" -eq 1 ] && [ "$DRIVER" -eq 1 ]; then
    DRIVER_ASSET="openshell-driver-vm-interim-$ARCH-linux.tar.gz"
fi

if [ "$DRY_RUN" -eq 1 ]; then
    say "Dry run: nothing is downloaded or changed."
    say "Release:  $REPO ($TAG)"
    if [ "$DO_BINARIES" -eq 1 ]; then
        say "Download: $BASE/$LAUNCHER_ASSET"
        [ -z "$DRIVER_ASSET" ] || say "Download: $BASE/$DRIVER_ASSET"
        say "Install:  $LIBEXEC/openshell-firecracker-launcher"
        [ -z "$DRIVER_ASSET" ] || say "Install:  $LIBEXEC/openshell-driver-vm"
    fi
    if [ "$DO_CONFIG" -eq 1 ]; then
        say "Download: $BASE/examples.tar.gz"
        say "Install:  $CONFIG_HOME/systemd/user/openshell-driver-firecracker.service"
        say "Install:  $CONFIG_HOME/systemd/user/openshell-gateway.service.d/firecracker.conf"
        say "Install:  $CONFIG_HOME/openshell/firecracker-driver.env (only if absent)"
    fi
    exit 0
fi

command -v curl >/dev/null || die "curl is required"
command -v sha256sum >/dev/null || die "sha256sum is required"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

fetch() { curl -fsSL --retry 3 -o "$TMP/$1" "$BASE/$1" || die "download failed: $BASE/$1"; }

verify() {
    local name="$1" line
    line="$(grep -F -- "  $name" "$TMP/SHA256SUMS" | head -n1 || true)"
    [ -n "$line" ] || die "no checksum for $name in SHA256SUMS"
    (cd "$TMP" && printf '%s\n' "$line" | sha256sum -c --quiet -) || die "checksum mismatch: $name"
}

fetch SHA256SUMS

install_binaries() {
    mkdir -p "$TMP/bin"
    local assets=("$LAUNCHER_ASSET")
    [ -z "$DRIVER_ASSET" ] || assets+=("$DRIVER_ASSET")
    local asset
    for asset in "${assets[@]}"; do
        fetch "$asset"
        verify "$asset"
        tar -xzf "$TMP/$asset" -C "$TMP/bin"
    done
    mkdir -p "$LIBEXEC" 2>/dev/null || die "cannot create $LIBEXEC: run with sudo (--binaries-only) or choose --prefix"
    install -m 0755 "$TMP/bin/openshell-firecracker-launcher" "$LIBEXEC/openshell-firecracker-launcher"
    say "installed $LIBEXEC/openshell-firecracker-launcher"
    if [ -n "$DRIVER_ASSET" ]; then
        install -m 0755 "$TMP/bin/openshell-driver-vm-interim" "$LIBEXEC/openshell-driver-vm"
        say "installed $LIBEXEC/openshell-driver-vm"
        warn_if_gateway_differs
    fi
}

# INTERIM: the driver embeds a host supervisor that must speak the gateway's protocol.
warn_if_gateway_differs() {
    fetch openshell-version.txt
    verify openshell-version.txt
    local built gateway
    built="$(tr -d '[:space:]' < "$TMP/openshell-version.txt")"
    say "the driver was built against OpenShell $built"
    command -v openshell-gateway >/dev/null 2>&1 || return 0
    gateway="$(openshell-gateway --version 2>/dev/null | grep -Eo '[0-9]+\.[0-9]+\.[0-9]+' | head -n1 || true)"
    if [ -n "$gateway" ] && [ "v$gateway" != "$built" ]; then
        say "warning: your gateway is $gateway but the driver was built for $built." >&2
        say "         Upgrade OpenShell, or build interim-driver/ against your release." >&2
    fi
}

# Install $1 as $2 with mode $3, never overwriting a file that differs: write
# $2.new beside it instead.
place() {
    local src="$1" dest="$2" mode="$3"
    mkdir -p "$(dirname "$dest")"
    if [ ! -e "$dest" ]; then
        install -m "$mode" "$src" "$dest"
        say "installed $dest"
    elif cmp -s "$src" "$dest"; then
        say "unchanged $dest"
    else
        install -m "$mode" "$src" "$dest.new"
        say "kept your $dest; the new version is $dest.new"
    fi
}

install_config() {
    fetch examples.tar.gz
    verify examples.tar.gz
    tar -xzf "$TMP/examples.tar.gz" -C "$TMP"
    local ex="$TMP/examples"

    # Point the unit and env file at where the binaries were installed.
    sed "s|/usr/local/libexec/openshell-firecracker|$LIBEXEC|g" \
        "$ex/openshell-driver-firecracker.service" > "$TMP/unit"
    sed "s|/usr/local/libexec/openshell-firecracker|$LIBEXEC|g" \
        "$ex/firecracker-driver.env" > "$TMP/env"

    place "$TMP/unit" "$CONFIG_HOME/systemd/user/openshell-driver-firecracker.service" 0644
    place "$ex/gateway-dropin.conf" \
        "$CONFIG_HOME/systemd/user/openshell-gateway.service.d/firecracker.conf" 0644
    place "$TMP/env" "$CONFIG_HOME/openshell/firecracker-driver.env" 0600
}

[ "$DO_BINARIES" -eq 0 ] || install_binaries
[ "$DO_CONFIG" -eq 0 ] || install_config

cat <<EOF

Installed. Still to do (the installer does not guess these):

  1. Install Firecracker (1.4 or newer) and build the guest kernel:
       see the README, "Guest kernel".
  2. Edit $CONFIG_HOME/openshell/firecracker-driver.env:
       OPENSHELL_GRPC_ENDPOINT, OPENSHELL_VM_TLS_CA, OPENSHELL_VM_DRIVER_STATE_DIR,
       the image, and the Firecracker and kernel paths. Replace USER.
  3. Register the driver in $CONFIG_HOME/openshell/gateway.toml:

       [openshell.gateway]
       compute_driver = "firecracker"

       [openshell.drivers.firecracker]
       socket_path = "/run/user/$(id -u)/openshell-firecracker/driver.sock"

  4. Check the host, with the env file's values:
       set -a; . $CONFIG_HOME/openshell/firecracker-driver.env; set +a
       $LIBEXEC/openshell-firecracker-launcher check
  5. Start it, then restart the gateway:
       systemctl --user daemon-reload
       systemctl --user enable --now openshell-driver-firecracker.service
       systemctl --user restart openshell-gateway.service
EOF
