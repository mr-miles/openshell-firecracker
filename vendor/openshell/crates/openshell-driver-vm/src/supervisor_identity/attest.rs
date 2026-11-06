// SPDX-FileCopyrightText: Copyright (c) 2026 openshell-firecracker contributors
// SPDX-License-Identifier: Apache-2.0

//! Workload attestation for the per-sandbox Workload API socket.
//!
//! The socket sits in the sandbox's private state directory, but being able to
//! reach it is not taken as proof of identity. Every request is checked
//! against the one process the driver started as this sandbox's supervisor,
//! using the kernel's `SO_PEERCRED` credentials of the connection:
//!
//! - the peer runs as the driver's uid;
//! - its pid is the supervisor's pid, and the process's start time is the one
//!   recorded at spawn, so a recycled pid does not inherit the identity;
//! - `/proc/<pid>/exe` names this sandbox's hard link of the supervisor binary
//!   and resolves to that same inode. The hard link shares an inode with the
//!   shared binary, so the path is what tells one sandbox's supervisor from
//!   another's; the inode check catches a link replaced after spawn.
//!
//! The start time is read again after the other checks, so a pid that exits
//! and is reused between them fails too.

use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};

/// The supervisor a sandbox's Workload API socket serves.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ExpectedSupervisor {
    pub pid: i32,
    pub start_time: u64,
    pub uid: u32,
    pub exe: PathBuf,
    pub dev: u64,
    pub ino: u64,
}

impl ExpectedSupervisor {
    /// Record the identity of a just-spawned supervisor.
    ///
    /// `exe` is the path it was started from; its inode is taken from the
    /// running process, not the path, and must match the path's.
    pub(super) fn capture(pid: u32, exe: &Path) -> Result<Self, String> {
        let pid = i32::try_from(pid).map_err(|_| format!("pid {pid} out of range"))?;
        let start_time = process_start_time(pid)?;
        let running = std::fs::metadata(proc_dir(pid).join("exe"))
            .map_err(|error| format!("inspect supervisor {pid} executable: {error}"))?;
        let on_disk = std::fs::metadata(exe)
            .map_err(|error| format!("inspect {}: {error}", exe.display()))?;
        if (running.dev(), running.ino()) != (on_disk.dev(), on_disk.ino()) {
            return Err(format!("supervisor {pid} is not running {}", exe.display()));
        }
        Ok(Self {
            pid,
            start_time,
            uid: rustix::process::geteuid().as_raw(),
            exe: exe.to_path_buf(),
            dev: running.dev(),
            ino: running.ino(),
        })
    }

    /// Check a peer's `SO_PEERCRED` credentials. The error is for the
    /// driver's log; the peer only learns that attestation failed.
    pub(super) fn verify(&self, peer_process: Option<i32>, peer_user: u32) -> Result<(), String> {
        let pid = peer_process.ok_or_else(|| "peer pid unavailable".to_string())?;
        if peer_user != self.uid {
            return Err(format!(
                "peer uid {peer_user} is not the driver's uid {}",
                self.uid
            ));
        }
        if pid != self.pid {
            return Err(format!(
                "peer pid {pid} is not the sandbox supervisor (pid {})",
                self.pid
            ));
        }
        self.check_start_time()?;
        let process = proc_dir(pid);
        let exe = std::fs::read_link(process.join("exe"))
            .map_err(|error| format!("read peer {pid} executable: {error}"))?;
        if exe != self.exe {
            return Err(format!(
                "peer {pid} runs {}, not {}",
                exe.display(),
                self.exe.display()
            ));
        }
        let running = std::fs::metadata(process.join("exe"))
            .map_err(|error| format!("inspect peer {pid} executable: {error}"))?;
        if (running.dev(), running.ino()) != (self.dev, self.ino) {
            return Err(format!(
                "peer {pid} executable inode changed since the supervisor started"
            ));
        }
        // A pid that exited and was reused while the checks above ran would
        // have a different start time now.
        self.check_start_time()
    }

