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

REPO="mr-miles/openshell-firecracker"                   # stamped with the repository by the release workflow
PLACEHOLDER="@""REPO@"
TAG="latest"
PREFIX="/usr/local"
DO_BINARIES=1
DO_CONFIG=1
DRIVER=1
KERNEL=1
SUBUIDS=1
FIRECRACKER=""                   # version to install, or empty to leave Firecracker alone
RUN_USER=""
DRY_RUN=0

usage() {
    cat <<'EOF'
Usage: install.sh [options]

  --version TAG            plugin release to install (default: latest)
  --prefix DIR           install under DIR/libexec/openshell-firecracker (default: /usr/local)
  --binaries-only          install the executables only (the part that needs root)
  --config-only            install the systemd units and env file only (run as your user)
  --launcher-only          do not install the VM driver (use when you build vendor/openshell
                           yourself)
  --firecracker VERSION    also download Firecracker (VERSION is a tag such as v1.17.0, or
                           latest) from its GitHub releases and install it as
                           PREFIX/bin/firecracker. Plugin needs 1.16.1 or newer
  --no-kernel              do not install the pre-built guest kernel (you supply your own)
  --no-subuids             do not set up per-VM uids (see below)
  --user NAME              the user who will run the driver (default: the user who ran sudo)

Per-VM uids (on by default with --binaries-only, which needs root): so that a VMM that
escaped its jail holds a uid owning nothing on the host, this gives NAME a range in
/etc/subuid and /etc/subgid if absent, installs the uidmap package (apt only), and
installs a udev rule making /dev/kvm world read/write. That last part lets every local
user use KVM; skip it all with --no-subuids and the plugin runs VMMs as NAME instead.
  --repo OWNER/NAME        GitHub repository to download from (set in release copies)
  --dry-run                print what would happen; download and change nothing
  -h, --help               this help
EOF
}

die() { printf 'install.sh: %s\n' "$*" >&2; exit 1; }
say() { printf '%s\n' "$*"; }

# Ubuntu 23.10 and later confine unprivileged user namespaces with AppArmor unless a
# profile allows the program. The launcher's jail needs them.
apparmor_restricts_userns() {
    [ "$(cat /proc/sys/kernel/apparmor_restrict_unprivileged_userns 2>/dev/null || true)" = 1 ]
}

while [ $# -gt 0 ]; do
    case "$1" in
        --version) TAG="${2:?--version needs a value}"; shift 2 ;;
        --prefix) PREFIX="${2:?--prefix needs a value}"; shift 2 ;;
        --binaries-only) DO_CONFIG=0; shift ;;
        --config-only) DO_BINARIES=0; shift ;;
        --launcher-only) DRIVER=0; shift ;;
        --firecracker) FIRECRACKER="${2:?--firecracker needs a version, e.g. v1.17.0 or latest}"; shift 2 ;;
        --no-kernel) KERNEL=0; shift ;;
        --no-subuids) SUBUIDS=0; shift ;;
        --user) RUN_USER="${2:?--user needs a value}"; shift 2 ;;
        --repo) REPO="${2:?--repo needs a value}"; shift 2 ;;
        --dry-run) DRY_RUN=1; shift ;;
        -h | --help) usage; exit 0 ;;
        *) usage >&2; die "unknown option: $1" ;;
    esac
done

[ "$REPO" != "$PLACEHOLDER" ] || die "no repository set: use the install.sh attached to a release, or pass --repo OWNER/NAME"
[[ "$REPO" =~ ^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$ ]] || die "invalid --repo: $REPO"
[[ "$TAG" == latest || "$TAG" =~ ^v[0-9][A-Za-z0-9._-]*$ ]] || die "invalid --version: $TAG"
[ -z "$FIRECRACKER" ] || [[ "$FIRECRACKER" == latest || "$FIRECRACKER" =~ ^v[0-9]+\.[0-9]+\.[0-9]+$ ]] || die "invalid --firecracker: $FIRECRACKER (use a tag like v1.17.0, or latest)"
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
# The driver and gateway run as a user service. Under sudo, $HOME is root's, so put
# the user's files in the home of the user who will run them, owned by them.
OWNER=""
RUN_HOME="$HOME"
RUN_UID="$(id -u)"
CONFIG_HOME="${XDG_CONFIG_HOME:-$HOME/.config}"
_run_user="${RUN_USER:-${SUDO_USER:-}}"
if [ "$(id -u)" -eq 0 ] && [ -n "$_run_user" ] && [ "$_run_user" != root ] && id "$_run_user" >/dev/null 2>&1; then
    OWNER="$_run_user"
    _home="$(getent passwd "$_run_user" | cut -d: -f6)"
    [ -n "$_home" ] || die "cannot find the home directory of $_run_user"
    RUN_HOME="$_home"
    RUN_UID="$(id -u "$_run_user")"
    CONFIG_HOME="$_home/.config"
