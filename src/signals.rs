// SPDX-License-Identifier: Apache-2.0

//! Process lifetime: the launcher dies with the driver, and forwards SIGTERM
//! and SIGINT to the jailed VMM it supervises.

#![allow(unsafe_code)]

use std::sync::atomic::{AtomicI32, Ordering};

/// Pid of the VMM shim, or 0 when none is running.
static CHILD_PID: AtomicI32 = AtomicI32::new(0);

/// Ask the kernel to send this process SIGTERM when its parent (the driver)
/// exits, and give up at once if the parent is already gone.
pub fn die_with_driver() -> Result<(), String> {
    let parent = unsafe { libc::getppid() };
    if unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM, 0, 0, 0) } != 0 {
        return Err(format!(
            "arm parent-death signal: {}",
            std::io::Error::last_os_error()
        ));
    }
    if unsafe { libc::getppid() } != parent || parent == 1 {
        return Err("the driver exited before the launcher started".to_string());
    }
    Ok(())
}

/// Forward SIGTERM and SIGINT to `pid` from now on.
pub fn install_signal_forwarding(pid: i32) {
    unsafe {
        libc::signal(
            libc::SIGINT,
            forward_signal as *const () as libc::sighandler_t,
        );
        libc::signal(
            libc::SIGTERM,
            forward_signal as *const () as libc::sighandler_t,
        );
    }
    CHILD_PID.store(pid, Ordering::Relaxed);
}

/// Stop forwarding: the child has been reaped.
pub fn clear_signal_forwarding() {
    CHILD_PID.store(0, Ordering::Relaxed);
}

/// Async-signal-safe: an atomic load and `kill(2)`.
extern "C" fn forward_signal(_signal: libc::c_int) {
    let pid = CHILD_PID.load(Ordering::Relaxed);
    if pid > 0 {
        unsafe {
            libc::kill(pid, libc::SIGTERM);
        }
    }
}

/// Wait for `pid` to exit; returns the raw wait status.
pub fn wait_for_child(pid: i32) -> Result<libc::c_int, String> {
    loop {
        let mut status: libc::c_int = 0;
        let rc = unsafe { libc::waitpid(pid, &raw mut status, 0) };
        if rc >= 0 {
            return Ok(status);
        }
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::EINTR) {
            return Err(format!("waitpid({pid}) failed: {error}"));
        }
    }
}

/// How a wait status reads as a result.
pub fn describe_exit(status: libc::c_int) -> Result<(), String> {
    if libc::WIFEXITED(status) {
        match libc::WEXITSTATUS(status) {
            0 => Ok(()),
            code => Err(format!("Firecracker VMM exited with status {code}")),
        }
    } else if libc::WIFSIGNALED(status) {
        Err(format!(
            "Firecracker VMM killed by signal {}",
            libc::WTERMSIG(status)
        ))
    } else {
        Err(format!(
            "Firecracker VMM exited with unexpected wait status {status}"
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_statuses_read_as_results() {
        assert!(describe_exit(0).is_ok(), "exited 0");
        assert!(describe_exit(3 << 8).unwrap_err().contains("status 3"));
        assert!(
            describe_exit(libc::SIGKILL)
                .unwrap_err()
                .contains("signal 9")
        );
    }
}
