// SPDX-License-Identifier: Apache-2.0

//! Firecracker launcher plugin for the OpenShell VM compute driver.
//!
//! The stock `openshell-driver-vm` starts every sandbox VM by running its
//! *launcher* with `--internal-run-vm` and a fixed set of flags. This crate is
//! a drop-in launcher that boots the guest under Firecracker instead of
//! libkrun, so the installed driver, host supervisor, guest init and images are
//! used unchanged.
//!
//! - [`launcher`] handles one VM: writes the guest environment into the overlay
//!   disk, boots Firecracker in a rootless jail ([`confine`]), and bridges its
//!   vsock socket to the host supervisor ([`bridge`]).
//! - [`cli`] parses the driver's launcher flags.
//! - [`check`] is the operator's pre-flight (`openshell-firecracker-launcher check`).
//! - Requests the plugin does not handle (the image-preparation worker, QEMU for
//!   GPU sandboxes) are handed back to the stock driver binary ([`delegate`]).

pub mod bridge;
pub mod check;
pub mod cli;
pub mod config;
pub mod confine;
pub mod console;
pub mod delegate;
pub mod envfile;
pub mod launcher;
pub mod signals;
pub mod subids;