elif [ "$(id -u)" -eq 0 ] && [ "$DO_CONFIG" -eq 1 ]; then
    say "note: running as root with no other user named, so the user service files go to $CONFIG_HOME."
    say "      Pass --user NAME to install them for the user who will run the driver."
fi

LAUNCHER_ASSET="openshell-firecracker-launcher-$ARCH-linux.tar.gz"
# The driver is the one vendored in this repository (vendor/openshell), built from the
# OpenShell revision named in openshell-commit.txt.
DRIVER_ASSET=""
if [ "$DO_BINARIES" -eq 1 ] && [ "$DRIVER" -eq 1 ]; then
    DRIVER_ASSET="openshell-driver-vm-$ARCH-linux.tar.gz"
fi
# The guest kernel is built by the release workflow, so nothing here compiles one.
KERNEL_ASSET=""
if [ "$DO_BINARIES" -eq 1 ] && [ "$KERNEL" -eq 1 ]; then
    KERNEL_ASSET="openshell-firecracker-kernel-$ARCH-linux.tar.gz"
fi

if [ "$DRY_RUN" -eq 1 ]; then
    say "Dry run: nothing is downloaded or changed."
    say "Release:  $REPO ($TAG)"
    if [ "$DO_BINARIES" -eq 1 ]; then
        say "Download: $BASE/$LAUNCHER_ASSET"
        [ -z "$DRIVER_ASSET" ] || say "Download: $BASE/$DRIVER_ASSET"
        [ -z "$KERNEL_ASSET" ] || say "Download: $BASE/$KERNEL_ASSET"
        if [ -n "$FIRECRACKER" ]; then
            say "Download: https://github.com/firecracker-microvm/firecracker/releases ($FIRECRACKER, firecracker-<tag>-$ARCH.tgz, checksum-verified)"
            say "Install:  $PREFIX/bin/firecracker"
        fi
        say "Install:  $LIBEXEC/openshell-firecracker-launcher"
        [ -z "$DRIVER_ASSET" ] || say "Install:  $LIBEXEC/openshell-driver-vm"
        [ -z "$KERNEL_ASSET" ] || say "Install:  $LIBEXEC/vmlinux"
        if apparmor_restricts_userns; then
            say "Install:  /etc/apparmor.d/openshell-firecracker-launcher (needs root; this host restricts user namespaces)"
        fi
        if [ "$SUBUIDS" -eq 1 ]; then
            say "Set up:   per-VM uids for ${RUN_USER:-${SUDO_USER:-<use --user>}}: /etc/subuid and /etc/subgid ranges,"
            say "          the uidmap package (apt), and a udev rule making /dev/kvm world read/write"
        fi
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

# Firecracker's own release: firecracker-<tag>-<arch>.tgz with a .sha256.txt beside it,
# extracting to release-<tag>-<arch>/firecracker-<tag>-<arch>.
install_firecracker() {
    local base="https://github.com/firecracker-microvm/firecracker/releases" tag="$FIRECRACKER" tgz dir
    if [ "$tag" = latest ]; then
        tag="$(curl -fsSIL -o /dev/null -w '%{url_effective}' "$base/latest" | sed 's|.*/||')"
        [[ "$tag" =~ ^v[0-9]+\.[0-9]+\.[0-9]+$ ]] || die "could not work out the latest Firecracker release (got '$tag'); pass --firecracker vX.Y.Z"
    fi
    tgz="firecracker-$tag-$ARCH.tgz"
    mkdir -p "$TMP/fc"
    curl -fsSL --retry 3 -o "$TMP/fc/$tgz" "$base/download/$tag/$tgz" || die "download failed: $base/download/$tag/$tgz"
    curl -fsSL --retry 3 -o "$TMP/fc/$tgz.sha256.txt" "$base/download/$tag/$tgz.sha256.txt" || die "download failed: $tgz.sha256.txt"
    (cd "$TMP/fc" && sha256sum -c --quiet "$tgz.sha256.txt") || die "checksum mismatch: $tgz"
    tar -xzf "$TMP/fc/$tgz" -C "$TMP/fc"
    dir="$TMP/fc/release-$tag-$ARCH"
    [ -f "$dir/firecracker-$tag-$ARCH" ] || die "$tgz does not contain firecracker-$tag-$ARCH"
    mkdir -p "$PREFIX/bin" 2>/dev/null || die "cannot create $PREFIX/bin: run with sudo (--binaries-only) or choose --prefix"
    install -m 0755 "$dir/firecracker-$tag-$ARCH" "$PREFIX/bin/firecracker"
    say "installed $PREFIX/bin/firecracker ($tag)"
}

