# Upstream change: let the VM driver use a different launcher

The Firecracker plugin needs exactly one thing from the OpenShell VM driver: a way
to name the executable it runs for each VM. This page is the proposal to send
upstream. Until it is merged, build `openshell-driver-vm` from your release tag with
the two-line change below.

## Why

`VmDriver` starts every sandbox VM, and the image-preparation VM, by running a
*launcher* with `--internal-run-vm` and a fixed set of flags. The launcher is
`VmDriverConfig::launcher_bin`, and when that is unset the driver uses its own
executable (`std::env::current_exe()` in `VmDriver::new_with_extensions`).

The stock `main` always passes `launcher_bin: None`, so the shipped binary cannot be
pointed at another launcher. Everything else needed to run a different VMM already
exists: the field, the `--internal-run-vm` flag contract, and the per-VM console and
state paths. A different VMM can then be added as a standalone launcher, with no
OpenShell crates linked and no fork of the driver.

## Change

In `crates/openshell-driver-vm/src/main.rs`, add an operator option:

```rust
    /// VM launcher executable the driver runs for every VM. Defaults to this
    /// binary. The launcher receives the driver's `--internal-run-vm` flags.
    #[arg(long, env = "OPENSHELL_VM_LAUNCHER_BIN")]
    launcher_bin: Option<PathBuf>,
```

and use it where the config is built (currently `launcher_bin: None,`):

```rust
        launcher_bin: args.launcher_bin.clone(),
```

Validate it with the other startup checks, so a bad path fails the driver at start
rather than on the first sandbox:

```rust
if let Some(path) = args.launcher_bin.as_deref()
    && (!path.is_absolute() || !path.is_file())
{
    return Err(miette::miette!(
        "launcher_bin must be an absolute path to an existing file: {}",
        path.display()
    ));
}
```

## Trust

The option is read from the driver's own command line and environment, which only the
operator controls, like `--state-dir` and the other driver options. A launcher runs as
the driver's user with the driver's privileges, so naming one is as sensitive as
choosing which driver binary to run. It is not reachable from a sandbox spec or the
gateway API, and it is not part of the gateway-to-driver protocol.

## Compatibility

Unset, behavior is unchanged. A launcher must accept the driver's flags
(`--internal-run-vm` plus the `--vm-*` set) and the other `--internal-*` modes the
driver may run through it, or hand them back to the stock binary. The Firecracker
plugin does the latter by running the executable of its parent process.

## Tests to add upstream

- The option defaults to the driver's own executable.
- A set path reaches `VmDriverConfig::launcher_bin`.
- A relative or missing path is rejected at startup.

## Alternatives considered

- A `[openshell.drivers.vm]` gateway key. The gateway spawns the VM driver with a
  fixed argument list, so this would add gateway code and a config field for what is a
  driver-local choice. The environment variable also works for the externally run
  driver the plugin uses.
- Selecting Firecracker inside the VM driver. This is the in-tree approach in
  RFC 0012 and #2934, and it couples the driver to one more VMM. The launcher hook keeps
  VMMs out of the driver.
