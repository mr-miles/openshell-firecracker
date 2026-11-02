// SPDX-License-Identifier: Apache-2.0

//! Rootless confinement for one Firecracker VMM.
//!
//! The launcher re-executes this binary as `openshell-driver-firecracker
//! __firecracker-vmm ...` for every VM. That process runs before any other
//! thread exists, which `unshare(CLONE_NEWUSER)` requires, and:
//!
//! 1. enters new user, network, mount, IPC, UTS and PID namespaces as their
//!    root, mapping the driver's own uid and gid (and, with `--sub`, one leased
//!    subordinate id). No host privilege is needed. Firecracker becomes PID 1 of
//!    its own PID namespace, so a compromised VMM cannot see or signal the
//!    driver or other VMMs;
//! 2. leaves the network namespace empty, so a compromised VMM has no network
//!    even though the guest has no NIC to begin with;
//! 3. builds a tmpfs root holding only what Firecracker uses (its binary, the
//!    kernel, the disks, its config, its vsock directory, three device nodes)
//!    and `pivot_root`s into it. Files keep their host paths, so the VMM
//!    config needs no translation. Everything else on the host, including the
//!    supervisor's sockets and other sandboxes' state, is absent;
//! 4. drops the whole capability bounding set, sets `no_new_privs`, disables
//!    core dumps, biases the OOM killer towards the VMM, and with `--sub` drops
//!    to the leased uid, before executing Firecracker, which keeps its own
//!    seccomp filter.
//!
//! The VMM dies with this shim and the shim dies with the launcher
//! (`PR_SET_PDEATHSIG`), so Stop and Delete leave no VMM behind.

#![allow(unsafe_code)]

use std::ffi::{CString, OsString};
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Component, Path, PathBuf};
use std::process::Command;

/// First argument that routes `main` to [`shim`].
pub const SHIM_ARG: &str = "__firecracker-vmm";
const PROBE_ARG: &str = "--probe";

/// Directory, next to the VMM config, where the new root is assembled.
const JAIL_ROOT_DIR: &str = ".fcroot";

/// `oom_score_adj` written for the VMM: well above the driver's, so under
/// memory pressure the kernel picks a VM before the gateway's driver.
const VMM_OOM_SCORE_ADJ: &[u8] = b"500";

/// The namespace uid Firecracker runs as with `sub`: the id the launcher mapped
/// to the VMM's leased subordinate uid. Namespace id 0 is the driver's user.
const SUB_UID: libc::uid_t = 1;

/// How a path is exposed inside the jail.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    ReadOnly,
    ReadWrite,
    /// A device node, bound read-write.
    Device,
}

impl Access {
    fn flag(self) -> &'static str {
        match self {
            Self::ReadOnly => "--ro",
            Self::ReadWrite => "--rw",
            Self::Device => "--dev",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bind {
    pub path: PathBuf,
    pub access: Access,
}

impl Bind {
    pub fn new(path: impl Into<PathBuf>, access: Access) -> Self {
        Self {
            path: path.into(),
            access,
        }
    }
}

/// Everything the jailed VMM may see.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JailSpec {
    pub firecracker: PathBuf,
    /// The VMM `--config-file`; its directory also hosts the jail root.
    pub config: PathBuf,
    pub binds: Vec<Bind>,
    /// Run Firecracker as the subordinate uid the launcher maps into this
    /// user namespace ([`crate::subids`]), instead of the driver's user.
    pub sub: bool,
}

impl JailSpec {
    /// Where the tmpfs root is mounted, in the shim's private mount namespace.
    pub fn root(&self) -> PathBuf {
        self.config
            .parent()
            .unwrap_or_else(|| Path::new("/"))
            .join(JAIL_ROOT_DIR)
    }

    fn all_binds(&self) -> Vec<Bind> {
        let mut all = vec![
            Bind::new(&self.firecracker, Access::ReadOnly),
            Bind::new(&self.config, Access::ReadOnly),
        ];
        all.extend(self.binds.iter().cloned());
        all
    }

    pub fn validate(&self) -> Result<(), String> {
        let root = self.root();
        let all = self.all_binds();
        for (index, bind) in all.iter().enumerate() {
            let path = &bind.path;
            if !path.is_absolute()
                || path
                    .components()
                    .any(|c| matches!(c, Component::ParentDir | Component::CurDir))
            {
                return Err(format!(
                    "jail path must be absolute and normalized: {}",
                    path.display()
                ));
            }
            if bind.access == Access::Device && !path.starts_with("/dev/") {
                return Err(format!(
                    "jail device must be under /dev: {}",
                    path.display()
                ));
            }
            if path.starts_with(&root) || root.starts_with(path) {
                return Err(format!(
                    "jail path overlaps the jail root {}: {}",
                    root.display(),
                    path.display()
                ));
            }
            if all[..index].iter().any(|other| other.path == *path) {
                return Err(format!("jail path listed twice: {}", path.display()));
            }
        }
        Ok(())
    }

