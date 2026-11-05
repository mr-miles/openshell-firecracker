// SPDX-License-Identifier: Apache-2.0

//! Hand a request back to the driver binary.
//!
//! The driver runs its launcher for more than booting VMs: its image-preparation
//! worker goes through the same path. That worker is the driver itself, not a VM,
//! so the launcher runs the driver binary with the same arguments.

use std::ffi::OsString;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::Command;

/// Environment variable naming the driver binary, when it cannot be found from
/// the launcher's parent process.
pub const DRIVER_ENV: &str = "OPENSHELL_FIRECRACKER_DRIVER";

/// The driver: an explicit override, else the executable of the process
/// that started this launcher (the driver itself).
pub fn driver_binary() -> Result<PathBuf, String> {
    if let Some(path) = std::env::var_os(DRIVER_ENV) {
        return Ok(PathBuf::from(path));
    }
    let parent = std::os::unix::process::parent_id();
    let exe = std::fs::read_link(format!("/proc/{parent}/exe")).map_err(|error| {
        format!("cannot find the driver from parent process {parent}: {error}; set {DRIVER_ENV}")
    })?;
    // A driver replaced on disk after it started reads as "<path> (deleted)".
    let text = exe.to_string_lossy();
    Ok(PathBuf::from(
        text.strip_suffix(" (deleted)").unwrap_or(&text).to_string(),
    ))
}

/// Replace this process with the driver running `argv[1..]`. Only
/// returns on failure.
pub fn exec_driver(argv: &[OsString]) -> String {
    let driver = match driver_binary() {
        Ok(driver) => driver,
        Err(error) => return error,
    };
    let error = Command::new(&driver).args(argv.iter().skip(1)).exec();
    format!("exec {}: {error}", driver.display())
}
