# OpenShell Firecracker compute driver (plugin)

This is a plugin, not part of the OpenShell source tree. It adds Firecracker as
a VMM for sandboxes and runs next to an installed, compiled OpenShell release as
an **external compute driver**. The installed gateway is not rebuilt or modified.

How it connects, in one paragraph: you build `openshell-driver-vm` from the OpenShell
tag that matches your installed release, with the Firecracker patch applied. You run
that binary yourself as a separate service, listening on a Unix socket. You tell the
installed gateway about it with two lines in `gateway.toml`. The gateway does not
start, stop, or supervise it.

> Status: compiled and unit-tested on the driver crate only. Nothing here has booted
> a VM, run the conformance suite, or been exercised against an installed gateway.
> See [Verification status](#verification-status).

## What the plugin changes

The plugin is a patch to `crates/openshell-driver-vm` (base: OpenShell `main` at
`8d0797a`). It touches no gateway, server, or docs file.

| Path | Change |
|---|---|
| `src/firecracker/` (new) | VMM config builder, rootless jail, console capture, launcher |
| `src/runtime.rs` | `VmBackend::Firecracker`, `VmHypervisor`, dispatch |
| `src/driver.rs` | Hypervisor selection, control socket and transport, image-prep, cleanup, startup checks |
| `src/main.rs` | Pre-runtime jail hook; `--hypervisor`, `--firecracker-bin`, `--firecracker-kernel` |
| `src/lib.rs` | Module and export wiring |
| `runtime/kernel/openshell-firecracker.kconfig` (new) | Guest kernel options |

Everything else (policy, authentication, image preparation, Stop/Start generations,
the host supervisor, the guest init) is the stock VM driver. Firecracker replaces
only how the VM is launched. GPU sandboxes still use QEMU.

Export the patch from the checkout (the working tree is uncommitted):

```shell
cd OpenShell
git add -N crates/openshell-driver-vm
git diff -- crates/openshell-driver-vm > ../firecracker-plugin/patches/0001-firecracker-vmm.patch
```

## Requirements

- Linux with `/dev/kvm` readable and writable by the driver's user.
- Unprivileged user namespaces: `sysctl user.max_user_namespaces` greater than 0.
- Firecracker 1.4 or newer. The driver rejects older versions at startup.
- A guest kernel built for Firecracker (see [Guest kernel](#guest-kernel)).
- An installed OpenShell release with a gateway you can restart.
- The gateway needs a `gateway_jwt` signing bundle. The driver requires
  launch-authentication, and sandbox creation fails without it. Package-managed
  local TLS setup provides one.

## Build against an installed release

The driver must come from the same release as the gateway. The host supervisor and
guest runtime embedded in the driver have to speak the installed gateway's protocol,
and the driver's caches are keyed on the OpenShell version. Negotiation itself checks
only the protocol major version, so a mismatch fails later, at supervisor connect.

```shell
openshell-gateway --version                      # note the version, e.g. 0.x.y
git clone https://github.com/NVIDIA/OpenShell && cd OpenShell
git checkout v<that version>
git apply ../firecracker-plugin/patches/0001-firecracker-vmm.patch

mise run vm:setup          # stages the runtime artifacts (downloads the vm-runtime release)
mise run vm:supervisor     # builds the guest sandbox and init
OPENSHELL_VM_RUNTIME_COMPRESSED_DIR=$PWD/target/vm-runtime-compressed \
  cargo build --release -p openshell-driver-vm
```

The build needs these six files in `OPENSHELL_VM_RUNTIME_COMPRESSED_DIR`:
`libkrun.so.zst`, `libkrunfw.so.5.zst`, `openshell-sandbox.zst`,
`openshell-supervisor.zst`, `openshell-vm-init.zst`, `umoci.zst`. Use the official
runtime tarball even though you will run Firecracker: the build expects the libkrun
files, and it is unverified whether a Firecracker-only run can do without them.

Install the binary under its own directory so it cannot collide with the packaged
`openshell-driver-vm`:

```shell
sudo install -D target/release/openshell-driver-vm \
  /usr/local/libexec/openshell-firecracker/openshell-driver-vm
```

## Guest kernel

Firecracker needs an uncompressed `vmlinux` (x86_64) or `Image` (aarch64) with
virtio-mmio, not PCI. Start from Firecracker's microvm CI config for your kernel
version (`resources/guest_configs` in the Firecracker repository), then merge
`openshell-firecracker.kconfig` and the stock
`crates/openshell-driver-vm/runtime/kernel/openshell.kconfig` (Landlock, seccomp,
cgroups):

```shell
cd linux-6.12.x
cp <firecracker>/resources/guest_configs/microvm-kernel-ci-x86_64-6.1.config .config
./scripts/kconfig/merge_config.sh -m .config \
  ../OpenShell/crates/openshell-driver-vm/runtime/kernel/openshell-firecracker.kconfig \
  ../OpenShell/crates/openshell-driver-vm/runtime/kernel/openshell.kconfig
make olddefconfig && make -j"$(nproc)" vmlinux
```

Assert the result. A missing symbol shows up as a silent hang or a panic at boot:

```shell
for f in openshell-firecracker openshell; do
  grep -E '^CONFIG_[A-Z0-9_]+=y$' ../OpenShell/crates/openshell-driver-vm/runtime/kernel/$f.kconfig
done | while read -r line; do grep -qx "$line" .config || echo "MISSING $line"; done
```

Compare against the kernel the Firecracker CI config targets; the `microvm-kernel-ci`
file name above is an example and depends on the version you build.

## Connect it to the installed gateway

1. **Pick a driver name** that is not a built-in: use `firecracker`. The stock gateway
   registers `docker`, `kubernetes`, `podman`, and `vm`; the docs say built-in names
   cannot be selected through an unmanaged socket.
2. **Run the driver as the gateway's user.** The gateway does no authentication of its
   own; the driver checks the peer. The gateway's pid is not known in advance, so run the
   driver with `--allow-same-uid-peer`, as the same UID as the gateway. The packaged
   gateway is a user service, so this is a user unit.
3. **Give the socket a dedicated directory.** The driver requires the socket's parent
   directory to be owned by its user and not a symlink, and it sets it to `0700`. Use
   `$XDG_RUNTIME_DIR/openshell-firecracker/`. Do not use `/tmp`: the packaged unit has
   `PrivateTmp=true`, so the gateway cannot see sockets there.
4. **Register it** in `~/.config/openshell/gateway.toml` (the path for the deb; other
   packages differ, see [Install layouts](#install-layouts)):

   ```toml
   [openshell.gateway]
   compute_driver = "firecracker"

   [openshell.drivers.firecracker]
   socket_path = "/run/user/1000/openshell-firecracker/driver.sock"
   ```

   `socket_path` is the only key the gateway reads from this table, apart from
   `allow_driver_config` and `resource_admission`, which it reads for admission.
   `[openshell] version = 2` must be present. Put `guest_tls_ca` under
   `[openshell.gateway]`; the gateway rejects `guest_tls_*` keys in a driver table.
   Alternatively set `OPENSHELL_COMPUTE_DRIVER=firecracker` and
   `OPENSHELL_COMPUTE_DRIVER_SOCKET=<path>` in `~/.config/openshell/gateway.env`.
5. **Start the driver before the gateway**, and restart the gateway whenever the driver
   restarts. The gateway reads the driver's capabilities at startup and again before every
   validate, create, and start. If the driver's admission acknowledgement changed, those
   calls fail until the gateway restarts.

Example units are in `examples/`. Copy them to `~/.config/systemd/user/` and the
gateway drop-in to `~/.config/systemd/user/openshell-gateway.service.d/`.

### What you must pass to the driver yourself

A gateway-spawned `vm` driver is given its configuration on its command line. A
plugin gets it from flags and environment. Set these in
`~/.config/openshell/firecracker-driver.env`:

| Variable | Purpose |
|---|---|
| `OPENSHELL_GRPC_ENDPOINT` | **Required.** URL the host supervisor uses to reach the gateway |
| `OPENSHELL_VM_TLS_CA` | **Required** when the endpoint is `https://`: the gateway CA |
| `OPENSHELL_VM_DRIVER_STATE_DIR` | Per-sandbox state and caches. Set it; the default is relative |
| `OPENSHELL_VM_BOOTSTRAP_IMAGE` / `OPENSHELL_SANDBOX_IMAGE` | At least one must be set |
| `OPENSHELL_VM_HYPERVISOR` | `firecracker` |
| `OPENSHELL_VM_FIRECRACKER_BIN` | Absolute path to Firecracker |
| `OPENSHELL_VM_FIRECRACKER_KERNEL` | Absolute path to the guest `vmlinux` |
| `OPENSHELL_DRIVER_ADMISSION_CONFIG_JSON` | Must match the gateway's `allow_driver_config` and `resource_admission` for this driver. `{}` means defaults on both sides |
| `OPENSHELL_LOG_LEVEL`, `OPENSHELL_VM_DRIVER_VCPUS`, `OPENSHELL_VM_DRIVER_MEM_MIB`, `OPENSHELL_VM_OVERLAY_DISK_MIB`, `OPENSHELL_VM_SANDBOX_UID`, `OPENSHELL_VM_SANDBOX_GID` | Optional tuning |

The gateway passes its own proxy settings and gateway name to a managed driver. A
plugin must set the equivalent `OPENSHELL_VM_UPSTREAM_PROXY*` and
`OPENSHELL_GATEWAY_NAME` itself.

### What does not work for an external driver

- `[openshell.supervisor.network].additional_ca_bundle`: the gateway rejects it with
  any remote driver, and the hidden flags that carry it cannot be set from outside.
- The shared supervisor pool (`[openshell.gateway.supervisor_pool]`).
- Rootfs tar uploads need the driver and gateway on the same filesystem and UID.
- `openshell-gateway config preflight` skips host-tool checks for a remote driver. Check
  for `mke2fs` and `debugfs` (e2fsprogs) yourself.
- The VM driver does not advertise `isolation-backend` or `suspend-resume`, and does not
  implement `AuthenticateSandbox`. Those gateway features do not apply.

## Install layouts

Confirmed from the repository for the **deb**: `/usr/bin/openshell-gateway`,
`/usr/libexec/openshell/openshell-driver-vm`, the user unit
`/usr/lib/systemd/user/openshell-gateway.service` (with `PrivateTmp=true` and
`UMask=0077`), config in `~/.config/openshell/gateway.toml`, and an optional
`~/.config/openshell/gateway.env`.

Not verified: the rpm, snap, and Homebrew layouts. The docs put rpm config in
`~/.config/openshell/gateway.toml`, Homebrew in
`$XDG_CONFIG_HOME/openshell/gateway.toml` or `/opt/homebrew/var/openshell/gateway.toml`,
and snap in `~/snap/openshell/common/.config/openshell/gateway.toml`. Snap confinement
may block an external socket. Firecracker is Linux-only, so Homebrew does not apply.

## Verification status

| Check | Status |
|---|---|
| Config builder, jail argument handling, console retention (unit tests) | Passed before the latest changes; the clock, version-parse and console changes are not yet compiled or run |
| `cargo clippy` / `rustfmt` on the driver | Clean before the latest changes; not re-run since |
| Jail on a real host (`unshare`, `pivot_root`, mount flags, PID namespace) | Not run |
| Boot of a real VM, vsock handshake, image preparation | Not run (no KVM available) |
| Conformance suite | Not run |
| Connection to an installed gateway | Not run. The steps above come from reading the repository |

## Known limits

- No per-VM uid. Every VMM runs as the driver's user. Each is in its own user, mount,
  network, IPC, UTS, and PID namespace, so a compromised VMM cannot see or signal the
  driver or other VMMs. Firecracker's `jailer` is not used.
- Stop kills the VMM without a guest flush, as libkrun does. The overlay uses `Writeback`
  caching so fsyncs reach the host file, and ext4 stays crash-consistent.
- No virtiofs bind mounts and no GPU passthrough on Firecracker.
- The kernel and Firecracker paths are checked when the driver starts, not pinned by hash
  per boot.
- aarch64 is untested.

## Troubleshooting

| Symptom | Cause and fix |
|---|---|
| Driver exits at start with `host cannot confine Firecracker` | User namespaces are disabled, or `/dev/kvm` is not read/write for the user |
| `unusable firecracker_bin: ... too old` | Install Firecracker 1.4 or newer |
| Gateway cannot connect to the socket | Socket under `/tmp` (hidden by `PrivateTmp`), or the driver runs as a different UID |
| Sandbox create fails after a driver restart | Restart the gateway: the admission acknowledgement changed |
| Create fails mentioning launch authentication | The gateway has no `gateway_jwt` signing bundle |
| Guest hangs or panics at boot | A kernel option is missing; run the symbol check above and read `rootfs-console.log` |
| Handshake fails with a certificate not-yet-valid error | Guest clock; check `date` in the console log |
