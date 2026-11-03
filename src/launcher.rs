// SPDX-License-Identifier: Apache-2.0

//! Boot one sandbox VM under Firecracker and wait for it to end.
//!
//! This is the stock VM driver's `--internal-run-vm`, for Firecracker. The
//! driver gives it the same flags it gives its libkrun launcher; the Firecracker
//! binary, guest kernel and sub-uid mode come from the environment the driver
//! inherited from its service unit (see [`HostConfig`]).
//!
//! The launcher writes the guest environment into the overlay disk, opens a
//! bridge from Firecracker's vsock socket to the supervisor, and starts the VMM
//! by re-executing this binary as the jail shim (see [`crate::confine`]), so the
//! sandboxed process is the only thing that ever executes Firecracker.

use std::io::Read;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::net::UnixListener;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::bridge;
use crate::cli::LauncherArgs;
use crate::config::{VSOCK_UDS_NAME, VmmConfig, bridge_socket_name};
use crate::confine::{Access, Bind, JailSpec};
use crate::console::pump;
use crate::envfile;
use crate::locate;
use crate::signals::{
    clear_signal_forwarding, describe_exit, die_with_driver, install_signal_forwarding,
    wait_for_child,
};
use crate::subids::{self, Lease, SubIds, SubuidMode};

/// Firecracker executable.
pub const ENV_BIN: &str = "OPENSHELL_VM_FIRECRACKER_BIN";
/// Uncompressed guest kernel.
pub const ENV_KERNEL: &str = "OPENSHELL_VM_FIRECRACKER_KERNEL";
/// `off`, `auto` (default) or `required`.
pub const ENV_SUBUIDS: &str = "OPENSHELL_VM_FIRECRACKER_SUBUIDS";
/// Directory of sub-uid lease files shared by all launchers of one driver.
pub const ENV_LEASE_DIR: &str = "OPENSHELL_VM_FIRECRACKER_LEASE_DIR";

const VMM_CONFIG_FILE: &str = "firecracker.json";

/// Host-level settings, read from the environment the launcher inherits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostConfig {
    pub firecracker: PathBuf,
    pub kernel: PathBuf,
    pub subuids: SubuidMode,
    pub lease_dir: PathBuf,
}

impl HostConfig {
    pub fn from_env() -> Result<Self, String> {
        let subuids = match std::env::var(ENV_SUBUIDS) {
            Ok(value) => value.parse()?,
            Err(_) => SubuidMode::default(),
        };
        let lease_dir = match std::env::var_os(ENV_LEASE_DIR) {
            Some(dir) => PathBuf::from(dir),
            None => match std::env::var_os("XDG_RUNTIME_DIR") {
                Some(runtime) => PathBuf::from(runtime).join("openshell-firecracker-leases"),
                None => PathBuf::from(format!(
                    "/tmp/openshell-firecracker-leases-{}",
                    rustix::process::getuid().as_raw()
                )),
            },
        };
        // The variables are optional: without them, look beside the launcher, on
        // PATH and in the standard directories (see `locate`).
        let beside = locate::exe_dir();
        Ok(Self {
            firecracker: locate::firecracker(beside.as_deref())?.path,
            kernel: locate::kernel(beside.as_deref())?.path,
            subuids,
            lease_dir,
        })
    }
}

/// A leased subordinate id and the host facts needed to map it.
struct SubUid {
    ids: &'static SubIds,
    lease: Lease,
}

/// Decide whether this VMM runs as a leased subordinate uid.
pub fn resolve_subuid(host: &HostConfig) -> Result<Option<(&'static SubIds, Lease)>, String> {
    if host.subuids == SubuidMode::Off {
        return Ok(None);
    }
    let attempt = subids::preflight(&host.firecracker, &host.kernel).and_then(|()| {
        let ids = subids::host().map_err(str::to_string)?;
        let lease = subids::acquire(&host.lease_dir, subids::limit(ids))?;
        Ok((ids, lease))
    });
    match (attempt, host.subuids) {
        (Ok(found), _) => Ok(Some(found)),
        (Err(error), SubuidMode::Required) => Err(format!(
            "sub-uid isolation is required but unavailable: {error}"
        )),
        (Err(error), _) => {
            eprintln!(
                "openshell-firecracker-launcher: sub-uid isolation unavailable ({error}); \
                 the VMM will run as the driver user"
            );
            Ok(None)
        }
    }
}