    fn check_start_time(&self) -> Result<(), String> {
        let start_time = process_start_time(self.pid)?;
        if start_time != self.start_time {
            return Err(format!(
                "pid {} was reused: the supervisor has exited",
                self.pid
            ));
        }
        Ok(())
    }
}

fn proc_dir(pid: i32) -> PathBuf {
    Path::new("/proc").join(pid.to_string())
}

/// Field 22 of `/proc/<pid>/stat`: start time in clock ticks after boot.
fn process_start_time(pid: i32) -> Result<u64, String> {
    let stat = std::fs::read_to_string(proc_dir(pid).join("stat"))
        .map_err(|error| format!("read process {pid} status: {error}"))?;
    parse_start_time(&stat).ok_or_else(|| format!("malformed /proc/{pid}/stat"))
}

fn parse_start_time(stat: &str) -> Option<u64> {
    // `comm` (field 2) is parenthesised and may itself contain spaces and
    // parentheses, so count fields from the last `)`. Field 3 is first after it.
    let (_, rest) = stat.rsplit_once(')')?;
    rest.split_ascii_whitespace().nth(22 - 3)?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn self_exe() -> PathBuf {
        std::fs::read_link("/proc/self/exe").unwrap()
    }

    #[test]
    fn parses_start_time_past_awkward_comm() {
        let stat =
            "123 (a) b) c) S 1 123 123 0 -1 4194560 100 0 0 0 1 2 0 0 20 0 1 0 98765 1000 10";
        assert_eq!(parse_start_time(stat), Some(98765));
        assert_eq!(parse_start_time("123 (x) S 1"), None);
    }

    #[test]
    fn accepts_the_captured_process() {
        let expected = ExpectedSupervisor::capture(std::process::id(), &self_exe()).unwrap();
        let uid = rustix::process::geteuid().as_raw();
        let pid = i32::try_from(std::process::id()).unwrap();
        expected.verify(Some(pid), uid).unwrap();
    }

    #[test]
    fn rejects_other_pid_uid_and_missing_pid() {
        let expected = ExpectedSupervisor::capture(std::process::id(), &self_exe()).unwrap();
        let uid = rustix::process::geteuid().as_raw();
        let pid = i32::try_from(std::process::id()).unwrap();
        assert!(expected.verify(Some(1), uid).is_err());
        assert!(expected.verify(Some(pid), uid.wrapping_add(1)).is_err());
        assert!(expected.verify(None, uid).is_err());
    }

    #[test]
    fn rejects_recycled_pid() {
        let mut expected = ExpectedSupervisor::capture(std::process::id(), &self_exe()).unwrap();
        expected.start_time += 1;
        let error = expected
            .verify(
                Some(i32::try_from(std::process::id()).unwrap()),
                rustix::process::geteuid().as_raw(),
            )
            .unwrap_err();
        assert!(error.contains("reused"), "{error}");
    }

    #[test]
    fn rejects_a_different_link_of_the_same_binary() {
        // Two sandboxes' links share an inode; only the path differs.
        let mut expected = ExpectedSupervisor::capture(std::process::id(), &self_exe()).unwrap();
        expected.exe = PathBuf::from("/elsewhere/openshell-supervisor");
        let error = expected
            .verify(
                Some(i32::try_from(std::process::id()).unwrap()),
                rustix::process::geteuid().as_raw(),
            )
            .unwrap_err();
        assert!(error.contains("not /elsewhere"), "{error}");
    }

    #[test]
    fn rejects_changed_inode() {
        let mut expected = ExpectedSupervisor::capture(std::process::id(), &self_exe()).unwrap();
        expected.ino += 1;
        assert!(
            expected
                .verify(
                    Some(i32::try_from(std::process::id()).unwrap()),
                    rustix::process::geteuid().as_raw(),
                )
                .is_err()
        );
    }

    #[test]
    fn capture_refuses_a_process_not_running_the_path() {
        let dir = tempfile::tempdir().unwrap();
        let other = dir.path().join("other");
        std::fs::write(&other, b"not the test binary").unwrap();
        assert!(ExpectedSupervisor::capture(std::process::id(), &other).is_err());
    }
}
