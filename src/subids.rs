// SPDX-License-Identifier: Apache-2.0

//! Subordinate uids for Firecracker VMMs.
//!
//! With ranges for the driver's user in `/etc/subuid` and `/etc/subgid` and the
//! setuid `newuidmap` and `newgidmap` helpers (package `uidmap`), each VMM's
//! user namespace maps two ids: 0, the driver's own user, which builds the jail,
//! and 1, a subordinate uid no other VMM uses, which Firecracker then runs as.
//! A VMM that escapes the jail holds a uid that owns nothing on the host and
//! cannot signal the driver or another VMM.
//!
//! Every VM has its own launcher process and the driver cannot hand one
//! an id, so launchers coordinate through lock files: a launcher holds an
//! exclusive `flock` on `lease-<n>` for as long as it runs. The kernel drops the
//! lock when the launcher exits for any reason, so a crash cannot leak an id.

use std::fs::{File, OpenOptions};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;
use std::time::Duration;

use rustix::fs::{FlockOperation, flock};

/// Most VMMs that can hold a subordinate uid at once.
const MAX_LEASES: u32 = 4096;

/// Whether each Firecracker VMM runs as a leased subordinate uid instead of
/// the driver's own user.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum SubuidMode {
    /// Use the driver's own user.
    Off,
    /// Use subordinate uids when the host supports them, and warn when not.
    #[default]
    Auto,
    /// Refuse to launch unless the host supports subordinate uids.
    Required,
}

impl std::str::FromStr for SubuidMode {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "off" => Ok(Self::Off),
            "auto" => Ok(Self::Auto),
            "required" => Ok(Self::Required),
            other => Err(format!(
                "unknown sub-uid mode '{other}'; expected 'off', 'auto' or 'required'"
            )),
        }
    }
}

/// A range of subordinate ids.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Range {
    pub start: u32,
    pub count: u32,
}

/// The first range for `user` (by name or numeric id) in the text of a
/// subuid or subgid file: `name:start:count` lines.
pub fn range_in(text: &str, user: &str, id: u32) -> Option<Range> {
    text.lines().find_map(|line| {
        let mut fields = line.trim().split(':');
        let (who, start, count) = (fields.next()?, fields.next()?, fields.next()?);
        if who != user && who != id.to_string() {
            return None;
        }
        let range = Range {
            start: start.parse().ok()?,
            count: count.parse().ok()?,
        };
        (range.count > 0).then_some(range)
    })
}

/// What the host offers for mapping: the driver user's ranges and helpers.
#[derive(Debug, Clone)]
pub struct SubIds {
    pub uid: u32,
    pub gid: u32,
    pub uids: Range,
    pub gids: Range,
    pub newuidmap: PathBuf,
    pub newgidmap: PathBuf,
}

fn user_name(uid: u32) -> Option<String> {
    let text = std::fs::read_to_string("/etc/passwd").ok()?;
    text.lines().find_map(|line| {
        let fields: Vec<&str> = line.split(':').collect();
        (fields.len() > 2 && fields[2] == uid.to_string()).then(|| fields[0].to_string())
    })
}

fn helper(name: &str) -> Option<PathBuf> {
    ["/usr/bin", "/bin", "/usr/sbin", "/sbin"]
        .iter()
        .map(|dir| Path::new(dir).join(name))
        .find(|path| path.is_file())
}

fn detect() -> Result<SubIds, String> {
    let uid = rustix::process::getuid().as_raw();
    let gid = rustix::process::getgid().as_raw();
    let name = user_name(uid).unwrap_or_else(|| uid.to_string());
    let range = |file: &str, id: u32| {
        std::fs::read_to_string(file)
            .ok()
            .and_then(|text| range_in(&text, &name, id))
            .ok_or_else(|| format!("no range for {name} in {file}"))
    };
    Ok(SubIds {
        uid,
        gid,
        uids: range("/etc/subuid", uid)?,
        gids: range("/etc/subgid", gid)?,
        newuidmap: helper("newuidmap").ok_or("no newuidmap (package uidmap)")?,
        newgidmap: helper("newgidmap").ok_or("no newgidmap (package uidmap)")?,
    })
}