install_binaries() {
    mkdir -p "$TMP/bin"
    [ -z "$FIRECRACKER" ] || install_firecracker
    local assets=("$LAUNCHER_ASSET")
    [ -z "$DRIVER_ASSET" ] || assets+=("$DRIVER_ASSET")
    [ -z "$KERNEL_ASSET" ] || assets+=("$KERNEL_ASSET")
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
        install -m 0755 "$TMP/bin/openshell-driver-vm" "$LIBEXEC/openshell-driver-vm"
        say "installed $LIBEXEC/openshell-driver-vm"
        warn_if_gateway_differs
    fi
    if [ -n "$KERNEL_ASSET" ]; then
        install -m 0644 "$TMP/bin/vmlinux" "$LIBEXEC/vmlinux"
        say "installed $LIBEXEC/vmlinux"
    fi
    install_apparmor_profile
    [ "$SUBUIDS" -eq 0 ] || setup_subuids
}

# True if /etc/$1 (subuid or subgid) already gives user $2 a range.
has_id_range() { grep -q "^$2:" "/etc/$1" 2>/dev/null; }

# Per-VM uids: each VMM runs as a subordinate uid of its own. Needs root. Every
# step is skipped when already done, and a step that cannot be done says so rather
# than failing the install: the plugin falls back to running VMMs as the driver's user.
setup_subuids() {
    local user="${RUN_USER:-${SUDO_USER:-}}"
    if [ "$(id -u)" -ne 0 ]; then
        say "note: per-VM uids need root; re-run with sudo --binaries-only, or pass --no-subuids."
        return 0
    fi
    if [ -z "$user" ] || [ "$user" = root ] || ! id "$user" >/dev/null 2>&1; then
        say "note: per-VM uids need the user who will run the driver: pass --user NAME (skipped)."
        return 0
    fi

    # Ranges of 65536 ids, after every range already allocated.
    local start file
    start="$(cat /etc/subuid /etc/subgid 2>/dev/null | awk -F: 'BEGIN { m = 100000 } { e = $2 + $3; if (e > m) m = e } END { print m }')"
    for file in subuid subgid; do
        if has_id_range "$file" "$user"; then
            say "unchanged /etc/$file (already has a range for $user)"
        else
            printf '%s:%s:65536\n' "$user" "$start" >> "/etc/$file"
            say "added a range for $user to /etc/$file ($start, 65536 ids)"
            start=$((start + 65536))
        fi
    done

    if command -v newuidmap >/dev/null 2>&1 && command -v newgidmap >/dev/null 2>&1; then
        say "unchanged uidmap (newuidmap and newgidmap are installed)"
    elif command -v apt-get >/dev/null 2>&1; then
        apt-get install -y uidmap && say "installed the uidmap package" \
            || say "warning: could not install uidmap; install the package that provides newuidmap and newgidmap" >&2
    else
        say "note: install the package that provides newuidmap and newgidmap (uidmap, or shadow-utils)."
    fi

    if [ -e /dev/kvm ]; then
        printf '%s\n' '# Installed by the OpenShell Firecracker plugin: per-VM uids need /dev/kvm usable by' \
            '# every user. Remove this file to restrict it to the kvm group again.' \
            'KERNEL=="kvm", GROUP="kvm", MODE="0666", OPTIONS+="static_node=kvm"' > "$TMP/kvm-rule"
        place "$TMP/kvm-rule" /etc/udev/rules.d/99-openshell-firecracker-kvm.rules 0644
        say "note: /dev/kvm is now usable by every local user (so VMMs can run as their own uid)."
        if command -v udevadm >/dev/null 2>&1; then
            udevadm control --reload-rules && udevadm trigger --name-match=kvm \
                || say "warning: could not apply the udev rule now; it applies at the next boot" >&2
        fi
    else
        say "note: /dev/kvm does not exist on this host, so there is no KVM rule to install."
    fi
}

