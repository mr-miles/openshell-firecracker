# Interim driver (temporary)

**This directory is a stopgap. Delete it when the upstream launcher override ships.**

## What it is

OpenShell's VM driver already has a `launcher_bin` setting (the executable it runs for each
VM); its `main` just always leaves it unset. The plugin needs that setting reachable, so
until OpenShell offers `OPENSHELL_VM_LAUNCHER_BIN`, the plugin builds OpenShell's own driver
with a small patch that adds it.

Nothing of OpenShell's is copied into this repository. `apply-launcher-override.sh` edits
two places in a checkout's `crates/openshell-driver-vm/src/main.rs`, found by anchor:

1. a `launcher_bin` argument (`OPENSHELL_VM_LAUNCHER_BIN`) added at the top of `struct Args`;
2. `launcher_bin: None,` changed to `launcher_bin: args.launcher_bin.clone(),`.

It fails, changing nothing, if either anchor is missing or repeated, and does nothing if the
option is already there. Because it patches the release's own file, it works on whichever
release is checked out. An earlier version copied one release's `main.rs` and broke as soon as
the pinned release and the copy disagreed.

## Releases

Plugin releases ship a pre-built driver, built against the **latest OpenShell release** at
the time (`openshell-version.txt` in the release names it). `install.sh` installs it and warns
if your gateway is a different version. Only the latest OpenShell release is covered.

## Build it yourself

You need `mise`, `gh` and `git`. In the plugin repository:

```shell
OPENSHELL_TAG=v0.1.6 mise run interim:package     # or omit OPENSHELL_TAG for the latest release
```

This resolves the release, clones it into `./openshell`, applies the patch, stages OpenShell's
runtime artifacts (host supervisor, guest sandbox and init, `umoci`, libkrun) with OpenShell's
own `vm:setup` and `vm:supervisor` tasks, builds `openshell-driver-vm`, and writes
`dist/openshell-driver-vm-interim-<arch>-linux.tar.gz`. Install the binary inside it as
`openshell-driver-vm` under its own directory (`install.sh` does this), so it cannot collide
with a packaged driver.

The driver must come from the same release as the gateway: its embedded host supervisor has to
speak that gateway's protocol.

To check just the patch, with no OpenShell checkout: `mise run interim:test`.

## Removing it

When the OpenShell release you run includes `OPENSHELL_VM_LAUNCHER_BIN`:

1. Delete this directory, `tasks/interim.toml` and `.github/workflows/interim-driver.yml`.
2. In `.github/workflows/release.yml`, delete the `driver` job and remove it from
   `publish`'s `needs`.
3. In `install.sh`, delete the lines marked `INTERIM` (the driver asset and the version
   warning); `--launcher-only` becomes the only behavior.
4. In `examples/openshell-driver-firecracker.service`, point `ExecStart` at the packaged
   `openshell-driver-vm` instead of the interim build.
5. In the top-level `README.md`, delete the "Interim driver" paragraph under
   "One upstream change is required".

Nothing else changes: the environment variables and the launcher are the same.