/// The detected ids, once per process, or why there are none.
pub fn host() -> Result<&'static SubIds, &'static str> {
    static HOST: OnceLock<Result<SubIds, String>> = OnceLock::new();
    HOST.get_or_init(detect).as_ref().map_err(String::as_str)
}

/// How many VMMs can each hold an id: as many as both ranges hold, at most
/// [`MAX_LEASES`].
pub fn limit(ids: &SubIds) -> u32 {
    ids.uids.count.min(ids.gids.count).min(MAX_LEASES)
}

/// Check that sub-uid isolation can work on this host: ranges and helpers
/// exist, and the files a VMM running as another uid must read or open are
/// accessible to every user.
pub fn preflight(firecracker: &Path, kernel: &Path) -> Result<(), String> {
    host().map_err(str::to_string)?;
    let mode = |path: &Path| {
        std::fs::metadata(path)
            .map(|meta| meta.permissions().mode())
            .map_err(|error| format!("{}: {error}", path.display()))
    };
    if mode(firecracker)? & 0o005 != 0o005 {
        return Err(format!(
            "{} must be readable and executable by all users",
            firecracker.display()
        ));
    }
    if mode(kernel)? & 0o004 == 0 {
        return Err(format!(
            "{} must be readable by all users",
            kernel.display()
        ));
    }
    if mode(Path::new("/dev/kvm"))? & 0o006 != 0o006 {
        return Err("/dev/kvm must be readable and writable by all users".to_string());
    }
    Ok(())
}

/// A subordinate-id offset held by this process; released when dropped or when
/// the process exits.
#[derive(Debug)]
pub struct Lease {
    pub offset: u32,
    _lock: File,
}

/// Take the lowest free offset below `limit`, recording it in `dir`.
pub fn acquire(dir: &Path, limit: u32) -> Result<Lease, String> {
    use std::os::unix::fs::DirBuilderExt as _;
    match std::fs::DirBuilder::new().mode(0o700).create(dir) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(format!("create {}: {error}", dir.display())),
    }
    // The directory may be under /tmp: never trust one somebody else made.
    {
        use std::os::unix::fs::MetadataExt as _;
        let meta = std::fs::symlink_metadata(dir)
            .map_err(|error| format!("stat {}: {error}", dir.display()))?;
        if !meta.is_dir() || meta.uid() != rustix::process::getuid().as_raw() {
            return Err(format!(
                "{} must be a directory owned by the driver's user",
                dir.display()
            ));
        }
    }
    for offset in 0..limit {
        let path = dir.join(format!("lease-{offset}"));
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .mode(0o600)
            .open(&path)
            .map_err(|error| format!("open {}: {error}", path.display()))?;
        match flock(&file, FlockOperation::NonBlockingLockExclusive) {
            Ok(()) => {
                return Ok(Lease {
                    offset,
                    _lock: file,
                });
            }
            Err(rustix::io::Errno::WOULDBLOCK) => {}
            Err(error) => return Err(format!("lock {}: {error}", path.display())),
        }
    }
    Err(format!("all {limit} subordinate ids are in use"))
}

/// The `newuidmap` and `newgidmap` arguments for the process `pid`, at lease
/// `offset`: namespace id 0 is the driver's user, id 1 the leased id.
pub fn map_args(ids: &SubIds, pid: u32, offset: u32) -> [Vec<String>; 2] {
    let args = |own: u32, sub: u32| [pid, 0, own, 1, 1, sub, 1].map(|v| v.to_string()).to_vec();
    [
        args(ids.uid, ids.uids.start.saturating_add(offset)),
        args(ids.gid, ids.gids.start.saturating_add(offset)),
    ]
}

