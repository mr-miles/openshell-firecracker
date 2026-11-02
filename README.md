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

**Interim driver.** Until a release contains that option, [`interim-driver/`](interim-driver/)
builds a driver that has it: a copy of the VM driver's entry point with the one change,
linking OpenShell's own crates unmodified. It is isolated from the rest of the plugin and
is removed by deleting that directory and its workflow; see its README. Everything else is
in the rest of this repository.

## Requirements

- Linux with `/dev/kvm` readable and writable by the driver's user.
- Unprivileged user namespaces (`sysctl user.max_user_namespaces` above 0).
- Firecracker 1.4 or newer.
- A guest kernel built for Firecracker ([Guest kernel](#guest-kernel)).
- `e2fsprogs` (`debugfs`, `mke2fs`): the launcher writes the guest environment into the
  overlay disk with `debugfs`, and the stock driver already needs both.
- An installed OpenShell **v0.1.x** release (the host-supervisor protocol). The older
  v0.0.x line runs the supervisor inside the guest and needs a guest NIC; this plugin
  does not support it.
- The gateway needs a `gateway_jwt` signing bundle, or sandbox creation fails: the
  driver requires launch authentication.

## Install

**From a release (no build toolchain).** Each release carries a pre-built launcher, an
interim driver built against the latest OpenShell release (`openshell-version.txt` names
it), the example units, checksums, and an installer. Download `install.sh` from the release,
read it, then:

```shell
sudo ./install.sh --binaries-only     # executables, under /usr/local/libexec/openshell-firecracker
./install.sh --config-only            # your user's systemd units and env file
./install.sh --dry-run                # show what it would do, change nothing
```

It verifies every download against `SHA256SUMS`, never overwrites a file you have edited
(it writes `<file>.new` beside it), and warns if your gateway is not the OpenShell release
the driver was built for. It needs only `bash`, `curl`, `tar` and `sha256sum`. It does not
install Firecracker or build the guest kernel, and it does not edit `gateway.toml`; it
prints what is left to do. Use `--launcher-only` once your OpenShell release has
`OPENSHELL_VM_LAUNCHER_BIN`. The assets are built for x86_64 Linux only.

The rest of this section builds from source instead, for another OpenShell release or
architecture.

1. **Build and install the launcher** (Rust 1.95; see `rust-toolchain.toml`):

   ```shell
   cargo build --release
   sudo install -D target/release/openshell-firecracker-launcher \
     /usr/local/libexec/openshell-firecracker/openshell-firecracker-launcher
   ```

2. **Build the interim driver** ([`interim-driver/`](interim-driver/README.md)) from your
   release. The driver must come from the same release as the gateway: its embedded host
   supervisor and guest runtime have to speak the gateway's protocol. Set the `rev` in
   `interim-driver/Cargo.toml` to your release's commit or tag, stage the runtime artifacts
   in an OpenShell checkout of that tag, then:

   ```shell
   openshell-gateway --version                 # e.g. 0.1.6
   cd interim-driver
   OPENSHELL_VM_RUNTIME_COMPRESSED_DIR=<openshell>/target/vm-runtime-compressed \
     cargo build --release
   sudo install -D target/release/openshell-driver-vm-interim \
     /usr/local/libexec/openshell-firecracker/openshell-driver-vm
   ```

   Install it under its own directory so it cannot collide with the packaged driver. Once
   your OpenShell release has `OPENSHELL_VM_LAUNCHER_BIN`, use the packaged
   `openshell-driver-vm` instead and delete `interim-driver/`.

3. **Install Firecracker and build the guest kernel.**

4. **Configure and start the driver**, then the gateway ([Connect](#connect-to-the-installed-gateway)).

5. **Check the host.** With the same environment as the service:

   ```shell
   openshell-firecracker-launcher check
   ```

   It reports Firecracker's version, KVM access, whether the VMM jail can be built,
   `debugfs`/`mke2fs`, and sub-uid support, and exits nonzero on any failure. The example
   unit runs it before the driver starts.

## Guest kernel

Firecracker needs an uncompressed `vmlinux` (x86_64) or `Image` (aarch64) with virtio-mmio,
not PCI. Start from Firecracker's microvm CI config for your kernel version
(`resources/guest_configs` in the Firecracker repository), then merge
[`kernel/openshell-firecracker.kconfig`](kernel/openshell-firecracker.kconfig) and
OpenShell's own `crates/openshell-driver-vm/runtime/kernel/openshell.kconfig` (Landlock,
seccomp, cgroups):

```shell
cd linux-6.12.x
cp <firecracker>/resources/guest_configs/<microvm-config-for-your-version> .config
./scripts/kconfig/merge_config.sh -m .config \
  <plugin>/kernel/openshell-firecracker.kconfig \
  <openshell>/crates/openshell-driver-vm/runtime/kernel/openshell.kconfig
make olddefconfig && make -j"$(nproc)" vmlinux
```

Assert the result. A missing symbol shows up as a silent hang or a panic at boot:

```shell
cat <plugin>/kernel/openshell-firecracker.kconfig \
    <openshell>/crates/openshell-driver-vm/runtime/kernel/openshell.kconfig \
  | grep -E '^CONFIG_[A-Z0-9_]+=y$' \
  | while read -r line; do grep -qx "$line" .config || echo "MISSING $line"; done
```

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
| `OPENSHELL_VM_FIRECRACKER_BIN` | Absolute path to Firecracker |
| `OPENSHELL_VM_FIRECRACKER_KERNEL` | Absolute path to the guest `vmlinux` |
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
| Config builder, jail arguments, console retention, bridge, argument parsing, leases | Unit tests written; not yet compiled or run in this repository |
| `cargo clippy` / `rustfmt`, CI workflow | Written; CI has not run |
| Jail on a real host (`unshare`, `pivot_root`, mount flags, PID namespace, sub-uid mapping) | Not run |
| Boot of a real VM, vsock handshake through the bridge, guest clock | Not run (no KVM available when written) |
| Stock driver accepting `OPENSHELL_VM_LAUNCHER_BIN` | Not implemented upstream (issue filed). `interim-driver/` provides it meanwhile; it is a copy of the driver's entry point written without compiling it |
| Whether the image-preparation worker is started through the launcher | Not checked; the plugin hands it to the stock driver either way |
| Conformance suite | Not run |

Confirmed from the OpenShell repository, not by running anything: the launcher flag
contract, the `launcher_bin` resolution, the `RuntimeInitiatedUnix` transport, the `0666`
supervisor control socket, and the external-driver registration keys (`compute_driver`,
`[openshell.drivers.<name>] socket_path`) at v0.1.6.

## Troubleshooting

| Symptom | Cause and fix |
|---|---|
| `check` reports `FAIL ... /dev/kvm` | `/dev/kvm` is not read/write for the user, or KVM is missing |
| `check` reports a user-namespace error | `user.max_user_namespaces` is 0 or unprivileged namespaces are disabled |
| `Firecracker ... is too old` | Install Firecracker 1.4 or newer |
| Driver ignores the plugin and boots libkrun | The driver build lacks `OPENSHELL_VM_LAUNCHER_BIN` (see `upstream/`) |
| Gateway cannot connect to the socket | Socket under `/tmp` (hidden by `PrivateTmp`), or the driver runs as a different UID |
| Sandbox create fails after a driver restart | Restart the gateway: the admission acknowledgement changed |
| Create fails mentioning launch authentication | The gateway has no `gateway_jwt` signing bundle |
| Guest hangs or panics at boot | A kernel option is missing; run the symbol check and read `rootfs-console.log` |
| Handshake fails with a certificate not-yet-valid error | Guest clock; check `date` in the console log |
| Launcher says the guest environment file was not written | `debugfs` missing or the overlay path has spaces or quotes |

## Development

```shell
cargo fmt --all
cargo clippy --all-targets -- -D warnings
cargo test --all-targets
```

CI (`.github/workflows/ci.yml`) runs rustfmt, clippy with warnings denied, the unit tests,
and a release build on every push and pull request. The tests need no KVM; the one that
writes into an ext4 image skips itself when `e2fsprogs` is missing.
