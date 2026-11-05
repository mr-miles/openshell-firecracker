# openshell-driver-vm (vendored)

This is a trimmed copy of OpenShell's VM compute driver, taken from NVIDIA/OpenShell
commit `8d0797a`, for use with the Firecracker launcher in the repository root. It is the
control plane only: gRPC, images, overlays, launch authentication, the host supervisor and
the lifecycle-extension hooks. It starts no VMM itself; every VM is booted by the launcher
named in `OPENSHELL_VM_LAUNCHER_BIN`.

Changes from upstream:

- **Image `ENV` and `WORKDIR`.** They are read from the image configuration (registry
  images and local Docker images), stored as `image-config.json` beside the cached disk,
  and applied to the workload. The environment is layered image, then template, then spec.
  The workdir is created in the guest, handed to the sandbox user, and passed to the
  supervisor as `--workdir`, which also makes it writable under the filesystem policy. An
  image with no `WORKDIR` starts in `/` and the driver warns, unless
  `OPENSHELL_VM_DEFAULT_WORKDIR` supplies one. See `src/image_config.rs`.
- **Launcher option.** `OPENSHELL_VM_LAUNCHER_BIN` selects the launcher executable.
- **Removed.** libkrun and its embedded runtime, QEMU, GPU passthrough and `openshell-vfio`,
  virtiofs bind mounts, and the backend negotiation in the lifecycle extension API. A
  sandbox that asks for a GPU or bind mounts is rejected.

Kept: Docker/Podman and registry image sources, rootfs tar uploads, telemetry.