fn require_file(path: &Path, what: &str) -> Result<(), String> {
    if path.is_file() {
        Ok(())
    } else {
        Err(format!("{what} not found: {}", path.display()))
    }
}

fn required<'a, T>(value: &'a Option<T>, flag: &str) -> Result<&'a T, String> {
    value
        .as_ref()
        .ok_or_else(|| format!("{flag} is required in internal VM mode"))
}

/// A short private directory for the VMM's vsock sockets. It lives under
/// `/tmp` rather than the sandbox state directory because a Unix socket path
/// is limited to about 100 bytes, and removes itself when dropped.
struct SocketDir {
    path: PathBuf,
}

impl SocketDir {
    fn create() -> Result<Self, String> {
        let uid = rustix::process::getuid().as_raw();
        let pid = std::process::id();
        for _ in 0..16 {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |elapsed| elapsed.subsec_nanos());
            let path = PathBuf::from(format!("/tmp/ofc-{uid}-{pid}-{nanos:x}"));
            match std::fs::DirBuilder::new().mode(0o700).create(&path) {
                Ok(()) => return Ok(Self { path }),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(format!("create {}: {error}", path.display())),
            }
        }
        Err("could not create a private socket directory".to_string())
    }
}

impl Drop for SocketDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// Change `path`'s permission bits to `change(current bits)`.
fn set_mode(path: &Path, change: impl Fn(u32) -> u32) -> Result<(), String> {
    let mode = std::fs::metadata(path)
        .map_err(|error| format!("stat {}: {error}", path.display()))?
        .permissions()
        .mode()
        & 0o7777;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(change(mode)))
        .map_err(|error| format!("chmod {}: {error}", path.display()))
}

/// Give a VMM running as a leased subordinate uid, in the driver's group, the
/// access it needs to what it is bound to, and nothing else.
///
/// The VMM reaches these files through bind mounts, never through their host
/// directories, so the sandbox's state directory stays private to the driver.
fn share_with_vmm(
    args: &LauncherArgs,
    root_disk: &Path,
    overlay: &Path,
    state_dir: &Path,
    socket_dir: Option<&Path>,
) -> Result<(), String> {
    set_mode(state_dir, |_| 0o700)?;
    set_mode(overlay, |_| 0o660)?;
    // Disk images may be shared between sandboxes: add group read only.
    set_mode(root_disk, |mode| mode | 0o040)?;
    if let Some(image) = &args.vm_image_disk {
        set_mode(image, |mode| mode | 0o040)?;
    }
    if let Some(dir) = socket_dir {
        set_mode(dir, |_| 0o770)?;
    }
    Ok(())
}

/// Write `content` to `path` with permission bits `mode`, whether or not the
/// file already existed (the open mode applies only on creation).
fn write_with_mode(path: &Path, content: &[u8], mode: u32) -> Result<(), String> {
    use std::io::Write as _;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(mode)
        .open(path)
        .map_err(|error| format!("write {}: {error}", path.display()))?;
    set_mode(path, |_| mode)?;
    file.write_all(content)
        .map_err(|error| format!("write {}: {error}", path.display()))
}

/// Host wall-clock time in Unix seconds, read as late as possible so the guest
/// clock is set close to boot even after a long image preparation.
fn host_epoch_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

/// Firecracker opens `/dev/kvm` read/write.
fn check_kvm_access() -> Result<(), String> {
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/kvm")
        .map(|_| ())
        .map_err(|error| {
            format!(
                "cannot open /dev/kvm read/write: {error}\nKVM access is required to run microVMs."
            )
        })
}