    pub fn to_args(&self) -> Vec<OsString> {
        let mut args: Vec<OsString> = vec![SHIM_ARG.into()];
        args.push("--fc".into());
        args.push(self.firecracker.clone().into());
        args.push("--config".into());
        args.push(self.config.clone().into());
        if self.sub {
            args.push("--sub".into());
        }
        for bind in &self.binds {
            args.push(bind.access.flag().into());
            args.push(bind.path.clone().into());
        }
        args
    }

    fn parse(args: &[OsString]) -> Result<Self, String> {
        let mut firecracker = None;
        let mut config = None;
        let mut binds = Vec::new();
        let mut sub = false;
        let mut iter = args.iter();
        while let Some(flag) = iter.next() {
            if flag.to_str() == Some("--sub") {
                sub = true;
                continue;
            }
            let value = || format!("{} needs a value", flag.to_string_lossy());
            let path = PathBuf::from(iter.next().ok_or_else(value)?);
            match flag.to_str() {
                Some("--fc") => firecracker = Some(path),
                Some("--config") => config = Some(path),
                Some("--ro") => binds.push(Bind::new(path, Access::ReadOnly)),
                Some("--rw") => binds.push(Bind::new(path, Access::ReadWrite)),
                Some("--dev") => binds.push(Bind::new(path, Access::Device)),
                _ => return Err(format!("unknown argument {}", flag.to_string_lossy())),
            }
        }
        let spec = Self {
            firecracker: firecracker.ok_or("--fc is required")?,
            config: config.ok_or("--config is required")?,
            binds,
            sub,
        };
        spec.validate()?;
        Ok(spec)
    }
}

/// Handle a `__firecracker-vmm` re-exec: `Some(exit code)` when `args` is one,
/// `None` otherwise. `main` must call this before starting any thread.
pub fn shim(args: &[OsString]) -> Option<i32> {
    if args.get(1).and_then(|a| a.to_str()) != Some(SHIM_ARG) {
        return None;
    }
    let rest = &args[2..];
    let result = if rest.first().and_then(|a| a.to_str()) == Some(PROBE_ARG) {
        unshare_namespaces(false).map(|()| 0)
    } else {
        JailSpec::parse(rest).and_then(|spec| run(&spec))
    };
    Some(match result {
        Ok(code) => code,
        Err(error) => {
            eprintln!("openshell firecracker vmm: {error}");
            1
        }
    })
}

/// Whether this host can run a jailed VMM. Run once at driver startup so a host
/// with unprivileged user namespaces disabled, or without usable KVM, fails
/// there rather than on the first sandbox.
pub fn probe(driver: &Path) -> Result<(), String> {
    // Firecracker opens /dev/kvm read/write.
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/kvm")
        .map_err(|error| format!("cannot open /dev/kvm read/write: {error}"))?;
    let output = Command::new(driver)
        .args([SHIM_ARG, PROBE_ARG])
        .output()
        .map_err(|error| format!("run {}: {error}", driver.display()))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).trim().to_string())
    }
}

fn os_error(what: &str) -> String {
    format!("{what}: {}", io::Error::last_os_error())
}

fn cstr(path: &Path) -> Result<CString, String> {
    CString::new(path.as_os_str().as_bytes())
        .map_err(|_| format!("path contains a NUL byte: {}", path.display()))
}

fn mount(
    source: Option<&Path>,
    target: &Path,
    fstype: Option<&str>,
    flags: libc::c_ulong,
    data: Option<&str>,
) -> Result<(), String> {
    let source = source.map(cstr).transpose()?;
    let target_c = cstr(target)?;
    let fstype = fstype.map(|f| CString::new(f).expect("static fstype"));
    let data = data.map(|d| CString::new(d).expect("static mount data"));
    let rc = unsafe {
        libc::mount(
            source.as_ref().map_or(std::ptr::null(), |s| s.as_ptr()),
            target_c.as_ptr(),
            fstype.as_ref().map_or(std::ptr::null(), |s| s.as_ptr()),
            flags,
            data.as_ref()
                .map_or(std::ptr::null(), |s| s.as_ptr().cast::<libc::c_void>()),
        )
    };
    if rc == 0 {
        Ok(())
    } else {
        Err(os_error(&format!("mount {}", target.display())))
    }
}

