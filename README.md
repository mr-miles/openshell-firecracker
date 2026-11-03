# OpenShell Firecracker plugin

Run OpenShell sandboxes as Firecracker microVMs, next to an installed, compiled
OpenShell release. No OpenShell crate is linked and the gateway is not modified.

This is an external compute driver in the sense of
[the driver docs](https://docs.nvidia.com/openshell/dev/extensibility/drivers#compute-drivers):
a separate service listening on a Unix socket that implements `compute_driver.proto`,
which the gateway does not start or supervise. The service is the stock
`openshell-driver-vm`; this plugin replaces only the program it runs to boot each VM.

> Status: written and unit-tested in parts, but not yet built end to end, and never
> booted against a real VM or an installed gateway. See
> [Verification status](#verification-status).

## How it works

```
 openshell-gateway ──gRPC over Unix socket──▶ openshell-driver-vm   (stock; external service)
                                                  │ for every VM: run the launcher
                                                  ▼
                                        openshell-firecracker-launcher   (this plugin)
                                                  │ jailed, rootless
                                                  ▼
 host supervisor ◀──── bridge ◀──── Firecracker ◀──vsock── guest (openshell-sandbox)
```

- The stock driver does everything it does today: images, overlays, Stop/Start
  generations, launch authentication, the host supervisor, policy. It believes it is
  running libkrun VMs.
- For each VM it runs its launcher with `--internal-run-vm` and a fixed set of flags.
  This plugin is that launcher. It boots the guest under Firecracker instead.
- Requests the plugin does not handle (the image-preparation worker, QEMU for GPU
  sandboxes) are handed back to the stock driver binary, found as the launcher's parent
  process.
- Guest to host traffic goes over vsock. Firecracker connects to a socket the launcher
  listens on, and the launcher relays to the supervisor. The supervisor's socket never
  enters the VMM jail.

The guest has no network device.

## One upstream change is required

The stock driver cannot yet be told to use a different launcher: its `main` always uses
its own executable. [`upstream/launcher-override.md`](upstream/launcher-override.md) is
the two-line change that adds `OPENSHELL_VM_LAUNCHER_BIN`, with the reasoning sent
upstream.

**Interim driver.** Until a release contains that option, [`interim/`](interim/) builds
OpenShell's own VM driver with a small patch that adds it. Nothing of OpenShell's is copied
here: the patch is applied to a checkout of the release at build time, by anchor, so it
matches whichever release is checked out. It is isolated from the rest of the plugin and is
removed by deleting that directory and its task file and workflow; see its README.
Everything else is in the rest of this repository.

## Requirements

What you need, and the [Install](#install) step that provides it:

| Need | Step |
|---|---|
| An installed OpenShell **v0.1.x** release (the host-supervisor protocol). The older v0.0.x line runs the supervisor inside the guest and needs a guest NIC; this plugin does not support it | 1 |
| Firecracker 1.16.1 or newer (the first release that validates the Linux 6.18 guest kernel the plugin ships) | 2 |
| The launcher, a driver that can use it, and a guest kernel (all pre-built) | 3 |
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

Each plugin release carries pre-built assets and an installer: the launcher, a driver that
can use it (an interim build of OpenShell's own, against the latest OpenShell release;
`openshell-version.txt` names it), and a guest kernel built for Firecracker. Download
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
(it writes `<file>.new` beside it), and warns if your gateway is not the OpenShell release
the driver was built for. It needs only `bash`, `curl`, `tar` and `sha256sum`. It does not
edit `gateway.toml`; it prints what is left to do. `--no-kernel` skips the kernel (to use
your own); `--launcher-only` skips the driver, once your OpenShell release has
`OPENSHELL_VM_LAUNCHER_BIN`; `--no-subuids` skips the per-VM uid setup in
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

For another OpenShell release, another architecture, or to change the plugin. Nothing here
is needed to use a release.

- **The launcher** (Rust 1.95; see `rust-toolchain.toml`):

  ```shell
  cargo build --release
  sudo install -D target/release/openshell-firecracker-launcher \
    /usr/local/libexec/openshell-firecracker/openshell-firecracker-launcher
  ```

- **The interim driver** ([`interim/`](interim/README.md)) from your release. The driver must
  come from the same release as the gateway: its embedded host supervisor and guest runtime
  have to speak the gateway's protocol. This needs `mise`, `gh` (logged in), `git`, `zstd`,
  `curl` and `tar`; it checks out that release, patches it, stages its runtime artifacts and
  builds. It installs only Rust, zig and cargo-zigbuild, not OpenShell's full toolchain:

  ```shell
  openshell-gateway --version                 # e.g. 0.1.6
  OPENSHELL_TAG=v0.1.6 mise run interim:package
  tar -xzf dist/openshell-driver-vm-interim-$(uname -m)-linux.tar.gz -C dist
  sudo install -D dist/openshell-driver-vm-interim \
    /usr/local/libexec/openshell-firecracker/openshell-driver-vm
  ```

  Install it under its own directory so it cannot collide with the packaged driver. Once
  your OpenShell release has `OPENSHELL_VM_LAUNCHER_BIN`, use the packaged
  `openshell-driver-vm` instead and delete `interim/` (see its README).

- **The guest kernel**, which is the part that needs a C toolchain (`build-essential flex
  bison bc libelf-dev libssl-dev xz-utils`). Firecracker needs an uncompressed `vmlinux`
  with virtio-mmio, not PCI.
  [`kernel/build.sh`](kernel/build.sh) downloads the newest Linux 6.18 (checked against
  kernel.org's checksums), starts from Firecracker's own CI config for that series, merges
  [`kernel/openshell-firecracker.kconfig`](kernel/openshell-firecracker.kconfig) and
  OpenShell's `openshell.kconfig` (Landlock, seccomp, cgroups), fails if any option either
  asks for is missing from the final configuration (a missing one shows up later as a silent
  hang or a panic at boot), and packages the stripped `vmlinux`:

  ```shell
  OPENSHELL_TAG=v0.1.6 mise run kernel:package
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

The stock driver reads its own options. The plugin reads these, from the environment the
driver inherits:

| Variable | Purpose |
|---|---|
| `OPENSHELL_VM_LAUNCHER_BIN` | Read by the driver (needs the upstream change): path to `openshell-firecracker-launcher` |
| `OPENSHELL_VM_FIRECRACKER_BIN` | Optional. Absolute path to Firecracker. Default: `firecracker` beside the launcher, on `PATH`, or in `/usr/local/bin`, `/usr/bin`, `/opt/firecracker` |
| `OPENSHELL_VM_FIRECRACKER_KERNEL` | Optional. Absolute path to the guest `vmlinux`. Default: `vmlinux` beside the launcher, or in `/usr/local/libexec/openshell-firecracker`, `/var/lib/openshell-firecracker` |
| `OPENSHELL_VM_FIRECRACKER_SUBUIDS` | `auto` (default), `required`, or `off`; see [Per-VM uid](#per-vm-uid) |
| `OPENSHELL_VM_FIRECRACKER_LEASE_DIR` | Sub-uid lease directory. Defaults to `$XDG_RUNTIME_DIR/openshell-firecracker-leases` |
| `OPENSHELL_FIRECRACKER_STOCK_DRIVER` | Path of the stock driver, if it cannot be found from the launcher's parent |

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

Launchers coordinate through `flock`ed lease files, because the stock driver cannot hand
a launcher an id. The kernel releases a lease when its launcher exits, so a crash cannot
leak an id. The launcher shares only what the VMM needs: the overlay disk and a private
socket directory are group-accessible, the sandbox state directory stays `0700`, and the
VMM reaches its files through bind mounts.

The startup check does not exercise a boot, so a host that passes it can still fail on the
first sandbox. If that happens, set `off` to confirm the cause and read
`rootfs-console.log`.

## What does not work

- Image preparation VMs and GPU (QEMU) launches are not Firecracker: the plugin hands
  them to the stock driver. Image preparation therefore needs the stock driver's libkrun
  runtime.
- No virtiofs bind mounts and no GPU passthrough on Firecracker.
- `[openshell.supervisor.network].additional_ca_bundle` and the shared supervisor pool
  are rejected by the gateway for any external driver.
- Rootfs tar uploads need the driver and gateway on the same filesystem and UID.
- `openshell-gateway config preflight` skips host-tool checks for an external driver;
  `openshell-firecracker-launcher check` covers the Firecracker ones.
- The VM driver does not advertise `isolation-backend` or `suspend-resume`, and does not
  implement `AuthenticateSandbox`.
- Stop kills the VMM without a guest flush, as libkrun does. The overlay uses `Writeback`
  caching so fsyncs reach the host file, and ext4 stays crash-consistent.
- The Firecracker and kernel paths are checked when a VM launches, not pinned by hash.
- aarch64 is untested.

## Security notes

- Each VMM runs rootless in its own user, mount, network, IPC, UTS and PID namespaces, on
  a tmpfs root holding only Firecracker, the kernel, the sandbox's disks and VMM config,
  the vsock directory and three device nodes. It drops the capability bounding set, sets
  `no_new_privs`, disables core dumps, and keeps Firecracker's own seccomp filter.
- The guest environment file sits in the sandbox's own overlay (mode `0600`, root-owned).
  It holds what the driver passes to the guest init, the same as the QEMU backend does.
- Firecracker's `jailer` is not used.

## Verification status

| Check | Status |
|---|---|
| Config builder, argument parsing, `check`, `cargo clippy` / `rustfmt`, CI | Unit tests pass and CI runs |
| End to end on one real host | `openshell sandbox create` boots a Firecracker sandbox: the release-built Linux 6.18 guest kernel (with `# CONFIG_ACPI is not set`), the jail, image preparation, the gateway-to-driver socket and the supervisor connection all worked. Needed fixes along the way are in the git history (`/dev/fd` links, `gateway.toml` schema version and socket path) |
| Interim driver | The release's driver, built from OpenShell's release plus `interim/` (`OPENSHELL_VM_LAUNCHER_BIN`), is what runs on that host |
| Not checked on that host | The sub-uid isolation path (`OPENSHELL_VM_FIRECRACKER_SUBUIDS`), the AppArmor profile in `install.sh`, the guest clock, long-running or concurrent sandboxes, and the conformance suite |
| Other hosts | One Ubuntu-style x86_64 host only; no arm64 |

Confirmed from the OpenShell repository, not by running anything: the launcher flag
contract, the `launcher_bin` resolution, the `RuntimeInitiatedUnix` transport, the `0666`
supervisor control socket, and the external-driver registration keys (`compute_driver`,
`[openshell.drivers.<name>] socket_path`) at v0.1.6.

## Troubleshooting

| Symptom | Cause and fix |
|---|---|
| `check` reports `FAIL ... /dev/kvm` | `/dev/kvm` is not read/write for the user, or KVM is missing ([Host setup](#4-host-setup)) |
| `check` reports a user-namespace error | `user.max_user_namespaces` is 0 or unprivileged namespaces are disabled |
| `write /proc/self/setgroups: Permission denied` | AppArmor restricts user namespaces (Ubuntu 23.10 and later). No sudo needed at run time: install the AppArmor profile ([Host setup](#4-host-setup)) |
| `Firecracker ... is too old` | Install Firecracker 1.16.1 or newer ([Install](#2-install-firecracker)) |
| Driver ignores the plugin and boots libkrun | The driver build lacks `OPENSHELL_VM_LAUNCHER_BIN` (see `upstream/`) |
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
`.github/workflows/ci.yml` runs `rust:format:check`, `rust:lint` and `rust:test` on every
push and pull request, then builds the launcher. The tests need no KVM; the one that
writes into an ext4 image skips itself when `e2fsprogs` is missing.

The interim driver has `interim:*` tasks in `tasks/interim.toml`: `interim:ci` tests the
patch, and `interim:package` builds and packages the driver. `kernel:package` builds the
guest kernel (`kernel/build.sh`; it needs a C toolchain). Releases are built by
`.github/workflows/release.yml` when a `v*` tag is pushed; it calls `package:launcher`,
`kernel:package`, `interim:package` and `package:assets`.