/// Boot the VM described by `args` and wait for it to exit.
pub fn run(args: &LauncherArgs) -> Result<(), String> {
    die_with_driver()?;
    let host = HostConfig::from_env()?;

    let root_disk = required(&args.vm_root_disk, "--vm-root-disk")?;
    let overlay = required(&args.vm_overlay_disk, "--vm-overlay-disk")?;
    let init_path = required(&args.vm_exec, "--vm-exec")?;
    let console_output = required(&args.vm_console_output, "--vm-console-output")?;
    let kernel: &Path = args
        .vm_kernel_image
        .as_deref()
        .unwrap_or(host.kernel.as_path());

    if args.vm_gpu_bdf.is_some() {
        return Err("GPU passthrough is not supported by the Firecracker launcher".to_string());
    }
    if !args.vm_mount.is_empty() {
        return Err("virtiofs mounts are not supported by the Firecracker launcher".to_string());
    }
    if args.vm_vsock_control_port.is_some() != args.vm_vsock_control_socket.is_some() {
        return Err(
            "--vm-vsock-control-port and --vm-vsock-control-socket must be set together"
                .to_string(),
        );
    }
    require_file(&host.firecracker, "firecracker binary")?;
    require_file(kernel, "guest kernel")?;
    require_file(root_disk, "root disk image")?;
    require_file(overlay, "overlay disk image")?;
    if let Some(image) = &args.vm_image_disk {
        require_file(image, "image disk")?;
    }
    check_kvm_access()?;

    let state_dir = console_output
        .parent()
        .ok_or("console output path has no parent directory")?;
    let sub = resolve_subuid(&host)?.map(|(ids, lease)| SubUid { ids, lease });

    // Firecracker has no exec/env injection like libkrun: the guest init
    // sources its environment from the overlay, as with the QEMU backend.
    let env = envfile::render(&args.vm_env, host_epoch_secs())?;
    envfile::write_into_overlay(overlay, &env)?;

    // Guest-initiated vsock connections reach `<uds>_<port>`, where this
    // process listens and relays to the supervisor. Only that directory is
    // exposed to the VMM.
    let mut socket_dir = None;
    let mut vsock_uds = None;
    if let (Some(port), Some(target)) = (args.vm_vsock_control_port, &args.vm_vsock_control_socket)
    {
        if !args.vm_vsock_runtime_initiated {
            return Err("the Firecracker launcher supports only guest-initiated vsock".to_string());
        }
        let dir = SocketDir::create()?;
        let listener = UnixListener::bind(dir.path.join(bridge_socket_name(port)))
            .map_err(|error| format!("bind vsock bridge: {error}"))?;
        set_mode(&dir.path.join(bridge_socket_name(port)), |_| {
            if sub.is_some() { 0o660 } else { 0o600 }
        })?;
        bridge::serve(listener, target.clone());
        vsock_uds = Some(dir.path.join(VSOCK_UDS_NAME));
        socket_dir = Some(dir);
    }
    let socket_path = socket_dir.as_ref().map(|dir| dir.path.as_path());

    let vmm_config = VmmConfig {
        kernel,
        root_disk,
        overlay_disk: overlay,
        image_disk: args.vm_image_disk.as_deref(),
        vcpus: args.vm_vcpus,
        mem_mib: args.vm_mem_mib,
        init_path,
        vsock_uds: vsock_uds.as_deref(),
    }
    .render()?;
    let config_path = state_dir.join(VMM_CONFIG_FILE);
    write_with_mode(
        &config_path,
        &serde_json::to_vec_pretty(&vmm_config).map_err(|e| format!("encode VMM config: {e}"))?,
        // The leased uid reads its config through the driver's group.
        if sub.is_some() { 0o640 } else { 0o600 },
    )?;
    if sub.is_some() {
        share_with_vmm(args, root_disk, overlay, state_dir, socket_path)?;
    }

    let mut binds = vec![
        Bind::new(kernel, Access::ReadOnly),
        Bind::new(root_disk, Access::ReadOnly),
        Bind::new(overlay, Access::ReadWrite),
    ];
    if let Some(image) = &args.vm_image_disk {
        binds.push(Bind::new(image, Access::ReadOnly));
    }
    if let Some(dir) = socket_path {
        binds.push(Bind::new(dir, Access::ReadWrite));
    }
    binds.extend(
        ["/dev/kvm", "/dev/null", "/dev/urandom"].map(|device| Bind::new(device, Access::Device)),
    );
    let jail = JailSpec {
        firecracker: host.firecracker.clone(),
        config: config_path,
        binds,
        sub: sub.is_some(),
    };
    jail.validate()?;

    // `/proc/self/exe` stays valid if the launcher binary is replaced on disk.
    let mut command = Command::new("/proc/self/exe");
    command
        .args(jail.to_args())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // SAFETY: the closure only calls the async-signal-safe `prctl`.
    unsafe {
        command.pre_exec(|| {
            libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL, 0, 0, 0);
            Ok(())
        });
    }
    let mut child = command
        .spawn()
        .map_err(|error| format!("failed to start the Firecracker VMM shim: {error}"))?;
    let pid = child.id();
    install_signal_forwarding(pid.cast_signed());
    if let Some(sub) = &sub
        && let Err(error) = subids::map(sub.ids, pid, sub.lease.offset)
    {
        let _ = child.kill();
        let _ = child.wait();
        clear_signal_forwarding();
        let _ = std::fs::remove_dir(jail.root());
        return Err(format!("map the VMM's subordinate ids: {error}"));
    }

    let mut streams: Vec<Box<dyn Read + Send>> = Vec::new();
    if let Some(stdout) = child.stdout.take() {
        streams.push(Box::new(stdout));
    }
    if let Some(stderr) = child.stderr.take() {
        streams.push(Box::new(stderr));
    }
    let console =
        pump(streams, console_output).map_err(|error| format!("open console log: {error}"))?;

    let status = wait_for_child(pid.cast_signed());
    clear_signal_forwarding();
    console.finish();
    // The jail's mount namespace died with the shim; only the mount point is
    // left behind.
    let _ = std::fs::remove_dir(jail.root());

    describe_exit(status?)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The environment is process-wide, so the variants share one test.
    #[test]
    fn host_config_reads_the_environment() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("firecracker");
        let kernel = dir.path().join("vmlinux");
        std::fs::write(&bin, b"").unwrap();
        std::fs::write(&kernel, b"").unwrap();
        // SAFETY: no other test in this crate reads or writes these variables.
        unsafe {
            std::env::set_var(ENV_BIN, &bin);
            std::env::set_var(ENV_KERNEL, &kernel);
            std::env::set_var(ENV_LEASE_DIR, dir.path().join("leases"));
            std::env::remove_var(ENV_SUBUIDS);
        }
        let host = HostConfig::from_env().unwrap();
        assert_eq!(host.firecracker, bin);
        assert_eq!(host.kernel, kernel);
        assert_eq!(host.subuids, SubuidMode::Auto, "auto by default");
        assert_eq!(host.lease_dir, dir.path().join("leases"));

        unsafe {
            std::env::set_var(ENV_SUBUIDS, "off");
        }
        assert_eq!(HostConfig::from_env().unwrap().subuids, SubuidMode::Off);
        assert!(
            resolve_subuid(&HostConfig::from_env().unwrap())
                .unwrap()
                .is_none()
        );

        unsafe {
            std::env::set_var(ENV_SUBUIDS, "sometimes");
        }
        assert!(HostConfig::from_env().is_err(), "bad mode");

        unsafe {
            std::env::set_var(ENV_SUBUIDS, "off");
            std::env::set_var(ENV_BIN, "relative/firecracker");
        }
        assert!(HostConfig::from_env().is_err(), "relative path");
    }

    #[test]
    fn socket_dirs_are_short_private_and_removed() {
        let path = {
            let dir = SocketDir::create().unwrap();
            let mode = std::fs::metadata(&dir.path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o700);
            assert!(dir.path.as_os_str().len() < 50, "{}", dir.path.display());
            dir.path.clone()
        };
        assert!(!path.exists(), "removed on drop");
    }

    #[test]
    fn required_flags_are_named_in_errors() {
        let none: Option<PathBuf> = None;
        assert!(
            required(&none, "--vm-root-disk")
                .unwrap_err()
                .contains("--vm-root-disk")
        );
        let some = Some(PathBuf::from("/x"));
        assert_eq!(required(&some, "--f").unwrap(), &PathBuf::from("/x"));
    }

    #[test]
    fn the_vsock_endpoint_name_is_stable() {
        // Firecracker appends `_<port>` to this name; the bridge listens there.
        assert_eq!(VSOCK_UDS_NAME, "fc");
        assert_eq!(bridge_socket_name(5500), "fc_5500");
    }
}
