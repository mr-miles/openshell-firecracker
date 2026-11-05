// SPDX-License-Identifier: Apache-2.0

//! Firecracker launcher plugin for the OpenShell VM compute driver.
//!
//! The OpenShell VM driver (vendored in this repository) starts every sandbox VM
//! by running its *launcher* with `--internal-run-vm` and a fixed set of flags.
//! This crate is that launcher: it boots the guest under Firecracker.
//!
//! - [`launcher`] handles one VM: writes the guest environment into the overlay
//!   disk, boots Firecracker in a rootless jail ([`confine`]), and bridges its
//!   vsock socket to the host supervisor ([`bridge`]).
//! - [`cli`] parses the driver's launcher flags.
//! - [`check`] is the operator's pre-flight (`openshell-firecracker-launcher check`).
//! - The driver's image-preparation worker is not a VM, so it is handed back to
//!   the driver binary that started the launcher ([`delegate`]).

pub mod bridge;
pub mod check;
pub mod cli;
pub mod config;
pub mod confine;
pub mod console;
pub mod delegate;
pub mod envfile;
pub mod launcher;
pub mod locate;
pub mod signals;
pub mod subids;