/// Map the ids into the user namespace of the process `pid`, once it has
/// entered one of its own (the jail shim waits for the maps). Blocking.
pub fn map(ids: &SubIds, pid: u32, offset: u32) -> Result<(), String> {
    let own = std::fs::read_link("/proc/self/ns/user")
        .map_err(|error| format!("read own user namespace: {error}"))?;
    let theirs = format!("/proc/{pid}/ns/user");
    let mut entered = false;
    // Up to ~5 s (1000 x 5 ms) for the shim to enter its user namespace.
    for _ in 0..1000 {
        match std::fs::read_link(&theirs) {
            Ok(ns) if ns != own => {
                entered = true;
                break;
            }
            Ok(_) => {}
            Err(error) => {
                return Err(format!(
                    "the VMM exited before its ids were mapped: {error}"
                ));
            }
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    if !entered {
        return Err("the VMM did not enter its user namespace".to_string());
    }
    let [uid_args, gid_args] = map_args(ids, pid, offset);
    for (tool, args) in [(&ids.newuidmap, uid_args), (&ids.newgidmap, gid_args)] {
        let output = Command::new(tool)
            .args(&args)
            .output()
            .map_err(|error| format!("run {}: {error}", tool.display()))?;
        if !output.status.success() {
            return Err(format!(
                "{} {}: {}",
                tool.display(),
                args.join(" "),
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids() -> SubIds {
        SubIds {
            uid: 1000,
            gid: 1000,
            uids: Range {
                start: 165_536,
                count: 65_536,
            },
            gids: Range {
                start: 200_000,
                count: 10,
            },
            newuidmap: "/usr/bin/newuidmap".into(),
            newgidmap: "/usr/bin/newgidmap".into(),
        }
    }

    #[test]
    fn ranges_are_found_by_name_or_id() {
        let text = "root:100000:65536\nmiles:165536:65536\n1000:300000:10\nempty:1:0\n";
        let range = |start, count| Some(Range { start, count });
        assert_eq!(range_in(text, "miles", 1000), range(165_536, 65_536));
        assert_eq!(range_in(text, "nobody", 1000), range(300_000, 10));
        assert_eq!(range_in(text, "empty", 7), None, "an empty range is none");
        assert_eq!(range_in(text, "nobody", 7), None);
    }

    #[test]
    fn map_arguments_name_the_driver_user_and_the_leased_id() {
        let [uid, gid] = map_args(&ids(), 4242, 3);
        assert_eq!(uid, ["4242", "0", "1000", "1", "1", "165539", "1"]);
        assert_eq!(gid, ["4242", "0", "1000", "1", "1", "200003", "1"]);
    }

    #[test]
    fn the_limit_is_the_smaller_range() {
        assert_eq!(limit(&ids()), 10);
    }

    #[test]
    fn modes_parse() {
        assert_eq!("off".parse::<SubuidMode>(), Ok(SubuidMode::Off));
        assert_eq!("auto".parse::<SubuidMode>(), Ok(SubuidMode::Auto));
        assert_eq!("required".parse::<SubuidMode>(), Ok(SubuidMode::Required));
        assert!("maybe".parse::<SubuidMode>().is_err());
        assert_eq!(SubuidMode::default(), SubuidMode::Auto);
    }

    #[test]
    fn leases_take_the_lowest_free_offset() {
        let dir = tempfile::tempdir().unwrap();
        let lease_dir = dir.path().join("leases");
        let first = acquire(&lease_dir, 2).unwrap();
        let second = acquire(&lease_dir, 2).unwrap();
        assert_eq!((first.offset, second.offset), (0, 1));
        assert!(acquire(&lease_dir, 2).is_err(), "all taken");
    }

    #[test]
    fn a_dropped_lease_is_free_again() {
        let dir = tempfile::tempdir().unwrap();
        let lease_dir = dir.path().join("leases");
        let first = acquire(&lease_dir, 1).unwrap();
        assert!(acquire(&lease_dir, 1).is_err());
        drop(first);
        // Tests run in parallel, and a sibling test may be between `fork` and
        // `exec` holding a copy of the descriptor, which keeps the flock until
        // the exec closes it. That is momentary, so allow for it.
        let mut again = acquire(&lease_dir, 1);
        for _ in 0..200 {
            if again.is_ok() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
            again = acquire(&lease_dir, 1);
        }
        assert_eq!(again.unwrap().offset, 0);
    }
}
