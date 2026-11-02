// SPDX-License-Identifier: Apache-2.0

//! Hand a request back to the stock driver binary.
//!
//! The driver runs its launcher for more than booting libkrun VMs: the image
//! preparation worker and QEMU (GPU) launches go through the same path. The
//! plugin does not implement those, so it runs the stock driver binary with the
//! same arguments, which handles them exactly as it would have.

use std::ffi::OsString;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::Command;

/// Environment variable naming the stock driver binary, when it cannot be
/// found from the launcher's parent process.
pub const STOCK_DRIVER_ENV: &str = "OPENSHELL_FIRECRACKER_STOCK_DRIVER";

/// The stock driver: an explicit override, else the executable of the process
/// that started this launcher (the driver itself).
pub fn stock_driver() -> Result<PathBuf, String> {
    if let Some(path) = std::env::var_os(STOCK_DRIVER_ENV) {
        return Ok(PathBuf::from(path));
    }
    let parent = std::os::unix::process::parent_id();
    let exe = std::fs::read_link(format!("/proc/{parent}/exe")).map_err(|error| {
        format!("cannot find the stock driver from parent process {parent}: {error}; set {STOCK_DRIVER_ENV}")
    })?;
    // A driver replaced on disk after it started reads as "<path> (deleted)".
    let text = exe.to_string_lossy();
    Ok(PathBuf::from(
        text.strip_suffix(" (deleted)").unwrap_or(&text).to_string(),
    ))
}

/// Replace this process with the stock driver running `argv[1..]`. Only
/// returns on failure.
pub fn exec_stock(argv: &[OsString]) -> String {
    let driver = match stock_driver() {
        Ok(driver) => driver,
        Err(error) => return error,
    };
    let error = Command::new(&driver).args(argv.iter().skip(1)).exec();
    format!("exec {}: {error}", driver.display())
}