/// Enter new user, network, mount, IPC, UTS and PID namespaces as their root.
///
/// `CLONE_NEWPID` applies to children, not to this process: the next process
/// this one forks, Firecracker, becomes PID 1 of the new PID namespace.
///
/// With `sub`, the launcher writes the id maps (it maps a subordinate id as
/// well, which only the setuid `newuidmap` helpers may) and this waits for
/// them; otherwise this process maps its own uid and gid.
fn unshare_namespaces(sub: bool) -> Result<(), String> {
    let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
    let flags = libc::CLONE_NEWUSER
        | libc::CLONE_NEWNET
        | libc::CLONE_NEWNS
        | libc::CLONE_NEWIPC
        | libc::CLONE_NEWUTS
        | libc::CLONE_NEWPID;
    if unsafe { libc::unshare(flags) } != 0 {
        return Err(format!(
            "{} (are unprivileged user namespaces disabled? check user.max_user_namespaces \
             and kernel.unprivileged_userns_clone)",
            os_error("unshare(user, net, mount, ipc, uts, pid)")
        ));
    }
    if sub {
        return wait_for_id_maps();
    }
    let write = |file: &str, content: String| {
        std::fs::write(format!("/proc/self/{file}"), content)
            .map_err(|error| format!("write /proc/self/{file}: {error}"))
    };
    write("setgroups", "deny".to_string())?;
    write("uid_map", format!("0 {uid} 1"))?;
    write("gid_map", format!("0 {gid} 1"))
}

