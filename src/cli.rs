// SPDX-License-Identifier: Apache-2.0

//! The stock VM driver's launcher command line.
//!
//! The driver runs its launcher as
//! `<launcher> --internal-run-vm --vm-root-disk ... --vm-vsock-control-socket ...`
//! and, for some work, other `--internal-*` modes. This is the subset of that
//! contract the Firecracker launcher honors. Flags that only matter to libkrun
//! (`--vm-krun-log-level`, `--vm-workdir`) are accepted and ignored.

use std::ffi::OsString;
use std::path::PathBuf;

use clap::Parser;

#[derive(Parser, Debug, Clone, PartialEq, Eq)]
#[command(name = "openshell-firecracker-launcher", disable_help_flag = true)]
pub struct LauncherArgs {
    #[arg(long)]
    pub internal_run_vm: bool,

    #[arg(long = "vm-root-disk")]
    pub vm_root_disk: Option<PathBuf>,

    #[arg(long = "vm-overlay-disk")]
    pub vm_overlay_disk: Option<PathBuf>,

    #[arg(long = "vm-image-disk")]
    pub vm_image_disk: Option<PathBuf>,

    #[arg(long = "vm-kernel-image")]
    pub vm_kernel_image: Option<PathBuf>,

    #[arg(long = "vm-exec")]
    pub vm_exec: Option<String>,

    #[arg(long = "vm-workdir", default_value = "/")]
    pub vm_workdir: String,

    #[arg(long = "vm-env")]
    pub vm_env: Vec<String>,

    #[arg(long = "vm-console-output")]
    pub vm_console_output: Option<PathBuf>,

    #[arg(long = "vm-vcpus", default_value_t = 2)]
    pub vm_vcpus: u8,

    #[arg(long = "vm-mem-mib", default_value_t = 2048)]
    pub vm_mem_mib: u32,

    #[arg(long = "vm-krun-log-level", default_value_t = 1)]
    pub vm_krun_log_level: u32,

    #[arg(long = "vm-backend")]
    pub vm_backend: Option<String>,

    #[arg(long = "vm-gpu-bdf")]
    pub vm_gpu_bdf: Option<String>,

    #[arg(long = "vm-vsock-cid")]
    pub vm_vsock_cid: Option<u32>,

    #[arg(long = "vm-vsock-control-port")]
    pub vm_vsock_control_port: Option<u32>,

    #[arg(long = "vm-vsock-control-socket")]
    pub vm_vsock_control_socket: Option<PathBuf>,

    #[arg(long = "vm-vsock-runtime-initiated")]
    pub vm_vsock_runtime_initiated: bool,

    #[arg(long = "vm-mount")]
    pub vm_mount: Vec<String>,
}

/// What the driver is asking its launcher to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request {
    /// `check`: the operator's pre-flight.
    Check,
    /// Boot the VM under Firecracker.
    RunVm(Box<LauncherArgs>),
    /// Anything the plugin does not handle: the stock driver binary runs it.
    Delegate,
}

/// Decide what `argv` (including the program name) asks for.
pub fn classify(argv: &[OsString]) -> Result<Request, String> {
    let has = |flag: &str| argv.iter().skip(1).any(|arg| arg == flag);
    if argv.get(1).is_some_and(|arg| arg == "check") {
        return Ok(Request::Check);
    }
    if has("--internal-prepare-image") {
        return Ok(Request::Delegate);
    }
    if !has("--internal-run-vm") {
        return Err(
            "this is the VM launcher the OpenShell VM driver runs; it is not started by hand \
             (see `check` for a host pre-flight)"
                .to_string(),
        );
    }
    let args = LauncherArgs::try_parse_from(argv).map_err(|error| error.to_string())?;
    match args.vm_backend.as_deref() {
        // The driver omits the backend for libkrun, which this plugin replaces.
        None | Some("libkrun") => Ok(Request::RunVm(Box::new(args))),
        // GPU sandboxes use QEMU; leave them to the stock driver.
        Some("qemu") => Ok(Request::Delegate),
        Some(other) => Err(format!("unknown VM backend: {other}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(parts: &[&str]) -> Vec<OsString> {
        std::iter::once("launcher")
            .chain(parts.iter().copied())
            .map(OsString::from)
            .collect()
    }

    const RUN: &[&str] = &[
        "--internal-run-vm",
        "--vm-root-disk",
        "/c/root.ext4",
        "--vm-overlay-disk",
        "/s/overlay.ext4",
        "--vm-exec",
        "/srv/openshell-vm-sandbox-init.sh",
        "--vm-workdir",
        "/",
        "--vm-console-output",
        "/s/rootfs-console.log",
        "--vm-vcpus",
        "4",
        "--vm-mem-mib",
        "4096",
        "--vm-vsock-runtime-initiated",
        "--vm-vsock-control-port",
        "5500",
        "--vm-vsock-control-socket",
        "/tmp/os-1/sb/control.sock",
        "--vm-krun-log-level",
        "1",
        "--vm-env",
        "A=1",
        "--vm-env",
        "B=2",
    ];

    #[test]
    fn the_drivers_libkrun_command_line_is_a_firecracker_run() {
        let Request::RunVm(args) = classify(&argv(RUN)).unwrap() else {
            panic!("expected a VM run");
        };
        assert_eq!(args.vm_vcpus, 4);
        assert_eq!(args.vm_mem_mib, 4096);
        assert_eq!(args.vm_env, ["A=1", "B=2"]);
        assert_eq!(args.vm_vsock_control_port, Some(5500));
        assert!(args.vm_vsock_runtime_initiated);
        assert_eq!(
            args.vm_vsock_control_socket,
            Some(PathBuf::from("/tmp/os-1/sb/control.sock"))
        );
    }

    #[test]
    fn qemu_and_the_image_worker_go_back_to_the_stock_driver() {
        let mut qemu = RUN.to_vec();
        qemu.extend(["--vm-backend", "qemu", "--vm-gpu-bdf", "0000:01:00.0"]);
        assert_eq!(classify(&argv(&qemu)).unwrap(), Request::Delegate);
        assert_eq!(
            classify(&argv(&["--internal-prepare-image", "/req.json"])).unwrap(),
            Request::Delegate
        );
    }

    #[test]
    fn check_is_the_operator_entry_point() {
        assert_eq!(classify(&argv(&["check"])).unwrap(), Request::Check);
    }

    #[test]
    fn running_it_by_hand_explains_itself() {
        assert!(classify(&argv(&[])).unwrap_err().contains("VM launcher"));
    }

    #[test]
    fn unknown_backends_and_flags_are_errors() {
        let mut bad = RUN.to_vec();
        bad.extend(["--vm-backend", "xen"]);
        assert!(classify(&argv(&bad)).is_err());
        let mut unknown = RUN.to_vec();
        unknown.push("--vm-surprise");
        assert!(classify(&argv(&unknown)).is_err());
    }
}
