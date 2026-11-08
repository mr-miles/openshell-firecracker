# openshell-driver-vm (vendored)

This is a trimmed copy of OpenShell's VM compute driver, taken from NVIDIA/OpenShell
at the revision in `driver/UPSTREAM.env` in the repository root, for use with the Firecracker
launcher there. Only this crate is copied; the OpenShell crates it depends on are git
dependencies on the same revision (declared once, in the workspace `Cargo.toml`). It is the
control plane only: gRPC, images, overlays, launch authentication, the host supervisor and
the lifecycle-extension hooks. It starts no VMM itself; every VM is booted by the launcher
named in `OPENSHELL_VM_LAUNCHER_BIN`.

Changes from upstream:

- **Image `ENV` and `WORKDIR`.** They are read from the image configuration (registry
  images and local Docker images), stored as `image-config.json` beside the cached disk,
  and applied to the workload. The environment is layered image, then template, then spec.
  The workdir is created in the guest, handed to the sandbox user, and passed to the
  supervisor as `--workdir`, which also makes it writable under the filesystem policy. An
  image with no `WORKDIR` starts in `OPENSHELL_VM_DEFAULT_WORKDIR` (default `/sandbox`);
  an image `WORKDIR` elsewhere gets the default as a symlink to it, and the sandbox
  account OpenShell creates has the workdir as its home. See `src/image_config.rs`.
- **Launcher option.** `OPENSHELL_VM_LAUNCHER_BIN` selects the launcher executable.
- **Supervisor identity** (off by default, `OPENSHELL_VM_SUPERVISOR_IDENTITY`). The driver
  serves the JWT half of the SPIFFE Workload API on a socket in each sandbox's state
  directory, starts the host supervisor from a hard link of its binary there, passes the
  socket in `OPENSHELL_PROVIDER_SPIFFE_WORKLOAD_API_SOCKET`, and issues JWT-SVIDs only to
  that attested process. It serves OIDC discovery and JWKS endpoints over HTTP. The server
  stub is generated in `build.rs` without `protoc`. See `src/supervisor_identity/` and
  "Supervisor identity" in the repository README.
- **Per-sandbox disk size and files.** `driver_config.overlay_disk_mib` sets the size of a
  sandbox's writable overlay disk, up to `OPENSHELL_VM_MAX_OVERLAY_DISK_MIB`.
  `driver_config.files` copies files and directories from `OPENSHELL_VM_FILES_ROOT` into the
  guest: the driver stages them in the overlay's upper layer and a guest init drop-in copies
  them into place with the requested owner and mode. See `src/sandbox_storage.rs` and
  "Per-sandbox disk size and files" in the repository README.
- **Image cache cleanup.** Upstream never deletes a prepared disk. When the driver starts
  and after each sandbox start, stop or delete, it removes cache entries no sandbox
  references: those from other driver builds at once, and its own build's once unused for
  `OPENSHELL_VM_IMAGE_CACHE_MAX_IDLE_HOURS` (default 48; 0 turns cleanup off). Provisioning
  holds an image-use lock shared, and cleanup takes it exclusively along with
  `preparation.lock`. See `src/image_cache_gc.rs`.
- **Graceful shutdown.** On SIGTERM or Ctrl-C, `VmDriver::shutdown` refuses new launches
  and stops every running VM the way `StopSandbox` does, but without the stop marker and with
  the exit monitor detached, so the next start relaunches those sandboxes. Then watch
  streams end so the RPC server can exit. Upstream stops serving and leaves the VMs to be
  killed with the process.
- **Removed.** libkrun and its embedded runtime, QEMU, GPU passthrough and `openshell-vfio`,
  virtiofs bind mounts, and the backend negotiation in the lifecycle extension API. A
  sandbox that asks for a GPU or bind mounts is rejected.

Kept: Docker/Podman and registry image sources, rootfs tar uploads, telemetry.