# Let the launcher, and only the launcher, create user namespaces on hosts where
# AppArmor restricts them. Needs root, like the rest of --binaries-only.
install_apparmor_profile() {
    apparmor_restricts_userns || return 0
    local profile=/etc/apparmor.d/openshell-firecracker-launcher
    if [ "$(id -u)" -ne 0 ] || [ ! -d /etc/apparmor.d ] || ! command -v apparmor_parser >/dev/null 2>&1; then
        say "note: this host restricts unprivileged user namespaces with AppArmor, and the"
        say "      launcher needs them. Re-run with sudo --binaries-only to install a profile"
        say "      that allows it, or see the README, \"User namespaces\"."
        return 0
    fi
    cat > "$TMP/apparmor-profile" <<EOF
abi <abi/4.0>,
include <tunables/global>

profile openshell-firecracker-launcher $LIBEXEC/openshell-firecracker-launcher flags=(unconfined) {
  userns,

  include if exists <local/openshell-firecracker-launcher>
}
EOF
    place "$TMP/apparmor-profile" "$profile" 0644
    if apparmor_parser -r "$profile"; then
        say "loaded the AppArmor profile (allows user namespaces for the launcher only)"
    else
        say "warning: could not load $profile; see the README, \"User namespaces\"" >&2
    fi
}

# The driver embeds a host supervisor that must speak the gateway's protocol, and the
# protocol is not stable across OpenShell revisions. A gateway's version cannot be
# mapped to a revision, so this reports both and leaves the comparison to the operator.
warn_if_gateway_differs() {
    fetch openshell-commit.txt
    verify openshell-commit.txt
    local built gateway
    built="$(tr -d '[:space:]' < "$TMP/openshell-commit.txt")"
    say "the driver was built from OpenShell commit $built"
    if grep -qF -- "  openshell-runtime-commit.txt" "$TMP/SHA256SUMS"; then
        fetch openshell-runtime-commit.txt
        verify openshell-runtime-commit.txt
        say "warning: its embedded supervisor was built from $(cat "$TMP/openshell-runtime-commit.txt")," >&2
        say "         not that commit: a trial build; see the README, \"Compatibility\"." >&2
    fi
    command -v openshell-gateway >/dev/null 2>&1 || return 0
    gateway="$(openshell-gateway --version 2>/dev/null | head -n1 || true)"
    [ -z "$gateway" ] || say "your gateway reports: $gateway"
    say "note: the gateway must come from a compatible OpenShell revision; the supervisor"
    say "      protocol is not stable across revisions. See the README, \"Compatibility\"."
}

