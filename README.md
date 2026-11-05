# OpenShell Firecracker plugin

Run OpenShell sandboxes as Firecracker microVMs. The gateway is not modified.

This is an external compute driver in the sense of
[the driver docs](https://docs.nvidia.com/openshell/dev/extensibility/drivers#compute-drivers):
a separate service listening on a Unix socket that implements `compute_driver.proto`,
which the gateway does not start or supervise. The service is OpenShell's VM driver,
**vendored in this repository** (`vendor/openshell`) and trimmed to what Firecracker needs;
the launcher (`src/`) is the program it runs to boot each VM. It is a Firecracker-only
driver: libkrun, QEMU, GPU passthrough and virtiofs bind mounts are not part of it.

It also differs from OpenShell's driver in one behavior: a sandbox starts the way a
container does, with the image's `ENV` and in the image's `WORKDIR`
([Image ENV and WORKDIR](#image-env-and-workdir)).

> Status: unit-tested, but the vendored driver has not yet been booted against a real VM.
> See [Verification status](#verification-status).

## How it works

```
 openshell-gateway ──gRPC over Unix socket──▶ openshell-driver-vm   (vendored; external service)
                                                  │ for every VM: run the launcher
                                                  ▼
                                        openshell-firecracker-launcher   (this plugin)
                                                  │ jailed, rootless
                                                  ▼
 host supervisor ◀──── bridge ◀──── Firecracker ◀──vsock── guest (openshell-sandbox)
```

- The driver does the control-plane work: images, overlays, Stop/Start generations, launch
  authentication, the host supervisor, policy. It boots no VMM itself.
- For each VM, and for image preparation, it runs its launcher with `--internal-run-vm`
  and a fixed set of flags (`OPENSHELL_VM_LAUNCHER_BIN` names it, and is required). This
  plugin is that launcher; it boots the guest under Firecracker.
- The driver's image-preparation worker is not a VM, so the launcher hands it back to the
  driver binary, found as the launcher's parent process.
- Guest to host traffic goes over vsock. Firecracker connects to a socket the launcher
  listens on, and the launcher relays to the supervisor. The supervisor's socket never
  enters the VMM jail.

The guest has no network device.

## The vendored driver

`vendor/openshell` holds the crates of OpenShell's VM driver and their dependencies, taken
from NVIDIA/OpenShell commit `8d0797a` (pinned in [`driver/UPSTREAM.env`](driver/UPSTREAM.env)),
as their own cargo workspace with upstream's lockfile. The changes from upstream are listed
in [its README](vendor/openshell/crates/openshell-driver-vm/README.md): image `ENV` and
`WORKDIR`, `OPENSHELL_VM_LAUNCHER_BIN`, and the removal of everything Firecracker does not
use. The host supervisor and guest sandbox binaries the driver embeds are not vendored: they
are built from the same pinned commit when a release is made.

### Compatibility

The supervisor protocol is not stable across OpenShell revisions, so the gateway must come
from a revision compatible with the one in `driver/UPSTREAM.env`. The vendored commit is a
`main` commit, not a release tag. Whether it is compatible with a given OpenShell release
(such as 0.1.x) has not been checked: until it is, run a gateway built from that commit
or a later release known to match. `install.sh` prints the commit the installed driver
was built from and the gateway's reported version, and leaves the comparison to you.

### Image ENV and WORKDIR

OpenShell's driver unpacks an image into a disk and ignored the image's `Config.Env` and
`Config.WorkingDir`: every workload started in `/sandbox` with only the sandbox spec's
environment. This driver reads both from the image configuration (registry images, and
local Docker or Podman images), keeps them beside the cached disk as `image-config.json`,
and applies them:

- **Environment.** The image's `ENV` first, then the sandbox template's, then the sandbox
  spec's, so the spec always wins. Entries the supervisor reserves (`OPENSHELL_*`) and
  malformed ones are dropped.
- **Working directory.** The workload starts in the image's `WORKDIR`. The guest init
  creates it if absent and hands it to the sandbox user, so it is writable as in a
  container, and it is passed to the supervisor as `--workdir`, which also makes it
  writable under the filesystem policy. `HOME` follows it, unless the image sets `HOME`.
- **No `WORKDIR`.** Starts in `/`, as a container would; nothing is made writable beyond
  what the policy lists (by default only `/tmp`), and the driver logs a warning at launch.
  Images that relied on the old `/sandbox` default (for example ones built from
  OpenShell's `deploy/docker/Dockerfile.sandbox`) need a `WORKDIR /sandbox`, or set
  `OPENSHELL_VM_DEFAULT_WORKDIR=/sandbox` on the driver: it applies to any sandbox whose
  image sets no `WORKDIR`, rootfs tar uploads included, and never overrides an image's.
  It must be an absolute path below `/` and is validated at driver start.
- **Refused `WORKDIR`.** A relative path, one containing `..`, or one under `/proc`,
  `/sys`, `/dev` or OpenShell's own directories is ignored with a warning.
- Rootfs tar uploads carry no image configuration, so they start in `/` unless `OPENSHELL_VM_DEFAULT_WORKDIR` is set.
- The image's `User` is not applied.

Prepared-image caches from before this change are rebuilt once (cache layout `v5`).

## Requirements

What you need, and the [Install](#install) step that provides it:

| Need | Step |
|---|---|
| An installed OpenShell gateway from a revision compatible with the driver's ([Compatibility](#compatibility)); the host-supervisor protocol is required. The older v0.0.x line runs the supervisor inside the guest and needs a guest NIC; this plugin does not support it | 1 |
| Firecracker 1.16.1 or newer (the first release that validates the Linux 6.18 guest kernel the plugin ships) | 2 |
| The launcher, the VM driver and a guest kernel (all pre-built) | 3 |
| Linux x86_64 with `/dev/kvm` readable and writable by the driver's user, unprivileged user namespaces, and `e2fsprogs` (`debugfs`, `mke2fs`) | 4 |
| A `gateway_jwt` signing bundle in the gateway: the driver requires launch authentication, or sandbox creation fails | 5 |

Nothing needs a build toolchain. aarch64 has no pre-built assets.

## Install

Seven steps, in order. Steps 1 to 4 are one-off; `check` (step 6) verifies all of them.

### 1. Install OpenShell

```shell
curl -LsSf https://raw.githubusercontent.com/NVIDIA/OpenShell/main/install.sh | sh
openshell-gateway --version        # must be 0.1.x
```

That is OpenShell's own installer (it sets up the CLI and a local gateway; see
[its docs](https://docs.nvidia.com/openshell/latest/about/installation)). To read it first,
download it with `curl -LsSfo`, read it, then run it with `sh`.

### 2. Install Firecracker

The easy way is to let the plugin installer do it in step 3: add `--firecracker latest` (or
a tag such as `--firecracker v1.17.0`) to `sudo ./install.sh --binaries-only`. It downloads
Firecracker's release, verifies its checksum and installs `/usr/local/bin/firecracker`
(under `--prefix`). To do it by hand:

Release tarballs from [the Firecracker releases](https://github.com/firecracker-microvm/firecracker/releases)
extract to `release-<version>-<arch>/`:

```shell
arch=$(uname -m)
tag=$(curl -fsSL https://api.github.com/repos/firecracker-microvm/firecracker/releases/latest \
  | grep -m1 '"tag_name"' | cut -d'"' -f4)
curl -fsSL "https://github.com/firecracker-microvm/firecracker/releases/download/$tag/firecracker-$tag-$arch.tgz" | tar -xz
sudo install -m 0755 "release-$tag-$arch/firecracker-$tag-$arch" /usr/local/bin/firecracker
firecracker --version              # 1.16.1 or newer
```

Each tarball has a `.sha256.txt` beside it on the release page. The launcher finds
`firecracker` beside itself, on `PATH`, or in `/usr/local/bin`, `/usr/bin` or
`/opt/firecracker`; set `OPENSHELL_VM_FIRECRACKER_BIN` only to use another location.

### 3. Install the plugin

Each plugin release carries pre-built assets and an installer: the launcher, the VM driver
(built from `vendor/openshell`; `openshell-commit.txt` names the OpenShell commit it is from),
and a guest kernel built for Firecracker. Download
`install.sh` from the release (not from the repository: the release copy is stamped with
the repository to download from), read it, then:

```shell
curl -fsSLO https://github.com/mr-miles/openshell-firecracker/releases/latest/download/install.sh
chmod +x install.sh
sudo ./install.sh --binaries-only     # launcher, driver and kernel, under /usr/local/libexec/openshell-firecracker
./install.sh --config-only            # your user's systemd units and env file
./install.sh --dry-run                # show what it would do, change nothing
```

It verifies every download against `SHA256SUMS`, never overwrites a file you have edited
(it writes `<file>.new` beside it), and prints the OpenShell commit the driver was built from next to your gateway's version. It needs only `bash`, `curl`, `tar` and `sha256sum`. It does not
edit `gateway.toml`; it prints what is left to do. `--no-kernel` skips the kernel (to use
your own); `--launcher-only` skips the driver, if you build `vendor/openshell` yourself;
`--no-subuids` skips the per-VM uid setup in
[Host setup](#4-host-setup). The assets are built for x86_64 Linux only.

The guest kernel is installed as `vmlinux` beside the launcher, where the launcher looks for
it; set `OPENSHELL_VM_FIRECRACKER_KERNEL` only to use another one.

### 4. Host setup

One-off, per host:

- **KVM.** Your user needs read/write access to `/dev/kvm`: usually membership of the `kvm`
  group (`sudo usermod -aG kvm "$USER"`, then log in again).
- **`e2fsprogs`**, for `debugfs` and `mke2fs` (`sudo apt install e2fsprogs`; usually present).
- **User namespaces.** The VMM jail needs unprivileged user namespaces
  (`sysctl user.max_user_namespaces` above 0). Ubuntu 23.10 and later also confine them with
  AppArmor (`cat /proc/sys/kernel/apparmor_restrict_unprivileged_userns` prints `1`), which
  makes the jail fail with `write /proc/self/setgroups: Permission denied`. `sudo
  ./install.sh --binaries-only` installs a profile that allows them for the launcher only.
  To do it by hand, create `/etc/apparmor.d/openshell-firecracker-launcher`:

  ```
  abi <abi/4.0>,
  include <tunables/global>

  profile openshell-firecracker-launcher /usr/local/libexec/openshell-firecracker/openshell-firecracker-launcher flags=(unconfined) {
    userns,

    include if exists <local/openshell-firecracker-launcher>
  }
  ```

  and load it with `sudo apparmor_parser -r /etc/apparmor.d/openshell-firecracker-launcher`.
  The alternative, `sudo sysctl kernel.apparmor_restrict_unprivileged_userns=0`, lifts the
  restriction for every program on the host.
- **Per-VM uids.** With the default `OPENSHELL_VM_FIRECRACKER_SUBUIDS=auto`, each VMM runs as
  a subordinate uid if the host allows it, and otherwise as the driver's user with a warning
  ([Per-VM uid](#per-vm-uid)). `sudo ./install.sh --binaries-only` sets this up for the user
  who ran `sudo` (or `--user NAME`), skipping anything already in place:
  - a 65536-id range for that user in `/etc/subuid` and `/etc/subgid`, after the ranges
    already allocated;
  - the `uidmap` package (`newuidmap`, `newgidmap`), on apt-based systems; elsewhere it tells
    you to install it;
  - a udev rule, `/etc/udev/rules.d/99-openshell-firecracker-kvm.rules`, making `/dev/kvm`
    read/write for everyone. **This lets every local user use KVM.** Delete the file to
    undo it.

  Pass `--no-subuids` to skip all of it. Then, or if you installed without the installer, do
  the same by hand: check `grep "$USER" /etc/subuid /etc/subgid` (add ranges with `sudo
  usermod --add-subuids 200000-265535 --add-subgids 200000-265535 "$USER"`), install
  `uidmap`, and make `/dev/kvm` world read/write.

### 5. Configure

- Edit `~/.config/openshell/firecracker-driver.env` (installed by `--config-only`):
  `OPENSHELL_GRPC_ENDPOINT`, `OPENSHELL_VM_TLS_CA`, `OPENSHELL_VM_DRIVER_STATE_DIR`, the
  image, and `USER`. The comments in the file say which are required.
- Register the driver in `~/.config/openshell/gateway.toml` and make sure the gateway has a
  `gateway_jwt` signing bundle ([Connect](#connect-to-the-installed-gateway)).

### 6. Check

With the same environment as the service:

```shell
set -a; . ~/.config/openshell/firecracker-driver.env; set +a
/usr/local/libexec/openshell-firecracker/openshell-firecracker-launcher check
```

It checks every step above, in groups, and exits nonzero if anything failed:

| Group | Checks |
|---|---|
| OpenShell | the gateway is installed and 0.1.x; the `openshell` CLI; the VM driver exists and accepts `OPENSHELL_VM_LAUNCHER_BIN`; that variable points at this launcher |
| Host | Firecracker found (where) and new enough; the guest kernel found and uncompressed; `/dev/kvm` and the VMM jail (user namespaces, AppArmor); `debugfs`, `mke2fs`; sub-uid support |
| Driver environment | gateway endpoint, TLS CA file, state directory, an image, a valid admission JSON |
| Gateway configuration | `gateway.toml` selects `firecracker` and gives a `socket_path`; launch authentication: `gateway_jwt` in it, or the local signing keys in `~/.local/state/openshell/tls/jwt/` |

Each failure says what to do. The example unit runs it before the driver starts. It does
not boot a VM.

### 7. Start

```shell
systemctl --user daemon-reload
systemctl --user enable --now openshell-driver-firecracker.service
systemctl --user restart openshell-gateway.service     # after every driver restart too
openshell gateway info                                  # shows the firecracker driver
```

## Build from source

For another architecture, or to change the plugin or driver. Nothing here is needed to use
a release.

- **The launcher** (Rust 1.95; see `rust-toolchain.toml`):

  ```shell
  cargo build --release
  sudo install -D target/release/openshell-firecracker-launcher \
    /usr/local/libexec/openshell-firecracker/openshell-firecracker-launcher
  ```

- **The driver** (`vendor/openshell`). Checking it needs only Rust (`mise run driver:ci`).
  Building a release driver also stages the runtime it embeds: the host supervisor, guest
  sandbox and guest init, built from the OpenShell commit in `driver/UPSTREAM.env` with that
  commit's own bundling script, plus umoci. That needs `mise`, `git`, `zstd` and `curl`; it
  installs only Rust, zig and cargo-zigbuild, not OpenShell's full toolchain. It compiles
  a good part of OpenShell, so expect it to be slow the first time:

  ```shell
  mise run driver:package
  tar -xzf dist/openshell-driver-vm-$(uname -m)-linux.tar.gz -C dist
  sudo install -D dist/openshell-driver-vm \
    /usr/local/libexec/openshell-firecracker/openshell-driver-vm
  ```

  Use this driver, not OpenShell's packaged `openshell-driver-vm`: that one cannot run the
  Firecracker launcher.

- **The guest kernel**, which is the part that needs a C toolchain (`build-essential flex
  bison bc libelf-dev libssl-dev xz-utils`). Firecracker needs an uncompressed `vmlinux`
  with virtio-mmio, not PCI.
  [`kernel/build.sh`](kernel/build.sh) downloads the newest Linux 6.18 (checked against
  kernel.org's checksums), starts from Firecracker's own CI config for that series, merges
  [`kernel/openshell-firecracker.kconfig`](kernel/openshell-firecracker.kconfig) and
  the vendored driver's `openshell.kconfig` (Landlock, seccomp, cgroups), fails if any option either
  asks for is missing from the final configuration (a missing one shows up later as a silent
  hang or a panic at boot), and packages the stripped `vmlinux`:

  ```shell
  mise run kernel:package
  tar -xzf dist/openshell-firecracker-kernel-$(uname -m)-linux.tar.gz -C dist
  sudo install -m 0644 dist/vmlinux /usr/local/libexec/openshell-firecracker/vmlinux
  ```

  `KERNEL_SERIES`, `KERNEL_VERSION` and `FIRECRACKER_REF` choose another kernel (see the top
  of the script). The release workflow runs the same script.

## Connect to the installed gateway

1. **Pick a driver name** that is not a built-in: `firecracker`. The stock gateway
   registers `docker`, `kubernetes`, `podman` and `vm`.
2. **Run the driver as the gateway's user.** The gateway does no authentication of its
   own. The driver checks the peer, and the gateway's pid is not known in advance, so the
   driver runs with `--allow-same-uid-peer`, as the same UID as the gateway. The packaged
   gateway is a user service, so this is a user unit.
3. **Give the socket a dedicated directory.** The driver requires the socket's parent
   directory to be owned by its user and not a symlink, and sets it to `0700`. Use
   `$XDG_RUNTIME_DIR/openshell-firecracker/`. Do not use `/tmp`: the packaged unit has
   `PrivateTmp=true`, so the gateway cannot see sockets there.
4. **Register it** in `~/.config/openshell/gateway.toml`
   ([`examples/gateway.toml`](examples/gateway.toml)):

   ```toml
   [openshell]
   version = 2

   [openshell.gateway]
   compute_driver = "firecracker"

   [openshell.drivers.firecracker]
   socket_path = "/run/user/1000/openshell-firecracker/driver.sock"
   ```

   `socket_path` is the only key the gateway reads from this table, apart from
   `allow_driver_config` and `resource_admission`, which it reads for admission. Put
   `guest_tls_ca` under `[openshell.gateway]`; the gateway rejects `guest_tls_*` keys in a
   driver table.
5. **Start the driver before the gateway**, and restart the gateway whenever the driver
   restarts. The gateway reads the driver's capabilities at startup and again before every
   validate, create and start. If the driver's admission acknowledgement changed, those
   calls fail until the gateway restarts.
6. **Verify**: `openshell gateway info` shows the active driver and negotiated
   capabilities.

Example units and environment are in [`examples/`](examples/): the driver service, its
environment file, a gateway drop-in that orders the two, and the `gateway.toml` snippet.

## Configuration

The driver reads its own options (below). The plugin reads these, from the environment the
driver inherits:

| Variable | Purpose |
|---|---|
| `OPENSHELL_VM_LAUNCHER_BIN` | Required, read by the driver: absolute path to `openshell-firecracker-launcher`. The driver will not start without it |
| `OPENSHELL_VM_FIRECRACKER_BIN` | Optional. Absolute path to Firecracker. Default: `firecracker` beside the launcher, on `PATH`, or in `/usr/local/bin`, `/usr/bin`, `/opt/firecracker` |
| `OPENSHELL_VM_FIRECRACKER_KERNEL` | Optional. Absolute path to the guest `vmlinux`. Default: `vmlinux` beside the launcher, or in `/usr/local/libexec/openshell-firecracker`, `/var/lib/openshell-firecracker` |
| `OPENSHELL_VM_FIRECRACKER_SUBUIDS` | `auto` (default), `required`, or `off`; see [Per-VM uid](#per-vm-uid) |
| `OPENSHELL_VM_FIRECRACKER_LEASE_DIR` | Sub-uid lease directory. Defaults to `$XDG_RUNTIME_DIR/openshell-firecracker-leases` |
| `OPENSHELL_FIRECRACKER_DRIVER` | Path of the driver, if it cannot be found from the launcher's parent |

The driver's `OPENSHELL_VM_DEFAULT_WORKDIR` (optional) sets the working directory for
sandboxes whose image has none ([Image ENV and WORKDIR](#image-env-and-workdir)).

The driver itself needs `OPENSHELL_GRPC_ENDPOINT` (required), `OPENSHELL_VM_TLS_CA` for an
`https://` endpoint, `OPENSHELL_VM_DRIVER_STATE_DIR`, at least one of
`OPENSHELL_VM_BOOTSTRAP_IMAGE` / `OPENSHELL_SANDBOX_IMAGE`, and
`OPENSHELL_DRIVER_ADMISSION_CONFIG_JSON`, which must match the gateway's
`allow_driver_config` and `resource_admission` for this driver (`{}` means defaults on
both sides). The gateway passes its proxy settings and gateway name to a driver it
spawns; an externally run driver must set the `OPENSHELL_VM_UPSTREAM_PROXY*` and
`OPENSHELL_GATEWAY_NAME` equivalents itself.

## Per-VM uid

With `OPENSHELL_VM_FIRECRACKER_SUBUIDS` at `auto` or `required`, each VMM runs as a
subordinate uid of its own. Inside its user namespace, id 0 is the driver's user, which
builds the jail, and id 1 is the leased uid Firecracker then runs as, with no
capabilities. A VMM that escaped the jail would hold a uid that owns nothing on the host.

| Mode | Behavior |
|---|---|
| `auto` (default) | Use subordinate uids when the host supports them; otherwise warn and run as the driver's user |
| `required` | Refuse to launch unless the host supports them |
| `off` | VMM runs as the driver's user |

Host requirements: ranges for the driver's user in `/etc/subuid` and `/etc/subgid`; the
setuid `newuidmap` and `newgidmap` helpers (package `uidmap`); `/dev/kvm` readable and
writable by all users; Firecracker executable and the kernel readable by all users.

Launchers coordinate through `flock`ed lease files, because the driver cannot hand
a launcher an id. The kernel releases a lease when its launcher exits, so a crash cannot
leak an id. The launcher shares only what the VMM needs: the overlay disk and a private
socket directory are group-accessible, the sandbox state directory stays `0700`, and the
VMM reaches its files through bind mounts.

The startup check does not exercise a boot, so a host that passes it can still fail on the
first sandbox. If that happens, set `off` to confirm the cause and read
`rootfs-console.log`.

## What does not work

- No virtiofs bind mounts and no GPU passthrough on Firecracker. A sandbox that asks for
  either is rejected at validation, not ignored.
- `[openshell.supervisor.network].additional_ca_bundle` and the shared supervisor pool
  are rejected by the gateway for any external driver.
- Rootfs tar uploads need the driver and gateway on the same filesystem and UID.
- `openshell-gateway config preflight` skips host-tool checks for an external driver;
  `openshell-firecracker-launcher check` covers the Firecracker ones.
- The VM driver does not advertise `isolation-backend` or `suspend-resume`, and does not
  implement `AuthenticateSandbox`.
- Stop kills the VMM without a guest flush. The overlay uses `Writeback`
  caching so fsyncs reach the host file, and ext4 stays crash-consistent.
- The Firecracker and kernel paths are checked when a VM launches, not pinned by hash.
- aarch64 is untested.

## Security notes

- Each VMM runs rootless in its own user, mount, network, IPC, UTS and PID namespaces, on
  a tmpfs root holding only Firecracker, the kernel, the sandbox's disks and VMM config,
  the vsock directory and three device nodes. It drops the capability bounding set, sets
  `no_new_privs`, disables core dumps, and keeps Firecracker's own seccomp filter.
- The guest environment file sits in the sandbox's own overlay (mode `0600`, root-owned).
  It holds what the driver passes to the guest init.
- Firecracker's `jailer` is not used.

## Verification status

| Check | Status |
|---|---|
| Config builder, argument parsing, `check`, `cargo clippy` / `rustfmt`, CI | Unit tests pass and CI runs |
| End to end on one real host | `openshell sandbox create` boots a Firecracker sandbox: the release-built Linux 6.18 guest kernel (with `# CONFIG_ACPI is not set`), the jail, image preparation, the gateway-to-driver socket and the supervisor connection all worked. Needed fixes along the way are in the git history (`/dev/fd` links, `gateway.toml` schema version and socket path) |
| Vendored driver | **Not booted.** The end-to-end result above was with OpenShell's v0.1.6 driver plus a launcher-override patch, before the driver was vendored. The vendored driver (commit `8d0797a`, with `ENV`/`WORKDIR` and the removals) passes its unit tests and builds, but has not run a sandbox. The release packaging (`driver:package`, `driver/stage-runtime.sh`) has not been run end to end either: only its source fetch and umoci download were checked |
| Not checked on that host | The sub-uid isolation path (`OPENSHELL_VM_FIRECRACKER_SUBUIDS`), the AppArmor profile in `install.sh`, the guest clock, long-running or concurrent sandboxes, image `ENV`/`WORKDIR` in a real guest, and OpenShell's conformance suite (`tests/suites/conformance` in the OpenShell repository), which is what would show parity with other drivers |
| Other hosts | One Ubuntu-style x86_64 host only; no arm64 |

Confirmed from the OpenShell repository, not by running anything: the launcher flag
contract, the `RuntimeInitiatedUnix` transport, the `0666` supervisor control socket, and the
external-driver registration keys (`compute_driver`, `[openshell.drivers.<name>]
socket_path`) at v0.1.6 and at the vendored commit.

## Troubleshooting

| Symptom | Cause and fix |
|---|---|
| `check` reports `FAIL ... /dev/kvm` | `/dev/kvm` is not read/write for the user, or KVM is missing ([Host setup](#4-host-setup)) |
| `check` reports a user-namespace error | `user.max_user_namespaces` is 0 or unprivileged namespaces are disabled |
| `write /proc/self/setgroups: Permission denied` | AppArmor restricts user namespaces (Ubuntu 23.10 and later). No sudo needed at run time: install the AppArmor profile ([Host setup](#4-host-setup)) |
| `Firecracker ... is too old` | Install Firecracker 1.16.1 or newer ([Install](#2-install-firecracker)) |
| Driver exits with `OPENSHELL_VM_LAUNCHER_BIN is required` | Set it in the driver's environment file ([Configure](#5-configure)). Make sure the service runs the vendored driver install.sh installs, not OpenShell's packaged one |
| Workload starts in `/`, or lacks the image's variables | The image has no `WORKDIR`/`ENV`, or its cached disk predates this driver (it is rebuilt once). See [Image ENV and WORKDIR](#image-env-and-workdir) |
| Gateway cannot connect to the socket | Socket under `/tmp` (hidden by `PrivateTmp`), or the driver runs as a different UID |
| Sandbox create fails after a driver restart | Restart the gateway: the admission acknowledgement changed |
| Create fails mentioning launch authentication | The gateway has no `gateway_jwt` signing bundle |
| Guest hangs or panics at boot | Read `rootfs-console.log`. The release kernel is built by `kernel/build.sh`, which fails if a required option is missing; a kernel you supplied needs the same options |
| Handshake fails with a certificate not-yet-valid error | Guest clock; check `date` in the console log |
| Launcher says the guest environment file was not written | `debugfs` missing or the overlay path has spaces or quotes |

## Development

The toolchain and tasks are managed with [mise](https://mise.jdx.dev/), as in OpenShell.
`mise install` installs the pinned Rust (1.95.0, with rustfmt and clippy) and writes
`mise.lock`; commit that file. `mise tasks` lists everything.

```shell
mise run rust:format          # cargo fmt --all
mise run ci                   # format check, clippy (deny warnings), tests
mise run package:launcher     # release build, packaged into dist/
```

CI runs the same tasks, so a local run and a workflow run do the same thing.
`.github/workflows/ci.yml` runs `rust:format:check`, `rust:lint` and `rust:test`, and
`driver:ci`, on every push and pull request, then builds the launcher. The tests need no KVM; the one that
writes into an ext4 image skips itself when `e2fsprogs` is missing.

The vendored driver is its own cargo workspace (`vendor/openshell`), so its checks are
separate tasks: `driver:ci` runs its format check, clippy (deny warnings) and tests, and is
part of `ci`. `driver:package` stages the runtime, builds the driver and packages it.
`kernel:package` builds the guest kernel (`kernel/build.sh`; it needs a C toolchain).
Releases are built by `.github/workflows/release.yml` when a `v*` tag is pushed; it calls `package:launcher`,
`kernel:package`, `driver:package` and `package:assets`.