/// Wait for the launcher to write this namespace's uid and gid maps.
fn wait_for_id_maps() -> Result<(), String> {
    let filled = |file: &str| {
        std::fs::read_to_string(format!("/proc/self/{file}"))
            .map(|text| !text.trim().is_empty())
            .unwrap_or(false)
    };
    // Up to ~10 s (2000 x 5 ms).
    for _ in 0..2000 {
        if filled("uid_map") && filled("gid_map") {
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    Err("the driver did not map this VMM's ids".to_string())
}

/// The mount flags a bind of `source` must keep: the kernel locks
/// `nosuid`/`nodev`/`noexec`/atime flags inherited from the parent mount
/// namespace, so a remount that drops one fails with `EPERM`.
fn inherited_mount_flags(source: &Path) -> Result<libc::c_ulong, String> {
    let path = cstr(source)?;
    let mut stat = unsafe { std::mem::zeroed::<libc::statvfs>() };
    if unsafe { libc::statvfs(path.as_ptr(), &raw mut stat) } != 0 {
        return Err(os_error(&format!("statvfs {}", source.display())));
    }
    let mut flags = 0;
    for (st, ms) in [
        (libc::ST_NOSUID, libc::MS_NOSUID),
        (libc::ST_NODEV, libc::MS_NODEV),
        (libc::ST_NOEXEC, libc::MS_NOEXEC),
        (libc::ST_NOATIME, libc::MS_NOATIME),
        (libc::ST_NODIRATIME, libc::MS_NODIRATIME),
        (libc::ST_RELATIME, libc::MS_RELATIME),
    ] {
        if stat.f_flag & st != 0 {
            flags |= ms;
        }
    }
    Ok(flags)
}

/// Bind `bind.path` at the same path under `root`.
fn bind_into(root: &Path, bind: &Bind) -> Result<(), String> {
    let source = bind.path.as_path();
    let meta = std::fs::metadata(source)
        .map_err(|error| format!("bind source {}: {error}", source.display()))?;
    let target = root.join(source.strip_prefix("/").unwrap_or(source));
    let create = if meta.is_dir() {
        std::fs::create_dir_all(&target)
    } else {
        target
            .parent()
            .map_or(Ok(()), std::fs::create_dir_all)
            .and_then(|()| std::fs::write(&target, b""))
    };
    create.map_err(|error| format!("create mount point {}: {error}", target.display()))?;

    mount(Some(source), &target, None, libc::MS_BIND, None)?;
    // Remount to harden: regular files and directories never carry setuid
    // programs or device nodes into the VMM, and read-only means read-only.
    let mut flags = libc::MS_BIND | libc::MS_REMOUNT | inherited_mount_flags(source)?;
    if bind.access != Access::Device {
        flags |= libc::MS_NOSUID | libc::MS_NODEV;
    }
    if bind.access == Access::ReadOnly {
        flags |= libc::MS_RDONLY;
    }
    mount(None, &target, None, flags, None)
}

/// Build the minimal root and pivot into it.
fn confine(spec: &JailSpec) -> Result<(), String> {
    // Keep every mount below in this namespace.
    mount(
        None,
        Path::new("/"),
        None,
        libc::MS_REC | libc::MS_PRIVATE,
        None,
    )?;
    let root = spec.root();
    std::fs::create_dir_all(&root)
        .map_err(|error| format!("create jail root {}: {error}", root.display()))?;
    mount(
        Some(Path::new("tmpfs")),
        &root,
        Some("tmpfs"),
        libc::MS_NOSUID | libc::MS_NODEV,
        Some("mode=0755,size=1m"),
    )?;
    for bind in spec.all_binds() {
        bind_into(&root, &bind)?;
    }

    let old = root.join(".old");
    std::fs::create_dir_all(&old).map_err(|error| format!("create {}: {error}", old.display()))?;
    let (root_c, old_c) = (cstr(&root)?, cstr(&old)?);
    if unsafe { libc::syscall(libc::SYS_pivot_root, root_c.as_ptr(), old_c.as_ptr()) } != 0 {
        return Err(os_error("pivot_root"));
    }
    std::env::set_current_dir("/").map_err(|error| format!("chdir /: {error}"))?;
    if unsafe { libc::umount2(c"/.old".as_ptr(), libc::MNT_DETACH) } != 0 {
        return Err(os_error("unmount old root"));
    }
    std::fs::remove_dir("/.old").map_err(|error| format!("remove /.old: {error}"))?;
    // The root holds only mount points: make it read-only.
    mount(
        None,
        Path::new("/"),
        None,
        libc::MS_BIND | libc::MS_REMOUNT | libc::MS_RDONLY | libc::MS_NOSUID | libc::MS_NODEV,
        None,
    )
}

/// Raise this process's `oom_score_adj` (no privilege needed to raise it);
/// Firecracker inherits it. Async-signal-safe: runs between fork and exec.
fn raise_oom_score() {
    unsafe {
        let fd = libc::open(
            c"/proc/self/oom_score_adj".as_ptr(),
            libc::O_WRONLY | libc::O_CLOEXEC,
        );
        if fd >= 0 {
            libc::write(
                fd,
                VMM_OOM_SCORE_ADJ.as_ptr().cast(),
                VMM_OOM_SCORE_ADJ.len(),
            );
            libc::close(fd);
        }
    }
}

/// Runs between fork and exec of Firecracker. Only async-signal-safe calls.
fn harden_before_exec(sub: bool) -> io::Result<()> {
    raise_oom_score();
    unsafe {
        // A VMM crash must not write guest memory to disk.
        let no_core = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        libc::setrlimit(libc::RLIMIT_CORE, &raw const no_core);

        // Without a bounding set, exec as namespace root yields no capabilities.
        for cap in 0..64 {
            if libc::prctl(libc::PR_CAPBSET_DROP, cap, 0, 0, 0) != 0 {
                let error = io::Error::last_os_error();
                if error.raw_os_error() == Some(libc::EINVAL) {
                    break; // past the last capability this kernel defines
                }
                return Err(error);
            }
        }
        libc::prctl(
            libc::PR_CAP_AMBIENT,
            libc::PR_CAP_AMBIENT_CLEAR_ALL,
            0,
            0,
            0,
        );
        if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0 {
            return Err(io::Error::last_os_error());
        }
        if sub {
            // The VMM's own uid, no capabilities, in the driver's group (for
            // the files and sockets the launcher shared with it). Files it
            // creates are not readable by others.
            libc::umask(0o007);
            if libc::setresuid(SUB_UID, SUB_UID, SUB_UID) != 0 {
                return Err(io::Error::last_os_error());
            }
        }
        // Firecracker dies with this shim, even if the shim is killed. Set last:
        // a change of uid clears it.
        libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL, 0, 0, 0);
    }
    Ok(())
}

fn run(spec: &JailSpec) -> Result<i32, String> {
    let parent = unsafe { libc::getppid() };
    unshare_namespaces(spec.sub)?;
    if spec.sub {
        // The jail's directories must be traversable by the leased uid
        // whatever umask the driver runs with.
        unsafe { libc::umask(0o022) };
    }
    confine(spec)?;
    // Entering the user namespace changed credentials, which cleared the
    // parent-death signal the launcher set on this shim: set it again, then
    // make sure the launcher is still there.
    unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL, 0, 0, 0) };
    if unsafe { libc::getppid() } != parent {
        return Ok(1);
    }

    let mut command = Command::new(&spec.firecracker);
    command
        .arg("--no-api")
        .arg("--config-file")
        .arg(&spec.config)
        .args(["--level", "Warn"])
        .stdin(std::process::Stdio::null());
    let sub = spec.sub;
    unsafe { command.pre_exec(move || harden_before_exec(sub)) };
    let status = command
        .spawn()
        .map_err(|error| format!("start {}: {error}", spec.firecracker.display()))?
        .wait()
        .map_err(|error| format!("wait for firecracker: {error}"))?;
    Ok(status
        .code()
        .or_else(|| status.signal().map(|signal| 128 + signal))
        .unwrap_or(1))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> JailSpec {
        JailSpec {
            firecracker: "/usr/bin/firecracker".into(),
            config: "/state/sb-1/firecracker.json".into(),
            binds: vec![
                Bind::new("/k/vmlinux", Access::ReadOnly),
                Bind::new("/state/sb-1/overlay.ext4", Access::ReadWrite),
                Bind::new("/state/sb-1/vmm", Access::ReadWrite),
                Bind::new("/dev/kvm", Access::Device),
            ],
            sub: false,
        }
    }

    #[test]
    fn args_round_trip() {
        let original = spec();
        let args = original.to_args();
        assert_eq!(args[0], SHIM_ARG);
        let parsed = JailSpec::parse(&args[1..]).unwrap();
        assert_eq!(parsed, original);
    }

    #[test]
    fn the_sub_flag_round_trips_and_defaults_off() {
        let off = JailSpec::parse(&spec().to_args()[1..]).unwrap();
        assert!(!off.sub);
        assert!(!spec().to_args().iter().any(|arg| arg == "--sub"));

        let on = JailSpec {
            sub: true,
            ..spec()
        };
        let args = on.to_args();
        assert!(args.iter().any(|arg| arg == "--sub"));
        let parsed = JailSpec::parse(&args[1..]).unwrap();
        assert_eq!(parsed, on);
        assert!(parsed.sub);
    }

    #[test]
    fn the_jail_root_sits_beside_the_config() {
        assert_eq!(spec().root(), Path::new("/state/sb-1/.fcroot"));
    }

    #[test]
    fn firecracker_and_its_config_are_always_read_only() {
        let all = spec().all_binds();
        assert_eq!(all[0], Bind::new("/usr/bin/firecracker", Access::ReadOnly));
        assert_eq!(
            all[1],
            Bind::new("/state/sb-1/firecracker.json", Access::ReadOnly)
        );
    }

    #[test]
    fn validation_rejects_unsafe_paths() {
        let reject = |mutate: fn(&mut JailSpec)| {
            let mut s = spec();
            mutate(&mut s);
            assert!(s.validate().is_err());
        };
        reject(|s| s.binds.push(Bind::new("relative/disk", Access::ReadOnly)));
        reject(|s| {
            s.binds
                .push(Bind::new("/a/../etc/shadow", Access::ReadOnly));
        });
        reject(|s| s.binds.push(Bind::new("/etc/passwd", Access::Device)));
        reject(|s| s.binds.push(Bind::new("/k/vmlinux", Access::ReadOnly)));
        // A bind that contains, or sits inside, the jail root.
        reject(|s| s.binds.push(Bind::new("/state/sb-1", Access::ReadWrite)));
        reject(|s| {
            s.binds
                .push(Bind::new("/state/sb-1/.fcroot/x", Access::ReadOnly));
        });
    }

    #[test]
    fn valid_spec_passes_validation() {
        spec().validate().unwrap();
    }

    #[test]
    fn parse_rejects_malformed_invocations() {
        let os = |parts: &[&str]| parts.iter().map(OsString::from).collect::<Vec<_>>();
        assert!(JailSpec::parse(&os(&["--fc", "/fc"])).is_err(), "no config");
        assert!(JailSpec::parse(&os(&["--config", "/c"])).is_err(), "no fc");
        assert!(JailSpec::parse(&os(&["--fc"])).is_err(), "no value");
        assert!(
            JailSpec::parse(&os(&["--fc", "/fc", "--config", "/c", "--wat", "/x"])).is_err(),
            "unknown flag"
        );
    }

    #[test]
    fn shim_ignores_ordinary_invocations() {
        let args = vec![
            OsString::from("openshell-driver-firecracker"),
            "--bind-socket".into(),
        ];
        assert_eq!(shim(&args), None);
    }
}