# Install $1 as $2 with mode $3, never overwriting a file that differs: write
# $2.new beside it instead. Nothing is written when $2 already matches, or when
# $2.new already holds this version (a repeat run), so reruns leave files alone.
place() {
    local src="$1" dest="$2" mode="$3"
    local own=()
    if [ -n "$OWNER" ] && [ "${dest#"$CONFIG_HOME"/}" != "$dest" ]; then
        # Create missing parents owned by the user, not root.
        local d="$CONFIG_HOME" part
        for part in $(dirname "${dest#"$CONFIG_HOME"/}" | tr / ' '); do
            [ "$part" = . ] && continue
            d="$d/$part"
            [ -d "$d" ] || install -d -m 0755 -o "$OWNER" "$d"
        done
        own=(-o "$OWNER")
    fi
    mkdir -p "$(dirname "$dest")"
    if [ ! -e "$dest" ]; then
        install -m "$mode" ${own[@]+"${own[@]}"} "$src" "$dest"
        say "installed $dest"
    elif cmp -s "$src" "$dest"; then
        say "unchanged $dest"
    elif [ -e "$dest.new" ] && cmp -s "$src" "$dest.new"; then
        say "unchanged $dest.new (your $dest differs from this version)"
    else
        install -m "$mode" ${own[@]+"${own[@]}"} "$src" "$dest.new"
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
    sed -e "s|/usr/local/libexec/openshell-firecracker|$LIBEXEC|g" \
        -e "s|/home/USER|$RUN_HOME|g" \
        "$ex/firecracker-driver.env" > "$TMP/env"

    place "$TMP/unit" "$CONFIG_HOME/systemd/user/openshell-driver-firecracker.service" 0644
    place "$ex/gateway-dropin.conf" \
        "$CONFIG_HOME/systemd/user/openshell-gateway.service.d/firecracker.conf" 0644
    place "$TMP/env" "$CONFIG_HOME/openshell/firecracker-driver.env" 0600

    # An env file from an earlier install is kept as it is, but without this variable
    # the driver boots libkrun VMs, so add it when it is missing.
    local envfile="$CONFIG_HOME/openshell/firecracker-driver.env"
    if [ -f "$envfile" ] && ! grep -q '^[[:space:]]*OPENSHELL_VM_LAUNCHER_BIN=' "$envfile"; then
        printf '\n# Added by install.sh: run the plugin for every VM.\nOPENSHELL_VM_LAUNCHER_BIN=%s\n' \
            "$LIBEXEC/openshell-firecracker-launcher" >> "$envfile"
        say "added OPENSHELL_VM_LAUNCHER_BIN to $envfile"
    fi
    # Show newer optional settings in a kept env file too, commented out so the
    # behavior does not change.
    if [ -f "$envfile" ] && ! grep -q 'OPENSHELL_VM_FIRECRACKER_LOG_LEVEL' "$envfile"; then
        printf '\n%s\n%s\n%s\n' \
            "# Added by install.sh. Firecracker's own log level, written to each sandbox's" \
            "# rootfs-console.log: off, error, warn (default), info, debug or trace." \
            "# OPENSHELL_VM_FIRECRACKER_LOG_LEVEL=warn" >> "$envfile"
        say "added a commented-out OPENSHELL_VM_FIRECRACKER_LOG_LEVEL to $envfile"
    fi
    if [ -f "$envfile" ] && ! grep -q 'OPENSHELL_VM_FIRECRACKER_ZRAM_PERCENT' "$envfile"; then
        printf '\n%s\n%s\n%s\n%s\n' \
            "# Added by install.sh. Compressed swap (zram) in each guest, as a percentage of" \
            "# its memory: 0 to 200, 0 for none. Raise OPENSHELL_VM_DRIVER_MEM_MIB (default" \
            "# 2048) to give VMs more memory." \
            "# OPENSHELL_VM_FIRECRACKER_ZRAM_PERCENT=50" >> "$envfile"
        say "added a commented-out OPENSHELL_VM_FIRECRACKER_ZRAM_PERCENT to $envfile"
    fi
}

[ "$DO_BINARIES" -eq 0 ] || install_binaries
[ "$DO_CONFIG" -eq 0 ] || install_config

cat <<EOF

Installed. Still to do (the installer does not guess these):

  1. Install OpenShell (0.1.x) and Firecracker (1.16.1 or newer) if you have not:
       see the README, "Install".
       The launcher finds Firecracker beside itself, on PATH, or in /usr/local/bin,
       /usr/bin or /opt/firecracker, and the guest kernel at $LIBEXEC/vmlinux.
       Set OPENSHELL_VM_FIRECRACKER_BIN or OPENSHELL_VM_FIRECRACKER_KERNEL in the env
       file only to put either somewhere else.
  2. Edit $CONFIG_HOME/openshell/firecracker-driver.env:
       OPENSHELL_GRPC_ENDPOINT, OPENSHELL_VM_TLS_CA, OPENSHELL_VM_DRIVER_STATE_DIR and
       the image. Replace USER.
  3. Register the driver in $CONFIG_HOME/openshell/gateway.toml:

       [openshell]
       version = 2          # required: the gateway refuses a file without it

       [openshell.gateway]
       compute_driver = "firecracker"

       [openshell.drivers.firecracker]
       socket_path = "/run/user/$RUN_UID/openshell-firecracker/driver.sock"

  4. Check the host, with the env file's values:
       set -a; . $CONFIG_HOME/openshell/firecracker-driver.env; set +a
       $LIBEXEC/openshell-firecracker-launcher check
  5. Start it, then restart the gateway:
       systemctl --user daemon-reload
       systemctl --user enable --now openshell-driver-firecracker.service
       systemctl --user restart openshell-gateway.service
EOF
