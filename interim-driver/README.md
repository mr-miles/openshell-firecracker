# Interim driver (temporary)

**This directory is a stopgap. Delete it when the upstream launcher override ships.**

## What it is

A copy of the entry point of OpenShell's VM driver (`crates/openshell-driver-vm/src/main.rs`)
with one change: it accepts `--launcher-bin` / `OPENSHELL_VM_LAUNCHER_BIN` and passes it to
`VmDriverConfig::launcher_bin`, which the upstream binary always leaves unset. That lets the
driver run the Firecracker launcher in the parent directory for every VM.

Everything else is OpenShell's code, used unmodified: this crate links `openshell-driver-vm`,
`openshell-core` and `openshell-otel` from the OpenShell repository, pinned to one commit.
The rest of the plugin (the launcher, the jail, the bridge, the kernel config) neither
depends on this directory nor knows about it.

It is a copy of one file rather than of the whole driver because `openshell-driver-vm` has
path dependencies on about a dozen other workspace crates and does not build on its own.

## Why it exists

The upstream request is: *let the VM driver run a configurable launcher executable*
(`../upstream/launcher-override.md`). Until a release contains it, the installed
`openshell-driver-vm` cannot be pointed at the plugin.

## Releases

Plugin releases ship a pre-built driver, built against the **latest OpenShell release** at
the time (`openshell-version.txt` in the release names it). `install.sh` installs it and
warns if your gateway is a different version. Only the latest OpenShell release is covered.

## Build it yourself

For another OpenShell release, set the three `tag = "..."` entries in `Cargo.toml` to your
release's tag, then:

```shell
cd interim-driver
mise run vm:setup && mise run vm:supervisor   # in an OpenShell checkout of that tag
OPENSHELL_VM_RUNTIME_COMPRESSED_DIR=<openshell>/target/vm-runtime-compressed \
  cargo build --release
sudo install -D target/release/openshell-driver-vm-interim \
  /usr/local/libexec/openshell-firecracker/openshell-driver-vm
```

The runtime artifacts (host supervisor, guest sandbox, `umoci`, libkrun) are embedded at
build time by `openshell-driver-vm`'s build script, so the build needs them from the same
release. Without `OPENSHELL_VM_RUNTIME_COMPRESSED_DIR` the build succeeds with empty stubs
and the resulting driver cannot boot VMs. CI builds that way.

The driver must come from the same release as the gateway: its embedded supervisor has to
speak that gateway's protocol.

## Licence

`src/main.rs` is derived from NVIDIA's `main.rs` and keeps its Apache-2.0 header and NVIDIA
copyright notice. The rest of this repository does not carry that notice.

## Removing it

When the OpenShell release you run includes `OPENSHELL_VM_LAUNCHER_BIN`:

1. Delete this directory and `.github/workflows/interim-driver.yml`.
2. In `examples/openshell-driver-firecracker.service`, point `ExecStart` at the packaged
   `openshell-driver-vm` instead of the interim build.
3. In the top-level `README.md`, delete the "Interim driver" paragraph under
   "One upstream change is required".

Nothing else changes: the environment variables and the launcher are the same.
